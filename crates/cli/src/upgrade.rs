use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
pub(crate) use reqwest::header::AUTHORIZATION;
use reqwest::{
    StatusCode,
    blocking::{Client, Response},
    header::{HeaderMap, HeaderValue, USER_AGENT},
};
use serde::{Deserialize, Serialize};

use super::*;

const DARC_LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/0xjunha/darc/releases/latest";
const DARC_INSTALLER_COMMAND: &str =
    "curl -fsSL https://github.com/0xjunha/darc/releases/latest/download/darc-installer.sh | sh";
const UPGRADE_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const UPGRADE_NUDGE_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const UPGRADE_NUDGE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const UPGRADE_NUDGE_NOTIFY_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const UPGRADE_ERROR_BODY_LIMIT: usize = 240;
/// Stores metadata returned by the GitHub latest-release endpoint.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GitHubLatestRelease {
    pub(crate) tag_name: String,
    pub(crate) html_url: String,
}

/// Stores the resolved Darc upgrade state.
#[derive(Debug, Clone)]
pub(crate) struct UpgradeStatus {
    pub(crate) current_version: String,
    pub(crate) latest_version: Option<String>,
    pub(crate) upgrade_available: bool,
    pub(crate) latest_release_url: Option<String>,
}

/// Stores the best-effort cached state for passive upgrade nudges.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct UpgradeNudgeCache {
    pub(crate) checked_at_unix: Option<u64>,
    pub(crate) last_notified_at_unix: Option<u64>,
    pub(crate) latest_version: Option<String>,
    pub(crate) latest_release_url: Option<String>,
    pub(crate) dismissed_version: Option<String>,
    pub(crate) upgrade_available: bool,
}

/// Stores the machine-readable payload for `darc upgrade --check --json`.
#[derive(Debug, Serialize)]
pub(crate) struct UpgradeCheckJson<'a> {
    pub(crate) current_version: &'a str,
    pub(crate) latest_version: Option<&'a str>,
    pub(crate) upgrade_available: bool,
    pub(crate) latest_release_url: Option<&'a str>,
    pub(crate) install_command: String,
}

/// Selects whether one upgrade check can attach ambient GitHub credentials.
#[derive(Clone, Copy)]
pub(crate) enum UpgradeCheckAuth {
    Anonymous,
    IncludeGitHubToken,
}

impl<'a> From<&'a UpgradeStatus> for UpgradeCheckJson<'a> {
    /// Builds one JSON payload from a resolved upgrade status.
    fn from(status: &'a UpgradeStatus) -> Self {
        Self {
            current_version: &status.current_version,
            latest_version: status.latest_version.as_deref(),
            upgrade_available: status.upgrade_available,
            latest_release_url: status.latest_release_url.as_deref(),
            install_command: manual_upgrade_installer_command(),
        }
    }
}

/// Stores one parsed semantic-ish release version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedReleaseVersion {
    pub(crate) major: u64,
    pub(crate) minor: u64,
    pub(crate) patch: u64,
    pub(crate) pre: Option<String>,
}

impl ParsedReleaseVersion {
    /// Parses one Darc release version or `v`-prefixed tag.
    fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        let value = value.strip_prefix('v').unwrap_or(value);
        let value = value.split_once('+').map_or(value, |(version, _)| version);
        let (core, pre) = value
            .split_once('-')
            .map_or((value, None), |(core, pre)| (core, Some(pre.to_owned())));
        let mut parts = core.split('.');
        let major = parse_version_component(parts.next(), value, "major")?;
        let minor = parse_version_component(parts.next(), value, "minor")?;
        let patch = parse_version_component(parts.next(), value, "patch")?;
        if parts.next().is_some() {
            bail!("invalid Darc release version `{value}`");
        }
        Ok(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// Compares two parsed release versions using the SemVer precedence shape Darc needs.
    fn cmp_semver(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(_), None) => std::cmp::Ordering::Less,
                (Some(left), Some(right)) => compare_prerelease(left, right),
            })
    }
}

/// Parses one numeric SemVer core component.
fn parse_version_component(component: Option<&str>, full: &str, name: &str) -> Result<u64> {
    let component = component.ok_or_else(|| anyhow!("invalid Darc release version `{full}`"))?;
    component
        .parse::<u64>()
        .with_context(|| format!("invalid {name} component in Darc release version `{full}`"))
}

/// Compares two SemVer prerelease identifier lists.
fn compare_prerelease(left: &str, right: &str) -> std::cmp::Ordering {
    let mut left_parts = left.split('.');
    let mut right_parts = right.split('.');
    loop {
        match (left_parts.next(), right_parts.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(left), Some(right)) => {
                let ordering = compare_prerelease_identifier(left, right);
                if !ordering.is_eq() {
                    return ordering;
                }
            }
        }
    }
}

/// Compares two SemVer prerelease identifiers.
fn compare_prerelease_identifier(left: &str, right: &str) -> std::cmp::Ordering {
    match (is_numeric_identifier(left), is_numeric_identifier(right)) {
        (true, true) => compare_numeric_identifier(left, right),
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (false, false) => left.cmp(right),
    }
}

/// Returns whether one prerelease identifier is numeric.
fn is_numeric_identifier(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Compares two numeric prerelease identifiers without risking integer overflow.
fn compare_numeric_identifier(left: &str, right: &str) -> std::cmp::Ordering {
    let left = trim_numeric_identifier(left);
    let right = trim_numeric_identifier(right);
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

/// Trims insignificant leading zeroes from one numeric identifier.
fn trim_numeric_identifier(value: &str) -> &str {
    let trimmed = value.trim_start_matches('0');
    if trimmed.is_empty() { "0" } else { trimmed }
}

/// Runs the explicit Darc CLI upgrade command.
pub(crate) fn run_upgrade(args: UpgradeArgs) -> Result<()> {
    if let Some(command) = args.command {
        if args.check {
            bail!("`darc upgrade dismiss` cannot be combined with `--check`");
        }
        return match command {
            UpgradeCommands::Dismiss(dismiss_args) => run_upgrade_dismiss(&args.root, dismiss_args),
        };
    }

    let status = check_darc_upgrade(UPGRADE_CHECK_TIMEOUT, UpgradeCheckAuth::IncludeGitHubToken)?;
    if args.json {
        return print_upgrade_check_json(&status);
    }

    if args.check {
        print_upgrade_check_report(&status);
        return Ok(());
    }

    run_darc_upgrade(status)
}

/// Dismisses one cached Darc upgrade nudge.
pub(crate) fn run_upgrade_dismiss(root: &Path, args: UpgradeDismissArgs) -> Result<()> {
    let mut cache = read_upgrade_nudge_cache(root);
    let version = match args.version {
        Some(version) => display_release_version(&version),
        None => cache
            .latest_version
            .clone()
            .ok_or_else(|| anyhow!("no cached Darc upgrade version is available to dismiss"))?,
    };
    cache.dismissed_version = Some(version.clone());
    write_upgrade_nudge_cache(root, &cache)?;

    let style = HumanStyle::stdout();
    print_section(style, "Upgrade");
    print_field(style, 2, "Dismissed", version);
    Ok(())
}

/// Prints one machine-readable upgrade check envelope.
fn print_upgrade_check_json(status: &UpgradeStatus) -> Result<()> {
    println!(
        "{}",
        render_json_envelope("darc.upgrade.check.v1", &UpgradeCheckJson::from(status))?
    );
    Ok(())
}

/// Prints one human-readable upgrade check report.
fn print_upgrade_check_report(status: &UpgradeStatus) {
    let style = HumanStyle::stdout();
    print_section(style, "Upgrade");
    print_field(style, 2, "Current", &status.current_version);
    print_field(
        style,
        2,
        "Latest",
        status
            .latest_version
            .as_deref()
            .unwrap_or("not published or not accessible"),
    );
    print_field(
        style,
        2,
        "Status",
        if status.upgrade_available {
            style.warn("upgrade available")
        } else if status.latest_version.is_none() {
            style.muted("not published or not accessible")
        } else {
            style.ok("current")
        },
    );
    if status.upgrade_available {
        print_line(2, "Run `darc upgrade` to upgrade this installation.");
    }
}

/// Applies one Darc CLI upgrade when the installed updater is available.
fn run_darc_upgrade(status: UpgradeStatus) -> Result<()> {
    if !status.upgrade_available {
        print_upgrade_check_report(&status);
        return Ok(());
    }

    let Some(updater_path) = find_darc_updater() else {
        let style = HumanStyle::stdout();
        print_section(style, "Upgrade");
        print_field(style, 2, "Current", &status.current_version);
        print_field(
            style,
            2,
            "Latest",
            status
                .latest_version
                .as_deref()
                .unwrap_or("not published or not accessible"),
        );
        print_field(style, 2, "Status", style.warn("manual upgrade required"));
        print_line(2, "This installation does not include `darc-update`.");
        print_line(2, format!("Run: {}", manual_upgrade_installer_command()));
        bail!("`darc-update` was not found; rerun the release installer to upgrade");
    };

    let style = HumanStyle::stdout();
    print_section(style, "Upgrade");
    print_field(style, 2, "Current", &status.current_version);
    print_field(
        style,
        2,
        "Latest",
        status
            .latest_version
            .as_deref()
            .unwrap_or("not published or not accessible"),
    );
    print_field(style, 2, "Updater", style.path(updater_path.display()));
    println!();

    let result = Command::new(&updater_path)
        .status()
        .with_context(|| format!("failed to run updater {}", updater_path.display()))?;
    if result.success() {
        return Ok(());
    }
    bail!("updater exited with status {result}")
}

/// Checks GitHub Releases for the latest Darc CLI release.
fn check_darc_upgrade(timeout: Duration, auth: UpgradeCheckAuth) -> Result<UpgradeStatus> {
    let current_version = env!("CARGO_PKG_VERSION").to_owned();
    let Some(release) = fetch_latest_darc_release(timeout, auth)? else {
        return Ok(UpgradeStatus {
            current_version,
            latest_version: None,
            upgrade_available: false,
            latest_release_url: None,
        });
    };
    let latest_version = display_release_version(&release.tag_name);
    let upgrade_available = release_version_is_newer(&latest_version, &current_version)?;
    Ok(UpgradeStatus {
        current_version,
        latest_version: Some(latest_version),
        upgrade_available,
        latest_release_url: Some(release.html_url),
    })
}

/// Fetches metadata for the latest Darc GitHub Release.
fn fetch_latest_darc_release(
    timeout: Duration,
    auth: UpgradeCheckAuth,
) -> Result<Option<GitHubLatestRelease>> {
    let client = build_upgrade_http_client(timeout, auth)?;
    let Some(response) = send_upgrade_request(
        client
            .get(DARC_LATEST_RELEASE_API_URL)
            .header("Accept", "application/vnd.github+json"),
        "fetch latest Darc release metadata",
    )?
    else {
        return Ok(None);
    };
    let bytes = response
        .bytes()
        .context("failed to read latest Darc release response body")?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .context("failed to parse latest Darc release response JSON")
}

/// Builds one short-lived HTTP client for upgrade checks.
fn build_upgrade_http_client(timeout: Duration, auth: UpgradeCheckAuth) -> Result<Client> {
    let token = github_api_token();
    let headers = build_upgrade_headers(auth, token.as_deref())?;
    Client::builder()
        .default_headers(headers)
        .timeout(timeout)
        .build()
        .context("failed to build HTTP client for Darc upgrade check")
}

/// Builds the default HTTP headers for one upgrade check request.
pub(crate) fn build_upgrade_headers(
    auth: UpgradeCheckAuth,
    token: Option<&str>,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&format!("darc/{}", env!("CARGO_PKG_VERSION")))
            .context("failed to build GitHub API user agent header")?,
    );
    if matches!(auth, UpgradeCheckAuth::IncludeGitHubToken)
        && let Some(token) = token
    {
        let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
            .context("failed to build GitHub API authorization header")?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }
    Ok(headers)
}

/// Returns the configured GitHub API token when one is available.
fn github_api_token() -> Option<String> {
    [env::var("GH_TOKEN"), env::var("GITHUB_TOKEN")]
        .into_iter()
        .find_map(|value| value.ok().filter(|value| !value.trim().is_empty()))
}

/// Sends one upgrade-check HTTP request and returns a successful response.
fn send_upgrade_request(
    request: reqwest::blocking::RequestBuilder,
    context_message: &str,
) -> Result<Option<Response>> {
    let response = request
        .send()
        .with_context(|| format!("failed to {context_message}"))?;
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if status.is_success() {
        return Ok(Some(response));
    }
    let body = response.text().unwrap_or_default();
    let Some(detail) = upgrade_http_error_detail(&body) else {
        bail!("failed to {context_message}: GitHub returned HTTP {status}");
    };
    bail!("failed to {context_message}: GitHub returned HTTP {status}: {detail}")
}

/// Returns compact remote error detail for an upgrade-check HTTP failure.
pub(crate) fn upgrade_http_error_detail(body: &str) -> Option<String> {
    let detail = collapse_whitespace(body);
    if detail.is_empty() {
        return None;
    }
    Some(truncate_text(&detail, UPGRADE_ERROR_BODY_LIMIT))
}

/// Collapses arbitrary text into one whitespace-normalized line.
fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncates one string by character count and appends an ellipsis marker.
fn truncate_text(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        return text.to_owned();
    }
    let truncated = text
        .chars()
        .take(max_len.saturating_sub(3))
        .collect::<String>();
    format!("{truncated}...")
}

/// Returns the user-visible version label for one release tag.
fn display_release_version(tag_name: &str) -> String {
    tag_name
        .trim()
        .strip_prefix('v')
        .unwrap_or_else(|| tag_name.trim())
        .to_owned()
}

/// Returns whether the latest release version is newer than the current version.
pub(crate) fn release_version_is_newer(latest: &str, current: &str) -> Result<bool> {
    let latest = ParsedReleaseVersion::parse(latest)?;
    let current = ParsedReleaseVersion::parse(current)?;
    Ok(latest.cmp_semver(&current).is_gt())
}

/// Finds the cargo-dist updater installed alongside the current Darc executable.
fn find_darc_updater() -> Option<PathBuf> {
    current_exe_sibling_updater()
}

/// Returns the updater next to the current executable when it exists.
fn current_exe_sibling_updater() -> Option<PathBuf> {
    current_exe_dir().and_then(|dir| upgrade_executable_at(&dir.join(upgrade_executable_name())))
}

/// Returns one updater path when the candidate exists as a file.
fn upgrade_executable_at(path: &Path) -> Option<PathBuf> {
    path.is_file().then(|| path.to_path_buf())
}

/// Returns the cargo-dist updater executable name for this platform.
fn upgrade_executable_name() -> String {
    format!("darc-update{}", env::consts::EXE_SUFFIX)
}

/// Returns the installer fallback command for the current Darc executable.
pub(crate) fn manual_upgrade_installer_command() -> String {
    current_exe_dir()
        .map(|dir| manual_upgrade_installer_command_for_dir(&dir))
        .unwrap_or_else(|| DARC_INSTALLER_COMMAND.to_owned())
}

/// Returns the installer fallback command for one target install directory.
pub(crate) fn manual_upgrade_installer_command_for_dir(dir: &Path) -> String {
    format!(
        "curl -fsSL https://github.com/0xjunha/darc/releases/latest/download/darc-installer.sh | DARC_INSTALL_DIR={} sh",
        shell_quote(&dir.display().to_string())
    )
}

/// Returns the directory that contains the current executable.
fn current_exe_dir() -> Option<PathBuf> {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

/// Returns one POSIX-shell-safe single-quoted string.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Stores one passive startup upgrade nudge decision for the current command.
pub(crate) struct UpgradeNudgeContext {
    pub(crate) root: PathBuf,
    pub(crate) cache: UpgradeNudgeCache,
}

impl UpgradeNudgeContext {
    /// Starts one cache-first passive upgrade nudge for an eligible command.
    pub(crate) fn start(command: &Commands) -> Option<Self> {
        let root = upgrade_nudge_root(command)?.to_path_buf();
        if !upgrade_nudge_enabled_from_env() || !upgrade_nudge_enabled_from_config(&root) {
            return None;
        }
        let mut cache = read_upgrade_nudge_cache(&root);
        if let Some(now) = current_unix_seconds()
            && should_notify_upgrade_nudge(now, &cache, env!("CARGO_PKG_VERSION"))
            && let Some(latest_version) = cache.latest_version.as_deref()
        {
            let style = HumanStyle::stderr();
            eprintln!(
                "{}",
                style.warn(format!(
                    "Darc {latest_version} is available. Run `darc upgrade`."
                ))
            );
            cache.last_notified_at_unix = Some(now);
            let _ = write_upgrade_nudge_cache(&root, &cache);
        }
        Some(Self { root, cache })
    }

    /// Refreshes the passive upgrade cache after the primary command succeeds.
    pub(crate) fn refresh_after_command(mut self, exit_code: i32) {
        if exit_code != 0 {
            return;
        }
        let Some(now) = current_unix_seconds() else {
            return;
        };
        if !should_check_upgrade_nudge(now, &self.cache) {
            return;
        }

        self.cache.checked_at_unix = Some(now);
        if let Ok(status) = check_darc_upgrade(UPGRADE_NUDGE_TIMEOUT, UpgradeCheckAuth::Anonymous) {
            self.cache.latest_version = status.latest_version;
            self.cache.latest_release_url = status.latest_release_url;
            self.cache.upgrade_available = status.upgrade_available;
        }
        let _ = write_upgrade_nudge_cache(&self.root, &self.cache);
    }
}

/// Returns the Darc root that can be used for one startup upgrade nudge.
pub(crate) fn upgrade_nudge_root(command: &Commands) -> Option<&Path> {
    match command {
        Commands::Refresh(args) if !args.watch => Some(&args.root),
        Commands::Sync(args) if !args.dry_run => Some(&args.root),
        Commands::Index(args) => Some(&args.root),
        Commands::Service(args) => match args.command {
            ServiceCommands::Start
            | ServiceCommands::Stop
            | ServiceCommands::Restart
            | ServiceCommands::Enable
            | ServiceCommands::Disable => Some(&args.root),
            ServiceCommands::Status => None,
        },
        Commands::Project(args) => match &args.command {
            ProjectCommands::Link(args) if !args.dry_run => Some(&args.root),
            ProjectCommands::Remove(args) if !args.dry_run => Some(&args.root),
            ProjectCommands::RenameFrom(args) if !args.dry_run => Some(&args.root),
            _ => None,
        },
        Commands::Link(args) if !args.dry_run => Some(&args.root),
        Commands::Remove(args) if !args.dry_run => Some(&args.root),
        Commands::RenameFrom(args) if !args.dry_run => Some(&args.root),
        _ => None,
    }
}

/// Returns whether the current process should try passive upgrade nudges.
pub(crate) fn upgrade_nudge_enabled_from_env() -> bool {
    upgrade_nudge_enabled(
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
        env::var("TERM").ok().as_deref(),
        env::var_os("CI").is_some(),
        env::var_os("DARC_NO_UPDATE_CHECK").is_some(),
    )
}

/// Returns whether the shared config allows passive startup upgrade checks.
pub(crate) fn upgrade_nudge_enabled_from_config(root: &Path) -> bool {
    load_config(&root.join("config.toml"))
        .map(|config| config.check_for_update_on_startup)
        .unwrap_or(false)
}

/// Returns whether passive upgrade nudges are enabled for resolved process facts.
pub(crate) fn upgrade_nudge_enabled(
    stdout_is_terminal: bool,
    stderr_is_terminal: bool,
    term: Option<&str>,
    ci: bool,
    disabled: bool,
) -> bool {
    stdout_is_terminal && stderr_is_terminal && term != Some("dumb") && !ci && !disabled
}

/// Returns whether the cache is old enough for another network upgrade check.
pub(crate) fn should_check_upgrade_nudge(now: u64, cache: &UpgradeNudgeCache) -> bool {
    cache.checked_at_unix.is_none_or(|checked_at| {
        now.saturating_sub(checked_at) >= UPGRADE_NUDGE_CHECK_INTERVAL.as_secs()
    })
}

/// Returns whether a cached available upgrade should be shown again.
pub(crate) fn should_notify_upgrade_nudge(
    now: u64,
    cache: &UpgradeNudgeCache,
    current_version: &str,
) -> bool {
    cache
        .latest_version
        .as_deref()
        .is_some_and(|latest_version| {
            cache.dismissed_version.as_deref() != Some(latest_version)
                && release_version_is_newer(latest_version, current_version).unwrap_or(false)
        })
        && cache.last_notified_at_unix.is_none_or(|notified_at| {
            now.saturating_sub(notified_at) >= UPGRADE_NUDGE_NOTIFY_INTERVAL.as_secs()
        })
}

/// Reads one passive upgrade nudge cache, treating missing or invalid JSON as empty.
fn read_upgrade_nudge_cache(root: &Path) -> UpgradeNudgeCache {
    fs::read_to_string(upgrade_nudge_cache_path(root))
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

/// Writes one passive upgrade nudge cache under the Darc runtime directory.
fn write_upgrade_nudge_cache(root: &Path, cache: &UpgradeNudgeCache) -> Result<()> {
    let path = upgrade_nudge_cache_path(root);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("upgrade nudge cache path has no parent"))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let content =
        serde_json::to_vec_pretty(cache).context("failed to serialize upgrade nudge cache")?;
    fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))
}

/// Returns the passive upgrade nudge cache path under one Darc root.
fn upgrade_nudge_cache_path(root: &Path) -> PathBuf {
    root.join("run/upgrade-check.json")
}

/// Returns the current Unix timestamp in seconds.
fn current_unix_seconds() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}
