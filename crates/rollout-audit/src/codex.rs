use std::{
    env,
    fmt::Write as _,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use darc_rollout::codex::{CodexCliVersion, latest_exact_supported_codex_cli_version};
use directories::BaseDirs;
use flate2::read::GzDecoder;
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tar::Archive;

use crate::schema_diff::{normalize_json, summarize_schema_differences, truncate_text};

/// Stores the GitHub Releases page size used for Codex release discovery.
const GITHUB_RELEASES_PAGE_SIZE: usize = 100;
/// Stores the GitHub Releases API URL for Codex release discovery.
const GITHUB_RELEASES_URL: &str = "https://api.github.com/repos/openai/codex/releases";
/// Stores the human-readable source label for Codex GitHub releases.
const GITHUB_RELEASE_SOURCE: &str = "GitHub Releases (openai/codex)";
/// Stores the stable tag prefix expected on released Codex binaries.
const RELEASE_TAG_PREFIX: &str = "rust-v";
/// Stores the rollout schema file name exported from released Codex binaries.
const ROLLOUT_SCHEMA_FILE_NAME: &str = "RolloutLine.json";
/// Stores the timeout used for released Codex schema export commands.
const CODEX_SCHEMA_EXPORT_TIMEOUT: Duration = Duration::from_secs(180);
/// Lists the Darc files most likely to need updates after Codex schema drift.
const LIKELY_UPDATE_PATHS: &[&str] = &[
    "crates/rollout/src/codex/version.rs",
    "crates/rollout/src/codex/header.rs",
    "crates/rollout/src/codex/mod.rs",
    "crates/rollout-audit/src/codex.rs",
    "crates/cli/src/lib.rs",
];

/// Stores the input options for a Codex rollout schema compatibility audit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexSchemaAuditOptions {
    pub cache_dir: Option<PathBuf>,
}

/// Stores the structured result of one Codex rollout schema compatibility audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexSchemaAuditReport {
    pub release_source: String,
    pub binary_cache_dir: PathBuf,
    pub latest_stable_release_tag: String,
    pub latest_exact_covered_version: String,
    pub audited_tags: Vec<String>,
    pub outcome: CodexSchemaAuditOutcome,
}

impl CodexSchemaAuditReport {
    /// Returns whether the audited stable Codex tags are schema-compatible with darc.
    pub fn is_compatible(&self) -> bool {
        matches!(self.outcome, CodexSchemaAuditOutcome::Compatible)
    }

    /// Formats the audited stable release tag range for user-facing summaries.
    pub fn audited_tag_range(&self) -> String {
        match (self.audited_tags.first(), self.audited_tags.last()) {
            (Some(first), Some(last)) if first == last => first.clone(),
            (Some(first), Some(last)) => format!("{first} ..= {last}"),
            _ => "<empty>".to_owned(),
        }
    }
}

/// Stores whether the audited stable Codex tags stayed compatible or drifted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CodexSchemaAuditOutcome {
    Compatible,
    Drift(CodexSchemaDrift),
}

/// Stores the first detected rollout schema drift against darc's exact coverage baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexSchemaDrift {
    pub first_drift_tag: String,
    pub difference_summary: Vec<String>,
    pub likely_files_to_update: Vec<String>,
}

/// Runs the hook-ready Codex rollout schema compatibility audit.
pub fn run_codex_schema_audit(options: CodexSchemaAuditOptions) -> Result<CodexSchemaAuditReport> {
    let mut noop = |_: &str| {};
    run_codex_schema_audit_with_progress(options, &mut noop)
}

/// Runs the hook-ready Codex rollout schema compatibility audit with progress updates.
pub fn run_codex_schema_audit_with_progress<F>(
    options: CodexSchemaAuditOptions,
    mut report_progress: F,
) -> Result<CodexSchemaAuditReport>
where
    F: FnMut(&str),
{
    report_progress("Resolving schema audit cache directory...");
    let cache_dir = resolve_binary_cache_dir(options.cache_dir.as_deref())?;
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("failed to create {}", cache_dir.display()))?;
    report_progress(&format!("Using audit cache: {}", cache_dir.display()));

    report_progress("Detecting host platform for released Codex binaries...");
    let host_platform = HostPlatform::detect()?;
    report_progress(&format!(
        "Using released Codex binaries for {}.",
        host_platform.display_name()
    ));

    match github_api_auth_source() {
        Some(source) => report_progress(&format!(
            "Using authenticated GitHub API requests from {source}."
        )),
        None => report_progress(
            "Using unauthenticated GitHub API requests; GitHub rate limits may apply.",
        ),
    }

    report_progress("Fetching Codex release metadata from GitHub Releases...");
    let release_catalog = GitHubReleaseCatalog::fetch(&mut report_progress)?;
    report_progress(&format!(
        "Fetched {} Codex release entry(ies) from GitHub Releases.",
        release_catalog.releases.len()
    ));

    let provider =
        GitHubCodexSchemaAuditProvider::new(release_catalog, cache_dir.clone(), host_platform)?;
    run_codex_schema_audit_with_provider_and_progress(
        GITHUB_RELEASE_SOURCE.to_owned(),
        cache_dir,
        &provider,
        &mut report_progress,
    )
}

/// Lists released Codex versions and exports one normalized RolloutLine schema per version.
trait CodexSchemaAuditProvider {
    /// Returns the raw release tag names available to the audit.
    fn list_release_tag_names(&self) -> Result<Vec<String>>;

    /// Exports the RolloutLine JSON schema for one released Codex version.
    fn export_rollout_line_schema<F>(
        &self,
        tag_name: &str,
        report_progress: &mut F,
    ) -> Result<Value>
    where
        F: FnMut(&str);
}

/// Stores one parsed stable Codex release tag with its sortable version.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StableCodexReleaseTag {
    raw_tag: String,
    version: CodexCliVersion,
}

/// Stores one fetched GitHub release catalog for Codex.
struct GitHubReleaseCatalog {
    releases: Vec<GitHubRelease>,
}

impl GitHubReleaseCatalog {
    /// Fetches the current Codex GitHub releases catalog.
    fn fetch<F>(report_progress: &mut F) -> Result<Self>
    where
        F: FnMut(&str),
    {
        let client = build_http_client()?;
        let mut releases = Vec::new();
        let mut page = 1usize;
        let exact_boundary_tag = format!(
            "{RELEASE_TAG_PREFIX}{}",
            latest_exact_supported_codex_cli_version()
        );

        loop {
            report_progress(&format!(
                "Fetching GitHub release page {page} from openai/codex..."
            ));
            let page_releases = fetch_github_release_page(&client, page)?;
            let fetched = page_releases.len();
            if fetched == 0 {
                break;
            }
            releases.extend(page_releases);
            if releases.iter().any(|release| {
                release.tag_name == exact_boundary_tag && !release.draft && !release.prerelease
            }) {
                report_progress(&format!(
                    "Found exact support boundary release {exact_boundary_tag}; stopping metadata fetch."
                ));
                break;
            }
            if fetched < GITHUB_RELEASES_PAGE_SIZE {
                break;
            }
            page += 1;
        }

        ensure!(
            !releases.is_empty(),
            "GitHub Releases returned no Codex releases"
        );
        Ok(Self { releases })
    }

    /// Returns one release entry for a specific tag name.
    fn release_by_tag(&self, tag_name: &str) -> Option<&GitHubRelease> {
        self.releases
            .iter()
            .find(|release| release.tag_name == tag_name)
    }
}

/// Stores the release metadata needed from one GitHub release row.
#[derive(Debug, Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GitHubReleaseAsset>,
}

/// Stores the release asset metadata needed to download one published binary package.
#[derive(Debug, Clone, Deserialize)]
struct GitHubReleaseAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

/// Binds released binary downloads to the audit provider trait.
struct GitHubCodexSchemaAuditProvider {
    release_catalog: GitHubReleaseCatalog,
    cache_dir: PathBuf,
    host_platform: HostPlatform,
    http: Client,
    scratch_dir: ScopedTempDir,
}

impl GitHubCodexSchemaAuditProvider {
    /// Creates one provider backed by the fetched GitHub Releases catalog.
    fn new(
        release_catalog: GitHubReleaseCatalog,
        cache_dir: PathBuf,
        host_platform: HostPlatform,
    ) -> Result<Self> {
        Ok(Self {
            release_catalog,
            cache_dir,
            host_platform,
            http: build_http_client()?,
            scratch_dir: ScopedTempDir::new("darc-codex-schema-audit")?,
        })
    }

    /// Resolves the released asset published for one stable Codex tag on this host.
    fn release_asset_for_tag(&self, tag_name: &str) -> Result<&GitHubReleaseAsset> {
        let release = self
            .release_catalog
            .release_by_tag(tag_name)
            .with_context(|| format!("missing GitHub release metadata for `{tag_name}`"))?;
        let version_text = tag_name.strip_prefix(RELEASE_TAG_PREFIX).with_context(|| {
            format!("release tag `{tag_name}` did not start with `{RELEASE_TAG_PREFIX}`")
        })?;
        let asset_name = self.host_platform.release_asset_name(version_text);
        release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .with_context(|| {
                format!(
                    "GitHub release `{tag_name}` is missing asset `{asset_name}` for {}",
                    self.host_platform.display_name()
                )
            })
    }

    /// Ensures one released Codex binary package is cached locally and returns its executable path.
    fn ensure_cached_binary<F>(&self, tag_name: &str, report_progress: &mut F) -> Result<PathBuf>
    where
        F: FnMut(&str),
    {
        let asset = self.release_asset_for_tag(tag_name)?;
        let digest = parse_sha256_digest(asset.digest.as_deref(), &asset.name)?;
        let cached_archive_path = self.cached_archive_path(&asset.name, &digest);

        if cached_archive_path.is_file() {
            report_progress(&format!(
                "Verifying cached released binary package for {tag_name}..."
            ));
            if let Err(error) = verify_file_sha256(&cached_archive_path, &digest) {
                report_progress(&format!(
                    "Cached package for {tag_name} failed integrity verification; refreshing cache."
                ));
                let cache_root = self.cache_root_for_digest(&digest);
                if cache_root.exists() {
                    fs::remove_dir_all(&cache_root)
                        .with_context(|| format!("failed to remove {}", cache_root.display()))?;
                }
                report_progress(&format!(
                    "Discarded invalid cache for {tag_name}: {error:#}"
                ));
            }
        }

        if !cached_archive_path.is_file() {
            report_progress(&format!(
                "Downloading released binary asset `{}`...",
                asset.name
            ));
            let archive_path = self.scratch_dir.path().join(format!(
                "download-{}-{}",
                sanitize_for_path(&asset.name),
                unique_suffix()
            ));
            download_to_path(
                &self.http,
                &asset.browser_download_url,
                &archive_path,
                &format!("download released Codex asset `{}`", asset.name),
            )?;

            report_progress(&format!("Verifying SHA-256 digest for `{}`...", asset.name));
            verify_file_sha256(&archive_path, &digest)?;

            report_progress(&format!(
                "Caching verified released binary package for {tag_name}..."
            ));
            stage_cached_archive_package(&archive_path, &cached_archive_path)?;
        }

        let extraction_root = self.scratch_dir.path().join(format!(
            "package-{}-{}",
            sanitize_for_path(tag_name),
            unique_suffix()
        ));
        report_progress(&format!(
            "Extracting verified released binary package for {tag_name}..."
        ));
        extract_verified_binary_package(&cached_archive_path, &digest, &extraction_root)?;
        let executable_path = extraction_root.join(self.host_platform.package_binary_path());
        ensure!(
            executable_path.is_file(),
            "extracted package for `{tag_name}` did not contain {}",
            executable_path.display()
        );

        #[cfg(unix)]
        mark_executable(&executable_path)?;

        Ok(executable_path)
    }

    /// Returns the cache directory root for one released Codex tag on this host.
    fn cache_root_for_digest(&self, digest: &str) -> PathBuf {
        self.cache_dir
            .join(self.host_platform.release_asset_suffix())
            .join(digest)
    }

    /// Returns the cached verified archive path for one released asset digest.
    fn cached_archive_path(&self, asset_name: &str, digest: &str) -> PathBuf {
        self.cache_root_for_digest(digest).join(asset_name)
    }
}

impl CodexSchemaAuditProvider for GitHubCodexSchemaAuditProvider {
    fn list_release_tag_names(&self) -> Result<Vec<String>> {
        Ok(self
            .release_catalog
            .releases
            .iter()
            .filter(|release| !release.draft && !release.prerelease)
            .map(|release| release.tag_name.clone())
            .collect())
    }

    fn export_rollout_line_schema<F>(
        &self,
        tag_name: &str,
        report_progress: &mut F,
    ) -> Result<Value>
    where
        F: FnMut(&str),
    {
        let binary_path = self.ensure_cached_binary(tag_name, report_progress)?;
        let schema_dir = self.scratch_dir.path().join(format!(
            "schema-{}-{}",
            sanitize_for_path(tag_name),
            unique_suffix()
        ));
        fs::create_dir_all(&schema_dir)
            .with_context(|| format!("failed to create {}", schema_dir.display()))?;

        report_progress(&format!(
            "Running released Codex binary to export RolloutLine schema for {tag_name}..."
        ));
        let working_dir = binary_path
            .parent()
            .context("released Codex binary unexpectedly had no parent directory")?;
        let runtime_root = self.scratch_dir.path().join(format!(
            "runtime-{}-{}",
            sanitize_for_path(tag_name),
            unique_suffix()
        ));
        let mut command = build_released_binary_command(&binary_path, working_dir, &runtime_root)?;
        command
            .arg("app-server")
            .arg("generate-internal-json-schema")
            .arg("-o")
            .arg(&schema_dir);
        let output = run_command_with_timeout(&mut command, CODEX_SCHEMA_EXPORT_TIMEOUT)
            .with_context(|| {
                format!(
                    "failed to run released Codex binary for `{tag_name}` at {}",
                    binary_path.display()
                )
            })?;
        if !output.status.success() {
            bail!(
                "failed to export RolloutLine schema for `{tag_name}`: {}",
                command_output_summary(&output.stderr)
            );
        }

        let schema_path = schema_dir.join(ROLLOUT_SCHEMA_FILE_NAME);
        let schema_bytes = fs::read(&schema_path).with_context(|| {
            format!(
                "failed to read exported RolloutLine schema for `{tag_name}` at {}",
                schema_path.display()
            )
        })?;
        serde_json::from_slice(&schema_bytes).with_context(|| {
            format!(
                "failed to parse exported RolloutLine schema for `{tag_name}` at {}",
                schema_path.display()
            )
        })
    }
}

/// Runs one process with a hard timeout and returns its captured output.
fn run_command_with_timeout(command: &mut Command, timeout: Duration) -> Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn command")?;
    let stdout_reader = spawn_output_reader(
        child
            .stdout
            .take()
            .context("failed to capture child stdout")?,
    );
    let stderr_reader = spawn_output_reader(
        child
            .stderr
            .take()
            .context("failed to capture child stderr")?,
    );
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().context("failed to poll child process")? {
            return collect_command_output(status, stdout_reader, stderr_reader);
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let status = child
                .wait()
                .context("failed to wait for timed out child process")?;
            let output = collect_command_output(status, stdout_reader, stderr_reader)?;
            bail!(
                "command timed out after {}s: {}",
                timeout.as_secs(),
                command_output_summary(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Spawns one background reader that drains one child output pipe into memory.
fn spawn_output_reader(
    mut reader: impl Read + Send + 'static,
) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

/// Collects the completed child status plus drained stdout/stderr bytes into one `Output`.
fn collect_command_output(
    status: std::process::ExitStatus,
    stdout_reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    stderr_reader: thread::JoinHandle<io::Result<Vec<u8>>>,
) -> Result<Output> {
    Ok(Output {
        status,
        stdout: join_output_reader(stdout_reader, "stdout")?,
        stderr: join_output_reader(stderr_reader, "stderr")?,
    })
}

/// Joins one background pipe reader and returns the captured bytes.
fn join_output_reader(
    handle: thread::JoinHandle<io::Result<Vec<u8>>>,
    label: &str,
) -> Result<Vec<u8>> {
    match handle.join() {
        Ok(result) => result.with_context(|| format!("failed to read child {label}")),
        Err(_panic) => anyhow::bail!("child {label} reader panicked"),
    }
}

/// Builds one released Codex command with a scrubbed runtime environment.
fn build_released_binary_command(
    binary_path: &Path,
    working_dir: &Path,
    runtime_root: &Path,
) -> Result<Command> {
    let runtime_home = runtime_root.join("home");
    let runtime_tmp = runtime_root.join("tmp");
    let xdg_config = runtime_home.join(".config");
    let xdg_cache = runtime_home.join(".cache");
    let xdg_data = runtime_home.join(".local").join("share");
    let xdg_state = runtime_home.join(".local").join("state");
    let xdg_runtime = runtime_root.join("run");

    for path in [
        runtime_root,
        &runtime_home,
        &runtime_tmp,
        &xdg_config,
        &xdg_cache,
        &xdg_data,
        &xdg_state,
        &xdg_runtime,
    ] {
        fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    }

    let mut command = Command::new(binary_path);
    command.current_dir(working_dir);
    command.env_clear();
    command.env("HOME", &runtime_home);
    command.env("TMPDIR", &runtime_tmp);
    command.env("TMP", &runtime_tmp);
    command.env("TEMP", &runtime_tmp);
    command.env("XDG_CONFIG_HOME", &xdg_config);
    command.env("XDG_CACHE_HOME", &xdg_cache);
    command.env("XDG_DATA_HOME", &xdg_data);
    command.env("XDG_STATE_HOME", &xdg_state);
    command.env("XDG_RUNTIME_DIR", &xdg_runtime);

    Ok(command)
}

/// Stores one supported host platform for released Codex binaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HostPlatform {
    release_asset_suffix: &'static str,
    vendor_target: &'static str,
    binary_file_name: &'static str,
    display_name: &'static str,
}

impl HostPlatform {
    /// Detects the current host platform for released Codex binary packages.
    fn detect() -> Result<Self> {
        host_platform_from_parts(env::consts::OS, env::consts::ARCH)
    }

    /// Returns the GitHub release asset suffix used by the npm package archive.
    fn release_asset_suffix(self) -> &'static str {
        self.release_asset_suffix
    }

    /// Returns the release asset name for one stable Codex version.
    fn release_asset_name(self, version_text: &str) -> String {
        format!("codex-npm-{}-{version_text}.tgz", self.release_asset_suffix)
    }

    /// Returns the packaged executable path inside one extracted npm archive.
    fn package_binary_path(self) -> PathBuf {
        PathBuf::from("package")
            .join("vendor")
            .join(self.vendor_target)
            .join("codex")
            .join(self.binary_file_name)
    }

    /// Returns the human-readable name of this released binary platform.
    fn display_name(self) -> &'static str {
        self.display_name
    }
}

/// Resolves one supported host platform from OS and architecture strings.
fn host_platform_from_parts(os: &str, arch: &str) -> Result<HostPlatform> {
    match (os, arch) {
        ("macos", "aarch64") => Ok(HostPlatform {
            release_asset_suffix: "darwin-arm64",
            vendor_target: "aarch64-apple-darwin",
            binary_file_name: "codex",
            display_name: "macOS arm64",
        }),
        ("macos", "x86_64") => Ok(HostPlatform {
            release_asset_suffix: "darwin-x64",
            vendor_target: "x86_64-apple-darwin",
            binary_file_name: "codex",
            display_name: "macOS x86_64",
        }),
        ("linux", "aarch64") => Ok(HostPlatform {
            release_asset_suffix: "linux-arm64",
            vendor_target: "aarch64-unknown-linux-musl",
            binary_file_name: "codex",
            display_name: "Linux arm64",
        }),
        ("linux", "x86_64") => Ok(HostPlatform {
            release_asset_suffix: "linux-x64",
            vendor_target: "x86_64-unknown-linux-musl",
            binary_file_name: "codex",
            display_name: "Linux x86_64",
        }),
        _ => bail!("unsupported host platform `{os}` / `{arch}` for released Codex binaries"),
    }
}

/// Builds one HTTP client for GitHub metadata and binary downloads.
fn build_http_client() -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&format!("darc/{}", env!("CARGO_PKG_VERSION")))
            .context("failed to build GitHub API user agent header")?,
    );
    if let Some(token) = github_api_token() {
        let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
            .context("failed to build GitHub API authorization header")?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }

    Client::builder()
        .default_headers(headers)
        .build()
        .context("failed to build HTTP client for Codex schema audit")
}

/// Returns the GitHub API auth source name when one is configured.
fn github_api_auth_source() -> Option<&'static str> {
    [
        ("GH_TOKEN", env::var("GH_TOKEN")),
        ("GITHUB_TOKEN", env::var("GITHUB_TOKEN")),
    ]
    .into_iter()
    .find_map(|(name, value)| {
        value
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|_| name)
    })
}

/// Returns the configured GitHub API token when one is available.
fn github_api_token() -> Option<String> {
    [env::var("GH_TOKEN"), env::var("GITHUB_TOKEN")]
        .into_iter()
        .find_map(|value| value.ok().filter(|value| !value.trim().is_empty()))
}

/// Fetches one page of Codex GitHub releases.
fn fetch_github_release_page(client: &Client, page: usize) -> Result<Vec<GitHubRelease>> {
    let url = format!("{GITHUB_RELEASES_URL}?per_page={GITHUB_RELEASES_PAGE_SIZE}&page={page}");
    let response = send_checked_request(
        client
            .get(&url)
            .header("Accept", "application/vnd.github+json"),
        &format!("fetch Codex GitHub releases page {page}"),
    )?;
    let bytes = response
        .bytes()
        .context("failed to read GitHub releases response body")?;
    serde_json::from_slice(&bytes).context("failed to parse GitHub releases response JSON")
}

/// Sends one HTTP request and returns a successful response or a compact error.
fn send_checked_request(
    request: reqwest::blocking::RequestBuilder,
    context_message: &str,
) -> Result<Response> {
    let response = request
        .send()
        .with_context(|| format!("failed to {context_message}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response.text().unwrap_or_default();
    bail!(
        "failed to {context_message}: HTTP {} {}",
        status.as_u16(),
        truncate_text(body.trim(), 240)
    )
}

/// Resolves the default binary cache directory for released Codex packages.
fn resolve_binary_cache_dir(cache_dir: Option<&Path>) -> Result<PathBuf> {
    if let Some(cache_dir) = cache_dir {
        return Ok(cache_dir.to_path_buf());
    }

    BaseDirs::new()
        .map(|dirs| {
            dirs.cache_dir()
                .join("darc")
                .join("schema-audit")
                .join("codex")
        })
        .context("unable to resolve the user cache directory")
}

/// Downloads one HTTP resource into a local file path.
fn download_to_path(
    client: &Client,
    url: &str,
    destination: &Path,
    context_message: &str,
) -> Result<()> {
    let mut response = send_checked_request(client.get(url), context_message)?;
    let mut file = File::create(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    io::copy(&mut response, &mut file)
        .with_context(|| format!("failed to write {}", destination.display()))?;
    Ok(())
}

/// Parses the expected SHA-256 digest from one release asset metadata row.
fn parse_sha256_digest(digest: Option<&str>, asset_name: &str) -> Result<String> {
    let digest = digest.with_context(|| {
        format!("GitHub release asset `{asset_name}` did not include a SHA-256 digest")
    })?;
    let digest = digest.strip_prefix("sha256:").with_context(|| {
        format!("GitHub release asset `{asset_name}` returned unsupported digest `{digest}`")
    })?;
    Ok(digest.to_owned())
}

/// Verifies one file against an expected SHA-256 hex digest.
fn verify_file_sha256(path: &Path, expected_digest: &str) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let mut actual_digest = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(&mut actual_digest, "{byte:02x}").expect("writing to a string should never fail");
    }
    ensure!(
        actual_digest == expected_digest,
        "SHA-256 mismatch for {}: expected {expected_digest}, got {actual_digest}",
        path.display()
    );
    Ok(())
}

/// Copies one verified package archive into the persistent cache atomically.
fn stage_cached_archive_package(archive_path: &Path, cached_archive_path: &Path) -> Result<()> {
    let cache_root = cached_archive_path
        .parent()
        .context("cached archive unexpectedly had no parent directory")?;
    let parent_dir = cache_root
        .parent()
        .context("cached binary root unexpectedly had no parent directory")?;
    fs::create_dir_all(parent_dir)
        .with_context(|| format!("failed to create {}", parent_dir.display()))?;

    let staged_root = temporary_sibling_path(cache_root, "staging");
    if staged_root.exists() {
        fs::remove_dir_all(&staged_root)
            .with_context(|| format!("failed to remove {}", staged_root.display()))?;
    }
    fs::create_dir_all(&staged_root)
        .with_context(|| format!("failed to create {}", staged_root.display()))?;
    let staged_archive_path = staged_root.join(
        cached_archive_path
            .file_name()
            .context("cached archive unexpectedly had no file name")?,
    );
    fs::copy(archive_path, &staged_archive_path).with_context(|| {
        format!(
            "failed to copy verified archive from {} to {}",
            archive_path.display(),
            staged_archive_path.display()
        )
    })?;

    if cache_root.exists() {
        fs::remove_dir_all(cache_root)
            .with_context(|| format!("failed to remove {}", cache_root.display()))?;
    }
    fs::rename(&staged_root, cache_root).with_context(|| {
        format!(
            "failed to move extracted package from {} to {}",
            staged_root.display(),
            cache_root.display()
        )
    })?;
    Ok(())
}

/// Extracts one verified npm package archive into a destination directory.
fn extract_verified_binary_package(
    archive_path: &Path,
    expected_digest: &str,
    destination_root: &Path,
) -> Result<()> {
    verify_file_sha256(archive_path, expected_digest)?;
    extract_binary_package(archive_path, destination_root)
}

/// Extracts one npm package archive into a destination directory.
fn extract_binary_package(archive_path: &Path, destination_root: &Path) -> Result<()> {
    if destination_root.exists() {
        fs::remove_dir_all(destination_root)
            .with_context(|| format!("failed to remove {}", destination_root.display()))?;
    }
    fs::create_dir_all(destination_root)
        .with_context(|| format!("failed to create {}", destination_root.display()))?;

    let archive_file = File::open(archive_path)
        .with_context(|| format!("failed to open {}", archive_path.display()))?;
    let decoder = GzDecoder::new(archive_file);
    let mut archive = Archive::new(decoder);
    archive
        .unpack(destination_root)
        .with_context(|| format!("failed to unpack {}", archive_path.display()))
}

/// Creates one temporary sibling path next to a final destination path.
fn temporary_sibling_path(path: &Path, label: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("path");
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{file_name}-{label}-{}", unique_suffix()))
}

/// Marks one extracted binary executable on Unix hosts.
#[cfg(unix)]
fn mark_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to mark {} executable", path.display()))
}

/// Keeps one temporary scratch directory cleaned up when the audit ends.
struct ScopedTempDir {
    path: PathBuf,
}

impl ScopedTempDir {
    /// Creates one unique temporary directory for audit scratch data.
    fn new(prefix: &str) -> Result<Self> {
        let path = env::temp_dir().join(format!("{prefix}-{}", unique_suffix()));
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        Ok(Self { path })
    }

    /// Returns the filesystem path of this temporary directory.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScopedTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Executes the audit against an abstracted release catalog and schema exporter.
#[cfg(test)]
fn run_codex_schema_audit_with_provider<P: CodexSchemaAuditProvider>(
    release_source: String,
    binary_cache_dir: PathBuf,
    provider: &P,
) -> Result<CodexSchemaAuditReport> {
    let mut noop = |_: &str| {};
    run_codex_schema_audit_with_provider_and_progress(
        release_source,
        binary_cache_dir,
        provider,
        &mut noop,
    )
}

/// Executes the audit against an abstracted release catalog and schema exporter with progress.
fn run_codex_schema_audit_with_provider_and_progress<P, F>(
    release_source: String,
    binary_cache_dir: PathBuf,
    provider: &P,
    report_progress: &mut F,
) -> Result<CodexSchemaAuditReport>
where
    P: CodexSchemaAuditProvider,
    F: FnMut(&str),
{
    report_progress("Listing stable Codex release tags...");
    let stable_tags = collect_stable_release_tags(provider.list_release_tag_names()?);
    report_progress(&format!(
        "Found {} stable Codex release tag(s).",
        stable_tags.len()
    ));
    let audited_tags = select_audited_release_tags(&stable_tags)?;
    let latest_tag = audited_tags
        .first()
        .context("selected audit range is unexpectedly empty")?;
    let baseline_tag = audited_tags
        .last()
        .context("selected audit range is unexpectedly empty")?;
    report_progress(&format!(
        "Auditing {} stable tag(s) from {} down to {}.",
        audited_tags.len(),
        latest_tag.raw_tag,
        baseline_tag.raw_tag
    ));
    report_progress(&format!(
        "Exporting baseline RolloutLine schema from {}...",
        baseline_tag.raw_tag
    ));
    let baseline_schema = normalize_json(
        provider.export_rollout_line_schema(&baseline_tag.raw_tag, report_progress)?,
    );
    let total_comparisons = audited_tags.len().saturating_sub(1);
    let mut drift = None;
    for (index, tag) in audited_tags.iter().rev().skip(1).enumerate() {
        report_progress(&format!(
            "Comparing {} against baseline ({}/{})...",
            tag.raw_tag,
            index + 1,
            total_comparisons
        ));
        let schema =
            normalize_json(provider.export_rollout_line_schema(&tag.raw_tag, report_progress)?);
        if schema == baseline_schema {
            continue;
        }
        report_progress(&format!("Detected schema drift at {}.", tag.raw_tag));
        drift = Some(CodexSchemaDrift {
            first_drift_tag: tag.raw_tag.clone(),
            difference_summary: summarize_schema_differences(&baseline_schema, &schema),
            likely_files_to_update: LIKELY_UPDATE_PATHS
                .iter()
                .map(|path| (*path).to_owned())
                .collect(),
        });
        break;
    }
    if drift.is_none() {
        report_progress(&format!(
            "No schema drift detected across {} audited stable tag(s).",
            audited_tags.len()
        ));
    }

    Ok(CodexSchemaAuditReport {
        release_source,
        binary_cache_dir,
        latest_stable_release_tag: latest_tag.raw_tag.clone(),
        latest_exact_covered_version: latest_exact_supported_codex_cli_version().to_string(),
        audited_tags: audited_tags.into_iter().map(|tag| tag.raw_tag).collect(),
        outcome: drift
            .map(CodexSchemaAuditOutcome::Drift)
            .unwrap_or(CodexSchemaAuditOutcome::Compatible),
    })
}

/// Parses and sorts the stable Codex release tags that matter to the audit.
fn collect_stable_release_tags(tag_names: Vec<String>) -> Vec<StableCodexReleaseTag> {
    let mut stable_tags = tag_names
        .into_iter()
        .filter_map(|tag_name| parse_stable_release_tag(&tag_name))
        .collect::<Vec<_>>();
    stable_tags.sort_by(|left, right| {
        right
            .version
            .cmp(&left.version)
            .then_with(|| left.raw_tag.cmp(&right.raw_tag))
    });
    stable_tags.dedup_by(|left, right| left.raw_tag == right.raw_tag);
    stable_tags
}

/// Parses one raw release tag into a stable Codex release tag if it matches the expected format.
fn parse_stable_release_tag(tag_name: &str) -> Option<StableCodexReleaseTag> {
    let version_text = tag_name.strip_prefix(RELEASE_TAG_PREFIX)?;
    let version = CodexCliVersion::parse(version_text).ok()?;
    version.is_stable().then_some(StableCodexReleaseTag {
        raw_tag: tag_name.to_owned(),
        version,
    })
}

/// Selects the audited stable tag range from latest stable down to darc's exact cutoff tag.
fn select_audited_release_tags(
    stable_tags: &[StableCodexReleaseTag],
) -> Result<Vec<StableCodexReleaseTag>> {
    let exact_version = latest_exact_supported_codex_cli_version();
    ensure!(
        exact_version.is_stable(),
        "darc's latest exact-covered Codex version `{exact_version}` is not a stable release"
    );
    ensure!(
        !stable_tags.is_empty(),
        "no stable Codex releases were available from GitHub Releases"
    );

    let exact_index = stable_tags
        .iter()
        .position(|tag| tag.version == exact_version)
        .with_context(|| {
            format!(
                "GitHub Releases are missing the stable release tag `{}{}` required by darc's exact coverage boundary",
                RELEASE_TAG_PREFIX, exact_version
            )
        })?;

    Ok(stable_tags[..=exact_index].to_vec())
}

/// Formats captured command stderr into a compact single-line failure summary.
fn command_output_summary(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "command exited without stderr output".to_owned();
    }
    truncate_text(&trimmed.replace('\n', " "), 240)
}

/// Returns a unique filesystem suffix for temporary audit paths.
fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

/// Rewrites arbitrary text into a filesystem-safe path fragment.
fn sanitize_for_path(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        ffi::OsString,
        fs::File,
        path::{Path, PathBuf},
        process::Command,
        time::Duration,
    };

    use anyhow::anyhow;
    use flate2::{Compression, write::GzEncoder};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use tar::{Builder, Header};

    use super::{
        CodexSchemaAuditOutcome, ScopedTempDir, StableCodexReleaseTag,
        build_released_binary_command, collect_stable_release_tags,
        extract_verified_binary_package, host_platform_from_parts,
        latest_exact_supported_codex_cli_version, parse_stable_release_tag,
        run_codex_schema_audit_with_provider, run_codex_schema_audit_with_provider_and_progress,
        run_command_with_timeout, select_audited_release_tags,
    };

    struct FakeSchemaAuditProvider {
        tag_names: Vec<String>,
        schemas: BTreeMap<String, serde_json::Value>,
    }

    impl FakeSchemaAuditProvider {
        /// Builds one fake provider for audit tests.
        fn new(tag_names: &[&str], schemas: &[(&str, serde_json::Value)]) -> Self {
            Self {
                tag_names: tag_names.iter().map(|tag| (*tag).to_owned()).collect(),
                schemas: schemas
                    .iter()
                    .map(|(tag, schema)| ((*tag).to_owned(), schema.clone()))
                    .collect(),
            }
        }
    }

    impl super::CodexSchemaAuditProvider for FakeSchemaAuditProvider {
        fn list_release_tag_names(&self) -> anyhow::Result<Vec<String>> {
            Ok(self.tag_names.clone())
        }

        fn export_rollout_line_schema<F>(
            &self,
            tag_name: &str,
            _report_progress: &mut F,
        ) -> anyhow::Result<serde_json::Value>
        where
            F: FnMut(&str),
        {
            self.schemas
                .get(tag_name)
                .cloned()
                .ok_or_else(|| anyhow!("missing fake schema for `{tag_name}`"))
        }
    }

    fn raw_tags(tags: &[StableCodexReleaseTag]) -> Vec<String> {
        tags.iter().map(|tag| tag.raw_tag.clone()).collect()
    }

    fn command_envs(command: &std::process::Command) -> BTreeMap<OsString, Option<OsString>> {
        command
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.map(|value| value.to_owned())))
            .collect()
    }

    fn write_test_release_archive(archive_path: &Path, relative_path: &Path, contents: &[u8]) {
        let archive_file = File::create(archive_path).unwrap();
        let encoder = GzEncoder::new(archive_file, Compression::default());
        let mut builder = Builder::new(encoder);
        let mut header = Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(contents.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, relative_path, contents)
            .unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
    }

    fn file_sha256(path: &Path) -> String {
        let bytes = std::fs::read(path).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let mut digest = String::new();
        for byte in hasher.finalize() {
            use std::fmt::Write as _;

            write!(&mut digest, "{byte:02x}").unwrap();
        }
        digest
    }

    #[test]
    fn parses_and_filters_stable_codex_release_tags() {
        let tags = collect_stable_release_tags(vec![
            "rust-v0.128.0".to_owned(),
            "rust-v0.129.0-alpha.1".to_owned(),
            "rust-v0.127.0".to_owned(),
            "rust-vrust-v0.99.0-alpha.16".to_owned(),
            "not-a-codex-tag".to_owned(),
        ]);

        assert_eq!(
            raw_tags(&tags),
            vec!["rust-v0.128.0".to_owned(), "rust-v0.127.0".to_owned()]
        );
        assert_eq!(
            parse_stable_release_tag("rust-v0.128.0")
                .unwrap()
                .version
                .to_string(),
            "0.128.0"
        );
        assert!(parse_stable_release_tag("rust-v0.128.0-alpha.1").is_none());
    }

    #[test]
    fn selects_audited_range_from_latest_stable_down_to_exact_cutoff() {
        assert_eq!(
            latest_exact_supported_codex_cli_version().to_string(),
            "0.128.0"
        );

        let tags = collect_stable_release_tags(vec![
            "rust-v0.130.0".to_owned(),
            "rust-v0.129.0".to_owned(),
            "rust-v0.128.0".to_owned(),
            "rust-v0.127.0".to_owned(),
        ]);

        let selected = select_audited_release_tags(&tags).unwrap();

        assert_eq!(
            raw_tags(&selected),
            vec![
                "rust-v0.130.0".to_owned(),
                "rust-v0.129.0".to_owned(),
                "rust-v0.128.0".to_owned(),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_helper_drains_large_child_output() {
        let mut command = Command::new("sh");
        command.arg("-c").arg(
            "i=0; while [ \"$i\" -lt 4096 ]; do printf '0123456789abcdef0123456789abcdef'; i=$((i + 1)); done; printf stderr-ok >&2",
        );

        let output = run_command_with_timeout(&mut command, Duration::from_secs(5)).unwrap();

        assert!(output.status.success());
        assert!(output.stdout.len() > 64 * 1024);
        assert_eq!(String::from_utf8_lossy(&output.stderr), "stderr-ok");
    }

    #[cfg(unix)]
    #[test]
    fn timeout_helper_closes_child_stdin() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("if read _line; then printf open; else printf closed; fi");

        let output = run_command_with_timeout(&mut command, Duration::from_secs(5)).unwrap();

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "closed");
    }

    #[test]
    fn reports_compatibility_when_normalized_schemas_match() {
        let provider = FakeSchemaAuditProvider::new(
            &["rust-v0.130.0", "rust-v0.129.0", "rust-v0.128.0"],
            &[
                (
                    "rust-v0.130.0",
                    json!({
                        "type": ["null", "object"],
                        "definitions": {
                            "RolloutItem": {
                                "required": ["payload", "type"],
                                "title": "RolloutItem",
                            }
                        }
                    }),
                ),
                (
                    "rust-v0.129.0",
                    json!({
                        "definitions": {
                            "RolloutItem": {
                                "title": "RolloutItem",
                                "required": ["type", "payload"],
                            }
                        },
                        "type": ["object", "null"],
                    }),
                ),
                (
                    "rust-v0.128.0",
                    json!({
                        "type": ["object", "null"],
                        "definitions": {
                            "RolloutItem": {
                                "required": ["type", "payload"],
                                "title": "RolloutItem",
                            }
                        }
                    }),
                ),
            ],
        );

        let report = run_codex_schema_audit_with_provider(
            "GitHub Releases".to_owned(),
            PathBuf::from("/tmp/darc-cache"),
            &provider,
        )
        .unwrap();

        assert!(matches!(
            report.outcome,
            CodexSchemaAuditOutcome::Compatible
        ));
        assert_eq!(report.latest_stable_release_tag, "rust-v0.130.0");
        assert_eq!(
            report.audited_tags,
            vec![
                "rust-v0.130.0".to_owned(),
                "rust-v0.129.0".to_owned(),
                "rust-v0.128.0".to_owned(),
            ]
        );
    }

    #[test]
    fn detects_drift_at_the_first_newer_stable_tag() {
        let provider = FakeSchemaAuditProvider::new(
            &["rust-v0.130.0", "rust-v0.129.0", "rust-v0.128.0"],
            &[
                (
                    "rust-v0.130.0",
                    json!({
                        "type": "object",
                        "required": ["timestamp", "item", "trace_id"],
                    }),
                ),
                (
                    "rust-v0.129.0",
                    json!({
                        "type": "object",
                        "required": ["timestamp", "item", "trace_id"],
                    }),
                ),
                (
                    "rust-v0.128.0",
                    json!({
                        "type": "object",
                        "required": ["timestamp", "item"],
                    }),
                ),
            ],
        );

        let report = run_codex_schema_audit_with_provider(
            "GitHub Releases".to_owned(),
            PathBuf::from("/tmp/darc-cache"),
            &provider,
        )
        .unwrap();

        let CodexSchemaAuditOutcome::Drift(drift) = report.outcome else {
            panic!("expected schema drift");
        };
        assert_eq!(drift.first_drift_tag, "rust-v0.129.0");
        assert!(
            drift
                .difference_summary
                .iter()
                .any(|line| line.contains("required"))
        );
    }

    #[test]
    fn reports_progress_for_compatible_audits() {
        let provider = FakeSchemaAuditProvider::new(
            &["rust-v0.130.0", "rust-v0.129.0", "rust-v0.128.0"],
            &[
                (
                    "rust-v0.130.0",
                    json!({
                        "type": "object",
                        "required": ["timestamp", "item"],
                    }),
                ),
                (
                    "rust-v0.129.0",
                    json!({
                        "type": "object",
                        "required": ["item", "timestamp"],
                    }),
                ),
                (
                    "rust-v0.128.0",
                    json!({
                        "required": ["timestamp", "item"],
                        "type": "object",
                    }),
                ),
            ],
        );
        let mut progress = Vec::new();

        let report = run_codex_schema_audit_with_provider_and_progress(
            "GitHub Releases".to_owned(),
            PathBuf::from("/tmp/darc-cache"),
            &provider,
            &mut |message| progress.push(message.to_owned()),
        )
        .unwrap();

        assert!(matches!(
            report.outcome,
            CodexSchemaAuditOutcome::Compatible
        ));
        assert_eq!(
            progress.first().map(String::as_str),
            Some("Listing stable Codex release tags...")
        );
        assert!(
            progress
                .iter()
                .any(|line| line
                    .contains("Exporting baseline RolloutLine schema from rust-v0.128.0"))
        );
        assert!(
            progress
                .iter()
                .any(|line| line.contains("Comparing rust-v0.129.0 against baseline (1/2)"))
        );
        assert!(progress.last().is_some_and(|line| {
            line.contains("No schema drift detected across 3 audited stable tag(s).")
        }));
    }

    #[test]
    fn detects_order_sensitive_prefix_items_drift() {
        let provider = FakeSchemaAuditProvider::new(
            &["rust-v0.130.0", "rust-v0.129.0", "rust-v0.128.0"],
            &[
                (
                    "rust-v0.130.0",
                    json!({
                        "type": "array",
                        "prefixItems": [
                            { "title": "second", "type": "number" },
                            { "title": "first", "type": "string" }
                        ],
                    }),
                ),
                (
                    "rust-v0.129.0",
                    json!({
                        "type": "array",
                        "prefixItems": [
                            { "title": "second", "type": "number" },
                            { "title": "first", "type": "string" }
                        ],
                    }),
                ),
                (
                    "rust-v0.128.0",
                    json!({
                        "type": "array",
                        "prefixItems": [
                            { "title": "first", "type": "string" },
                            { "title": "second", "type": "number" }
                        ],
                    }),
                ),
            ],
        );

        let report = run_codex_schema_audit_with_provider(
            "GitHub Releases".to_owned(),
            PathBuf::from("/tmp/darc-cache"),
            &provider,
        )
        .unwrap();

        let CodexSchemaAuditOutcome::Drift(drift) = report.outcome else {
            panic!("expected schema drift");
        };
        assert_eq!(drift.first_drift_tag, "rust-v0.129.0");
        assert!(
            drift
                .difference_summary
                .iter()
                .any(|line| line.contains("prefixItems"))
        );
    }

    #[test]
    fn ignores_order_only_one_of_reorders() {
        let provider = FakeSchemaAuditProvider::new(
            &["rust-v0.130.0", "rust-v0.129.0", "rust-v0.128.0"],
            &[
                (
                    "rust-v0.130.0",
                    json!({
                        "oneOf": [
                            {
                                "properties": {
                                    "kind": { "const": "b" },
                                    "value": { "type": "number" }
                                },
                                "required": ["value", "kind"],
                                "type": "object"
                            },
                            {
                                "properties": {
                                    "kind": { "const": "a" },
                                    "value": { "type": "string" }
                                },
                                "required": ["kind", "value"],
                                "type": "object"
                            }
                        ]
                    }),
                ),
                (
                    "rust-v0.129.0",
                    json!({
                        "oneOf": [
                            {
                                "properties": {
                                    "kind": { "const": "b" },
                                    "value": { "type": "number" }
                                },
                                "required": ["kind", "value"],
                                "type": "object"
                            },
                            {
                                "properties": {
                                    "kind": { "const": "a" },
                                    "value": { "type": "string" }
                                },
                                "required": ["value", "kind"],
                                "type": "object"
                            }
                        ]
                    }),
                ),
                (
                    "rust-v0.128.0",
                    json!({
                        "oneOf": [
                            {
                                "properties": {
                                    "kind": { "const": "a" },
                                    "value": { "type": "string" }
                                },
                                "required": ["kind", "value"],
                                "type": "object"
                            },
                            {
                                "properties": {
                                    "kind": { "const": "b" },
                                    "value": { "type": "number" }
                                },
                                "required": ["kind", "value"],
                                "type": "object"
                            }
                        ]
                    }),
                ),
            ],
        );

        let report = run_codex_schema_audit_with_provider(
            "GitHub Releases".to_owned(),
            PathBuf::from("/tmp/darc-cache"),
            &provider,
        )
        .unwrap();

        assert!(matches!(
            report.outcome,
            CodexSchemaAuditOutcome::Compatible
        ));
    }

    #[test]
    fn ignores_order_only_all_of_reorders() {
        let provider = FakeSchemaAuditProvider::new(
            &["rust-v0.130.0", "rust-v0.129.0", "rust-v0.128.0"],
            &[
                (
                    "rust-v0.130.0",
                    json!({
                        "allOf": [
                            { "properties": { "kind": { "const": "x" } }, "type": "object" },
                            { "properties": { "value": { "type": "string" } }, "type": "object" }
                        ]
                    }),
                ),
                (
                    "rust-v0.129.0",
                    json!({
                        "allOf": [
                            { "properties": { "kind": { "const": "x" } }, "type": "object" },
                            { "properties": { "value": { "type": "string" } }, "type": "object" }
                        ]
                    }),
                ),
                (
                    "rust-v0.128.0",
                    json!({
                        "allOf": [
                            { "properties": { "value": { "type": "string" } }, "type": "object" },
                            { "properties": { "kind": { "const": "x" } }, "type": "object" }
                        ]
                    }),
                ),
            ],
        );

        let report = run_codex_schema_audit_with_provider(
            "GitHub Releases".to_owned(),
            PathBuf::from("/tmp/darc-cache"),
            &provider,
        )
        .unwrap();

        assert!(matches!(
            report.outcome,
            CodexSchemaAuditOutcome::Compatible
        ));
    }

    #[test]
    fn resolves_expected_release_asset_names_for_supported_platforms() {
        let mac = host_platform_from_parts("macos", "aarch64").unwrap();
        assert_eq!(
            mac.release_asset_name("0.128.0"),
            "codex-npm-darwin-arm64-0.128.0.tgz"
        );
        assert_eq!(
            mac.package_binary_path(),
            PathBuf::from("package/vendor/aarch64-apple-darwin/codex/codex")
        );

        let linux = host_platform_from_parts("linux", "x86_64").unwrap();
        assert_eq!(
            linux.release_asset_name("0.128.0"),
            "codex-npm-linux-x64-0.128.0.tgz"
        );
        assert_eq!(
            linux.package_binary_path(),
            PathBuf::from("package/vendor/x86_64-unknown-linux-musl/codex/codex")
        );
    }

    #[test]
    fn released_binary_command_uses_scrubbed_runtime_environment() {
        let scratch_dir = ScopedTempDir::new("darc-codex-schema-audit-test").unwrap();
        let runtime_root = scratch_dir.path().join("runtime");
        let working_dir = scratch_dir.path().join("work");
        std::fs::create_dir_all(&working_dir).unwrap();
        let binary_path = scratch_dir.path().join("codex");

        let command =
            build_released_binary_command(&binary_path, &working_dir, &runtime_root).unwrap();
        let envs = command_envs(&command);

        assert_eq!(command.get_current_dir(), Some(working_dir.as_path()));
        assert_eq!(
            envs.get(&OsString::from("HOME")),
            Some(&Some(runtime_root.join("home").into_os_string()))
        );
        assert_eq!(
            envs.get(&OsString::from("TMPDIR")),
            Some(&Some(runtime_root.join("tmp").into_os_string()))
        );
        assert_eq!(
            envs.get(&OsString::from("XDG_CACHE_HOME")),
            Some(&Some(runtime_root.join("home/.cache").into_os_string()))
        );
        assert!(!envs.contains_key(&OsString::from("GH_TOKEN")));
        assert!(!envs.contains_key(&OsString::from("GITHUB_TOKEN")));
        assert!(!envs.contains_key(&OsString::from("PATH")));
    }

    #[test]
    fn verified_binary_package_extraction_rejects_tampered_cached_archive() {
        let scratch_dir = ScopedTempDir::new("darc-codex-schema-audit-test").unwrap();
        let archive_path = scratch_dir.path().join("release.tgz");
        let extraction_root = scratch_dir.path().join("extract");
        let relative_binary_path =
            PathBuf::from("package/vendor/x86_64-unknown-linux-musl/codex/codex");

        write_test_release_archive(&archive_path, &relative_binary_path, b"trusted-binary");
        let digest = file_sha256(&archive_path);
        extract_verified_binary_package(&archive_path, &digest, &extraction_root).unwrap();
        assert_eq!(
            std::fs::read(extraction_root.join(&relative_binary_path)).unwrap(),
            b"trusted-binary"
        );

        std::fs::write(&archive_path, b"tampered").unwrap();
        let error =
            extract_verified_binary_package(&archive_path, &digest, &extraction_root).unwrap_err();
        assert!(error.to_string().contains("SHA-256 mismatch"));
    }
}
