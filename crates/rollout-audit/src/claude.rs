use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use darc_paths::encode_path_for_claude;
use darc_rollout::claude::{ClaudeCliVersion, latest_exact_supported_claude_cli_version};
use directories::BaseDirs;
use flate2::read::GzDecoder;
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha512};
use tar::Archive;

use crate::schema_diff::{normalize_json, summarize_schema_differences, truncate_text};

/// Stores the npm package name for released Claude CLI audits.
const NPM_CLAUDE_CODE_PACKAGE: &str = "@anthropic-ai/claude-code";
/// Stores the native-wrapper marker expected by Claude's npm launcher.
const CLAUDE_NPM_WRAPPER_ENV_NAME: &str = "CLAUDE_CODE_INSTALLED_VIA_NPM_WRAPPER";
/// Stores the npm package name for released Claude SDK audits.
const NPM_AGENT_SDK_PACKAGE: &str = "@anthropic-ai/claude-agent-sdk";
/// Stores the human-readable source label for Claude npm releases.
const NPM_RELEASE_SOURCE: &str = "npm registry (@anthropic-ai/claude-code)";
/// Stores the base URL for npm registry metadata requests.
const NPM_REGISTRY_BASE_URL: &str = "https://registry.npmjs.org";
/// Stores the timeout used for released Claude CLI command execution.
const CLI_COMMAND_TIMEOUT: Duration = Duration::from_secs(180);
/// Lists the Darc files most likely to need updates after Claude schema drift.
const LIKELY_UPDATE_PATHS: &[&str] = &[
    "crates/rollout/src/claude/version.rs",
    "crates/rollout/src/claude/mod.rs",
    "crates/rollout-audit/src/claude.rs",
    "crates/cli/src/lib.rs",
];
/// Lists the exact auth environment variables allowed into host-auth audit runs.
const AUTH_ENV_NAMES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_PROFILE",
    "AWS_CONFIG_FILE",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ROLE_ARN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT",
    "GCLOUD_PROJECT",
    "CLOUDSDK_CONFIG",
    "AZURE_OPENAI_API_KEY",
    "AZURE_OPENAI_ENDPOINT",
];
/// Lists auth environment variable prefixes allowed into host-auth audit runs.
const AUTH_ENV_PREFIXES: &[&str] = &["VERTEX_REGION_"];
/// Lists benign shell environment variables forwarded to released Claude processes.
const ENV_PASSTHROUGH_NAMES: &[&str] = &[
    "LANG", "LC_ALL", "PATH", "SHELL", "TERM", "USER", "LOGNAME", "USERNAME",
];
/// Lists host runtime environment variables needed for released Claude processes.
const HOST_RUNTIME_ENV_NAMES: &[&str] = &[
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "TMPDIR",
    "TMP",
    "TEMP",
];
/// Stores the audit settings file name written into temporary Claude workspaces.
const SETTINGS_FILE_NAME: &str = "claude-audit-settings.json";
/// Stores the hook capture file name written during Claude audit runs.
const HOOK_CAPTURE_FILE_NAME: &str = "claude-audit-hooks.jsonl";
/// Stores the temporary workspace directory name used for Claude audit fixtures.
const FIXTURE_WORKSPACE_DIR_NAME: &str = ".darc-claude-audit";

/// Stores the input options for a Claude rollout schema compatibility audit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeSchemaAuditOptions {
    pub cache_dir: Option<PathBuf>,
    pub use_host_auth: bool,
    pub sample_stride: usize,
    pub from_version: Option<String>,
    pub survey_mode: ClaudeSchemaSurveyMode,
}

/// Stores the structured result of one Claude rollout schema compatibility audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaudeSchemaAuditReport {
    pub release_source: String,
    pub binary_cache_dir: PathBuf,
    pub latest_published_version: String,
    pub latest_exact_covered_version: String,
    pub audited_versions: Vec<String>,
    pub inspected_versions: Vec<String>,
    pub assumed_compatible_intervals: Vec<String>,
    pub sample_stride: usize,
    pub used_host_auth: bool,
    pub survey_mode: ClaudeSchemaSurveyMode,
    pub transcript_drift_windows: Vec<ClaudeSchemaDriftWindow>,
    pub outcome: ClaudeSchemaAuditOutcome,
    pub supplementary_sdk_drift: Option<ClaudeSdkSchemaDrift>,
}

impl ClaudeSchemaAuditReport {
    /// Returns whether the audited Claude versions are transcript-compatible with darc.
    pub fn is_compatible(&self) -> bool {
        matches!(self.outcome, ClaudeSchemaAuditOutcome::Compatible)
    }

    /// Formats the audited Claude version range for user-facing summaries.
    pub fn audited_version_range(&self) -> String {
        match (self.audited_versions.first(), self.audited_versions.last()) {
            (Some(first), Some(last)) if first == last => first.clone(),
            (Some(first), Some(last)) => format!("{first} ..= {last}"),
            _ => "<empty>".to_owned(),
        }
    }
}

/// Stores whether the audited Claude versions stayed transcript-compatible or drifted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ClaudeSchemaAuditOutcome {
    Compatible,
    Drift(ClaudeSchemaDrift),
}

/// Selects how the Claude schema audit handles drift windows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeSchemaSurveyMode {
    #[default]
    Refine,
    Coarse,
}

/// Stores the first detected Claude transcript schema drift against darc's baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaudeSchemaDrift {
    pub first_drift_version: String,
    pub difference_summary: Vec<String>,
    pub likely_files_to_update: Vec<String>,
}

/// Stores one sampled Claude transcript drift window between two compatible anchors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaudeSchemaDriftWindow {
    pub window_start_version: String,
    pub window_end_version: String,
    pub sampled_compatible_version: String,
    pub sampled_drift_version: String,
    pub difference_summary: Vec<String>,
}

/// Stores the first detected supplementary Agent SDK surface drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaudeSdkSchemaDrift {
    pub first_drift_version: String,
    pub difference_summary: Vec<String>,
}

/// Runs the hook-ready Claude rollout schema compatibility audit.
pub fn run_claude_schema_audit(
    options: ClaudeSchemaAuditOptions,
) -> Result<ClaudeSchemaAuditReport> {
    let mut noop = |_: &str| {};
    run_claude_schema_audit_with_progress(options, &mut noop)
}

/// Runs the hook-ready Claude rollout schema compatibility audit with progress updates.
pub fn run_claude_schema_audit_with_progress<F>(
    options: ClaudeSchemaAuditOptions,
    mut report_progress: F,
) -> Result<ClaudeSchemaAuditReport>
where
    F: FnMut(&str),
{
    ensure!(
        options.use_host_auth,
        "Claude schema audit requires --use-host-auth because darc does not provide an OS-level sandbox for safely executing published Claude packages"
    );
    report_progress("Resolving schema audit cache directory...");
    let cache_dir = resolve_binary_cache_dir(options.cache_dir.as_deref())?;
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("failed to create {}", cache_dir.display()))?;
    report_progress(&format!("Using audit cache: {}", cache_dir.display()));

    report_progress("Detecting local runtimes required by released Claude packages...");
    let runtime = AuditRuntime::detect()?;
    report_progress(&format!(
        "Using Node runtime `{}` and hook interpreter `{}`.",
        runtime.node_binary.display(),
        runtime.hook_python.display()
    ));

    report_progress("Fetching Claude Code package metadata from the npm registry...");
    let cli_catalog = NpmPackageCatalog::fetch(NPM_CLAUDE_CODE_PACKAGE, &mut report_progress)?;
    report_progress(&format!(
        "Fetched {} published Claude Code version(s).",
        cli_catalog.versions.len()
    ));

    report_progress("Fetching Claude Agent SDK package metadata from the npm registry...");
    let sdk_catalog = match NpmPackageCatalog::fetch(NPM_AGENT_SDK_PACKAGE, &mut report_progress) {
        Ok(catalog) => {
            report_progress(&format!(
                "Fetched {} published Claude Agent SDK version(s).",
                catalog.versions.len()
            ));
            Some(catalog)
        }
        Err(error) => {
            report_progress(&format!(
                "Skipping supplementary Agent SDK audit because metadata fetch failed: {error:#}"
            ));
            None
        }
    };

    ensure!(
        options.sample_stride > 0,
        "Claude schema audit sample stride must be at least 1"
    );
    if options.survey_mode == ClaudeSchemaSurveyMode::Coarse {
        report_progress(
            "Using coarse Claude survey mode; sampled drift windows will be reported without exact refinement.",
        );
    }
    report_progress(
        "Using opt-in host Claude auth mode; published Claude packages will execute with the caller's existing Claude login or credential environment.",
    );

    let fixture_workspace_root = resolve_fixture_workspace_root()?;
    fs::create_dir_all(&fixture_workspace_root)
        .with_context(|| format!("failed to create {}", fixture_workspace_root.display()))?;
    report_progress(&format!(
        "Using Claude audit workspace root: {}",
        fixture_workspace_root.display()
    ));

    let audit_floor_version = options.from_version.clone();

    let provider = NpmClaudeSchemaAuditProvider::new(
        cli_catalog,
        sdk_catalog,
        cache_dir.clone(),
        fixture_workspace_root,
        options.use_host_auth,
        runtime,
    )?;
    run_claude_schema_audit_with_provider_and_progress(
        NPM_RELEASE_SOURCE.to_owned(),
        cache_dir,
        &provider,
        options.sample_stride,
        audit_floor_version,
        options.survey_mode,
        &mut report_progress,
    )
}

/// Lists published Claude versions and derives transcript manifests per version.
trait ClaudeSchemaAuditProvider {
    /// Returns the raw published Claude versions available to the audit.
    fn list_release_versions(&self) -> Result<Vec<String>>;

    /// Returns whether this provider is using opt-in host auth.
    fn uses_host_auth(&self) -> bool {
        false
    }

    /// Collects one derived audit snapshot for a specific Claude version.
    fn collect_snapshot<F>(
        &self,
        version: &str,
        report_progress: &mut F,
    ) -> Result<ClaudeAuditSnapshot>
    where
        F: FnMut(&str);
}

/// Stores one derived Claude audit snapshot for a released version.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaudeAuditSnapshot {
    transcript_manifest: ClaudeTranscriptSchemaManifest,
    sdk_manifest: ClaudeSdkSchemaManifest,
}

/// Stores the runtime dependencies needed to execute released Claude packages.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AuditRuntime {
    node_binary: PathBuf,
    node_platform_suffix: String,
    hook_python: PathBuf,
}

impl AuditRuntime {
    /// Detects the local binaries required for Claude audit fixture execution.
    fn detect() -> Result<Self> {
        let node_binary = resolve_runtime_binary(&["node"])?;
        let node_platform_suffix = detect_node_native_cli_platform_suffix(&node_binary)?;
        Ok(Self {
            node_binary,
            node_platform_suffix,
            hook_python: resolve_runtime_binary(if cfg!(windows) {
                &["python", "python3"]
            } else {
                &["python3", "python"]
            })?,
        })
    }
}

/// Describes how to execute one released Claude CLI package.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReleasedClaudeCli {
    NodeScript(PathBuf),
    NativeBinary(PathBuf),
}

impl ReleasedClaudeCli {
    /// Builds a command for this released Claude CLI target.
    fn command(&self, runtime: &AuditRuntime, working_dir: &Path) -> Command {
        let mut command = match self {
            Self::NodeScript(entrypoint) => {
                let mut command = Command::new(&runtime.node_binary);
                command.arg(entrypoint);
                command
            }
            Self::NativeBinary(binary) => Command::new(binary),
        };
        command.current_dir(working_dir);
        command
    }

    /// Applies environment entries required by this released Claude CLI target.
    fn apply_target_environment(&self, command: &mut Command) {
        if matches!(self, Self::NativeBinary(_)) {
            command.env(CLAUDE_NPM_WRAPPER_ENV_NAME, "1");
        }
    }

    /// Returns the path used to identify this released CLI target in diagnostics.
    fn display_path(&self) -> &Path {
        match self {
            Self::NodeScript(path) | Self::NativeBinary(path) => path,
        }
    }
}

/// Stores one fetched npm package catalog.
#[derive(Debug, Clone, Deserialize)]
struct NpmPackageCatalog {
    #[serde(default)]
    versions: BTreeMap<String, NpmPackageVersion>,
}

impl NpmPackageCatalog {
    /// Fetches one npm package catalog from the public registry.
    fn fetch<F>(package_name: &str, report_progress: &mut F) -> Result<Self>
    where
        F: FnMut(&str),
    {
        let client = build_http_client()?;
        let url = format!("{NPM_REGISTRY_BASE_URL}/{package_name}");
        report_progress(&format!("Fetching npm metadata for {package_name}..."));
        let response = send_checked_request(
            client.get(&url),
            &format!("fetch npm metadata for `{package_name}`"),
        )?;
        let bytes = response
            .bytes()
            .context("failed to read npm registry response body")?;
        serde_json::from_slice(&bytes).context("failed to parse npm registry response JSON")
    }

    /// Returns the published metadata row for one package version.
    fn version(&self, version: &str) -> Option<&NpmPackageVersion> {
        self.versions.get(version)
    }
}

/// Stores the registry metadata needed from one npm package version row.
#[derive(Debug, Clone, Deserialize)]
struct NpmPackageVersion {
    version: String,
    dist: NpmPackageDist,
    #[serde(rename = "claudeCodeVersion")]
    claude_code_version: Option<String>,
}

/// Stores the npm package metadata fields needed to resolve the Claude CLI entrypoint.
#[derive(Debug, Clone, Deserialize)]
struct NpmPackageManifest {
    #[serde(default)]
    bin: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, String>,
}

/// Stores the npm distribution metadata needed to download one package tarball.
#[derive(Debug, Clone, Deserialize)]
struct NpmPackageDist {
    tarball: String,
    integrity: Option<String>,
}

/// Stores one released Claude fixture scenario used for transcript derivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClaudeAuditFixture {
    name: &'static str,
    prompt: &'static str,
    allowed_tools: &'static [&'static str],
    required_tools: &'static [&'static str],
    max_turns: u32,
    require_subagent_signal: bool,
}

/// Stores one parsed package integrity descriptor from the npm registry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PackageIntegrity {
    algorithm: IntegrityAlgorithm,
    base64_digest: String,
}

/// Enumerates the package integrity algorithms supported by the audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntegrityAlgorithm {
    Sha512,
}

/// Binds npm package downloads and fixture execution to the Claude audit provider trait.
struct NpmClaudeSchemaAuditProvider {
    cli_catalog: NpmPackageCatalog,
    sdk_catalog: Option<NpmPackageCatalog>,
    cache_dir: PathBuf,
    http: Client,
    scratch_dir: ScopedTempDir,
    fixture_run_dir: ScopedTempDir,
    use_host_auth: bool,
    runtime: AuditRuntime,
}

impl NpmClaudeSchemaAuditProvider {
    /// Creates one Claude audit provider backed by npm registry metadata.
    fn new(
        cli_catalog: NpmPackageCatalog,
        sdk_catalog: Option<NpmPackageCatalog>,
        cache_dir: PathBuf,
        fixture_workspace_root: PathBuf,
        use_host_auth: bool,
        runtime: AuditRuntime,
    ) -> Result<Self> {
        Ok(Self {
            cli_catalog,
            sdk_catalog,
            cache_dir,
            http: build_http_client()?,
            scratch_dir: ScopedTempDir::new("darc-claude-schema-audit")?,
            fixture_run_dir: ScopedTempDir::new_in(
                &fixture_workspace_root,
                "darc-claude-schema-audit-run",
            )?,
            use_host_auth,
            runtime,
        })
    }

    /// Returns the published CLI package row for one Claude version.
    fn cli_package(&self, version: &str) -> Result<&NpmPackageVersion> {
        self.cli_catalog
            .version(version)
            .with_context(|| format!("missing npm metadata for Claude Code `{version}`"))
    }

    /// Returns the best matching Agent SDK package row for one Claude version when available.
    fn matching_agent_sdk_package(&self, version: &str) -> Option<(String, &NpmPackageVersion)> {
        let mut matches = self
            .sdk_catalog
            .as_ref()?
            .versions
            .values()
            .filter(|package| package.claude_code_version.as_deref() == Some(version))
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            ClaudeCliVersion::parse(&right.version)
                .ok()
                .cmp(&ClaudeCliVersion::parse(&left.version).ok())
                .then_with(|| left.version.cmp(&right.version))
        });
        matches
            .into_iter()
            .next()
            .map(|package| (package.version.clone(), package))
    }

    /// Ensures one verified npm package tarball is cached locally and returns its path.
    fn ensure_cached_package<F>(
        &self,
        package_name: &str,
        package_version: &str,
        package: &NpmPackageVersion,
        report_progress: &mut F,
    ) -> Result<PathBuf>
    where
        F: FnMut(&str),
    {
        let integrity = parse_package_integrity(
            package.dist.integrity.as_deref(),
            package_name,
            package_version,
        )?;
        let asset_name = tarball_file_name(&package.dist.tarball)?;
        let cache_root = self
            .cache_dir
            .join(sanitize_for_path(package_name))
            .join(integrity.cache_key());
        let cached_archive_path = cache_root.join(&asset_name);

        if cached_archive_path.is_file() {
            report_progress(&format!(
                "Verifying cached npm tarball for {package_name}@{package_version}..."
            ));
            if let Err(error) = verify_file_integrity(&cached_archive_path, &integrity) {
                report_progress(&format!(
                    "Cached npm tarball for {package_name}@{package_version} failed integrity verification; refreshing cache."
                ));
                if cache_root.exists() {
                    fs::remove_dir_all(&cache_root)
                        .with_context(|| format!("failed to remove {}", cache_root.display()))?;
                }
                report_progress(&format!(
                    "Discarded invalid npm cache for {package_name}@{package_version}: {error:#}"
                ));
            }
        }

        if !cached_archive_path.is_file() {
            let archive_path = self.scratch_dir.path().join(format!(
                "{}-{}-{}",
                sanitize_for_path(package_name),
                sanitize_for_path(package_version),
                unique_suffix()
            ));
            report_progress(&format!(
                "Downloading npm tarball for {package_name}@{package_version}..."
            ));
            download_to_path(
                &self.http,
                &package.dist.tarball,
                &archive_path,
                &format!("download npm package `{package_name}@{package_version}`"),
            )?;
            report_progress(&format!(
                "Verifying npm integrity for {package_name}@{package_version}..."
            ));
            verify_file_integrity(&archive_path, &integrity)?;
            report_progress(&format!(
                "Caching verified npm tarball for {package_name}@{package_version}..."
            ));
            stage_cached_archive_package(&archive_path, &cached_archive_path)?;
        }

        Ok(cached_archive_path)
    }

    /// Extracts one verified npm tarball into a temporary directory and returns its package root.
    fn extract_cached_package<F>(
        &self,
        package_name: &str,
        package_version: &str,
        package: &NpmPackageVersion,
        report_progress: &mut F,
    ) -> Result<PathBuf>
    where
        F: FnMut(&str),
    {
        let archive_path =
            self.ensure_cached_package(package_name, package_version, package, report_progress)?;
        let extraction_root = self.scratch_dir.path().join(format!(
            "package-{}-{}-{}",
            sanitize_for_path(package_name),
            sanitize_for_path(package_version),
            unique_suffix()
        ));
        report_progress(&format!(
            "Extracting verified npm tarball for {package_name}@{package_version}..."
        ));
        extract_package(archive_path.as_path(), &extraction_root)?;
        Ok(extraction_root.join("package"))
    }

    /// Collects the supplementary static SDK manifest for one Claude version.
    fn collect_sdk_manifest<F>(
        &self,
        version: &str,
        cli_root: &Path,
        report_progress: &mut F,
    ) -> Result<ClaudeSdkSchemaManifest>
    where
        F: FnMut(&str),
    {
        let mut manifest = collect_cli_sdk_manifest(cli_root, version, report_progress);

        let Some((sdk_version, sdk_package)) = self.matching_agent_sdk_package(version) else {
            report_progress(&format!(
                "No published Agent SDK build advertises compatibility with Claude Code {version}; continuing without supplementary SDK types."
            ));
            return Ok(manifest);
        };

        report_progress(&format!(
            "Collecting supplementary Agent SDK types from {sdk_version} for Claude Code {version}..."
        ));
        let sdk_root = match self.extract_cached_package(
            NPM_AGENT_SDK_PACKAGE,
            &sdk_version,
            sdk_package,
            report_progress,
        ) {
            Ok(sdk_root) => sdk_root,
            Err(error) => {
                report_progress(&format!(
                    "Skipping supplementary Agent SDK types for Claude Code {version}: {error:#}"
                ));
                return Ok(manifest);
            }
        };
        let sdk_dts = match fs::read_to_string(sdk_root.join("sdk.d.ts")) {
            Ok(sdk_dts) => sdk_dts,
            Err(error) => {
                report_progress(&format!(
                    "Skipping supplementary Agent SDK types for Claude Code {version} because {} could not be read: {error}",
                    sdk_root.join("sdk.d.ts").display()
                ));
                return Ok(manifest);
            }
        };
        manifest.agent_sdk_version = Some(sdk_version);
        manifest.agent_sdk_message_variants =
            collect_type_union_members(&sdk_dts, "export declare type SDKMessage =");
        manifest.agent_sdk_hook_events = collect_field_string_literals(&sdk_dts, "hook_event_name");
        Ok(manifest)
    }

    /// Runs the released Claude CLI fixtures and derives one transcript manifest.
    fn collect_transcript_manifest<F>(
        &self,
        version: &str,
        cli_root: &Path,
        report_progress: &mut F,
    ) -> Result<ClaudeTranscriptSchemaManifest>
    where
        F: FnMut(&str),
    {
        let cli_command = self.resolve_cli_command(version, cli_root, report_progress)?;
        let probe_root = self.fixture_run_dir.path().join(format!(
            "capability-probe-{}-{}",
            sanitize_for_path(version),
            unique_suffix()
        ));
        fs::create_dir_all(&probe_root)
            .with_context(|| format!("failed to create {}", probe_root.display()))?;
        let cli_capabilities =
            self.inspect_cli_capabilities_with_environment(&cli_command, cli_root, &probe_root)?;
        let mut builder = TranscriptManifestBuilder::default();
        for fixture in audit_fixtures() {
            report_progress(&format!(
                "Running Claude transcript fixture `{}` against {}...",
                fixture.name, version
            ));
            let session = self.run_fixture(version, &cli_command, cli_capabilities, *fixture)?;
            builder.record_fixture(&session, fixture.name)?;
            validate_fixture_coverage(&session, *fixture)?;
        }
        Ok(builder.finish())
    }

    /// Resolves a runnable Claude CLI command target for one extracted package.
    fn resolve_cli_command<F>(
        &self,
        version: &str,
        cli_root: &Path,
        report_progress: &mut F,
    ) -> Result<ReleasedClaudeCli>
    where
        F: FnMut(&str),
    {
        let manifest = read_package_manifest(cli_root)?;
        let entrypoint = manifest
            .bin
            .get("claude")
            .cloned()
            .unwrap_or_else(|| "cli.js".to_owned());
        let path = cli_root.join(&entrypoint);
        ensure!(
            path.is_file(),
            "extracted Claude package did not contain CLI entrypoint {}",
            path.display()
        );

        let wrapper_path = cli_root.join("cli-wrapper.cjs");
        if entrypoint.ends_with(".exe") && wrapper_path.is_file() {
            let native_binary =
                self.stage_native_cli_dependency(version, cli_root, &manifest, report_progress)?;
            return Ok(ReleasedClaudeCli::NativeBinary(native_binary));
        }

        Ok(ReleasedClaudeCli::NodeScript(path))
    }

    /// Makes the platform-native optional package available to `cli-wrapper.cjs`.
    fn stage_native_cli_dependency<F>(
        &self,
        version: &str,
        cli_root: &Path,
        manifest: &NpmPackageManifest,
        report_progress: &mut F,
    ) -> Result<PathBuf>
    where
        F: FnMut(&str),
    {
        let (package_name, package_version) =
            native_cli_dependency(manifest, &self.runtime.node_platform_suffix)?;
        report_progress(&format!(
            "Preparing native Claude package {package_name}@{package_version} for {version}..."
        ));
        let native_catalog = NpmPackageCatalog::fetch(&package_name, report_progress)?;
        let native_package = native_catalog.version(&package_version).with_context(|| {
            format!("missing npm metadata for `{package_name}@{package_version}`")
        })?;
        let native_root = self.extract_cached_package(
            &package_name,
            &package_version,
            native_package,
            report_progress,
        )?;
        let destination = node_modules_package_path(cli_root, &package_name)?;
        if destination.exists() {
            fs::remove_dir_all(&destination)
                .with_context(|| format!("failed to remove {}", destination.display()))?;
        }
        let parent = destination
            .parent()
            .context("native Claude package destination had no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        fs::rename(&native_root, &destination).with_context(|| {
            format!(
                "failed to stage native Claude package at {}",
                destination.display()
            )
        })?;

        let native_binary =
            destination.join(native_cli_binary_name(&self.runtime.node_platform_suffix));
        #[cfg(unix)]
        mark_executable(&native_binary)?;

        Ok(native_binary)
    }

    /// Runs one released Claude CLI fixture in an isolated temporary project.
    fn run_fixture(
        &self,
        version: &str,
        cli_command: &ReleasedClaudeCli,
        cli_capabilities: ClaudeCliCapabilities,
        fixture: ClaudeAuditFixture,
    ) -> Result<ClaudeFixtureSession> {
        let project_root = self.fixture_run_dir.path().join(format!(
            "fixture-{}-{}-{}",
            sanitize_for_path(version),
            sanitize_for_path(fixture.name),
            unique_suffix()
        ));
        fs::create_dir_all(&project_root)
            .with_context(|| format!("failed to create {}", project_root.display()))?;
        fs::write(
            project_root.join("README.md"),
            "# Audit Fixture\n\nThis file exists so Claude can read a stable heading.\n",
        )
        .with_context(|| {
            format!(
                "failed to write {}",
                project_root.join("README.md").display()
            )
        })?;
        fs::create_dir_all(project_root.join("src"))
            .with_context(|| format!("failed to create {}", project_root.join("src").display()))?;
        fs::write(
            project_root.join("src").join("sample.rs"),
            "/// Returns a stable fixture value.\npub fn sample_value() -> &'static str {\n    \"fixture\"\n}\n",
        )
        .with_context(|| {
            format!(
                "failed to write {}",
                project_root.join("src").join("sample.rs").display()
            )
        })?;

        let settings_path = project_root.join(SETTINGS_FILE_NAME);
        let hook_log_path = project_root.join(HOOK_CAPTURE_FILE_NAME);
        fs::write(
            &settings_path,
            build_hook_settings(&hook_log_path, &self.runtime.hook_python),
        )
        .with_context(|| format!("failed to write {}", settings_path.display()))?;

        let mut command = cli_command.command(&self.runtime, &project_root);
        command.arg("--print");
        command.arg(fixture.prompt);
        command.arg("--verbose");
        if cli_capabilities.supports_output_format {
            command.arg("--output-format");
            command.arg("stream-json");
        }
        if cli_capabilities.supports_max_turns {
            command.arg("--max-turns");
            command.arg(fixture.max_turns.to_string());
        }
        if cli_capabilities.supports_settings {
            command.arg("--settings");
            command.arg(&settings_path);
        }
        if cli_capabilities.supports_add_dir {
            command.arg("--add-dir");
            command.arg(&project_root);
        } else if cli_capabilities.supports_cwd {
            command.arg("--cwd");
            command.arg(&project_root);
        }
        if let Some(flag) = cli_capabilities.allowed_tools_flag
            && !fixture.allowed_tools.is_empty()
        {
            command.arg(flag.as_cli_flag());
            command.arg(fixture.allowed_tools.join(","));
        }
        self.configure_command_environment(&mut command, &project_root)?;
        cli_command.apply_target_environment(&mut command);

        let output =
            run_command_with_timeout(&mut command, CLI_COMMAND_TIMEOUT).with_context(|| {
                format!(
                    "failed to run released Claude CLI fixture `{}` for {}",
                    fixture.name, version
                )
            })?;
        if !output.status.success() {
            bail!(
                "released Claude CLI fixture `{}` for {} failed: {}",
                fixture.name,
                version,
                command_output_summary(&output.stderr)
            );
        }

        let (stream_events, hook_events, transcript_lines) =
            if cli_capabilities.supports_output_format && cli_capabilities.supports_settings {
                let stream_events = parse_json_lines(&output.stdout, "Claude stream-json output")?;
                let hook_events = parse_hook_events(&hook_log_path)?;
                let transcript_path =
                    transcript_path_from_hook_events(&hook_events, command.get_envs())?;
                let transcript_lines = collect_fixture_transcript_lines(&transcript_path)?;
                (stream_events, hook_events, transcript_lines)
            } else {
                let transcript_lines =
                    collect_live_transcript_lines(&project_root, command.get_envs())?;
                (Vec::new(), Vec::new(), transcript_lines)
            };

        Ok(ClaudeFixtureSession {
            transcript_lines,
            hook_events,
            stream_events,
        })
    }

    /// Configures one released Claude CLI command environment for fixture execution.
    fn configure_command_environment(
        &self,
        command: &mut Command,
        project_root: &Path,
    ) -> Result<()> {
        if self.use_host_auth {
            configure_host_auth_environment(command, project_root);
            return Ok(());
        }

        let runtime_root = self
            .scratch_dir
            .path()
            .join(format!("runtime-{}", sanitize_for_path(&unique_suffix())));
        let runtime_home = runtime_root.join("home");
        let runtime_tmp = runtime_root.join("tmp");
        let xdg_config = runtime_home.join(".config");
        let xdg_cache = runtime_home.join(".cache");
        let xdg_data = runtime_home.join(".local").join("share");
        let xdg_state = runtime_home.join(".local").join("state");
        let xdg_runtime = runtime_root.join("run");
        for path in [
            &runtime_root,
            &runtime_home,
            &runtime_tmp,
            &xdg_config,
            &xdg_cache,
            &xdg_data,
            &xdg_state,
            &xdg_runtime,
        ] {
            fs::create_dir_all(path)
                .with_context(|| format!("failed to create {}", path.display()))?;
        }
        command.env_clear();
        for name in ENV_PASSTHROUGH_NAMES {
            if let Some(value) = env::var_os(name) {
                command.env(name, value);
            }
        }
        command.env("HOME", runtime_home);
        command.env("TMPDIR", runtime_tmp.clone());
        command.env("TMP", runtime_tmp.clone());
        command.env("TEMP", runtime_tmp);
        command.env("XDG_CONFIG_HOME", xdg_config);
        command.env("XDG_CACHE_HOME", xdg_cache);
        command.env("XDG_DATA_HOME", xdg_data);
        command.env("XDG_STATE_HOME", xdg_state);
        command.env("XDG_RUNTIME_DIR", xdg_runtime);
        command.env("CLAUDE_CODE_AUDIT_PROJECT_ROOT", project_root);
        Ok(())
    }

    /// Inspects one released Claude CLI using the same configured environment as fixtures.
    fn inspect_cli_capabilities_with_environment(
        &self,
        cli_command: &ReleasedClaudeCli,
        working_dir: &Path,
        project_root: &Path,
    ) -> Result<ClaudeCliCapabilities> {
        let mut command =
            build_cli_capability_probe_command(&self.runtime, cli_command, working_dir);
        self.configure_command_environment(&mut command, project_root)?;
        cli_command.apply_target_environment(&mut command);
        inspect_cli_capabilities(&mut command, cli_command.display_path())
    }
}

/// Configures one released Claude CLI command to use the host login state and auth allowlist.
fn configure_host_auth_environment(command: &mut Command, project_root: &Path) {
    configure_host_auth_environment_from_iter(command, project_root, env::vars_os());
}

/// Configures host-auth execution from one explicit environment iterator for testability.
fn configure_host_auth_environment_from_iter(
    command: &mut Command,
    project_root: &Path,
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) {
    command.env_clear();
    for (name, value) in vars {
        let name_text = name.to_string_lossy();
        if ENV_PASSTHROUGH_NAMES.contains(&name_text.as_ref())
            || HOST_RUNTIME_ENV_NAMES.contains(&name_text.as_ref())
            || AUTH_ENV_NAMES.contains(&name_text.as_ref())
            || AUTH_ENV_PREFIXES
                .iter()
                .any(|prefix| name_text.starts_with(prefix))
        {
            command.env(&name, &value);
        }
    }
    command.env("CLAUDE_CODE_AUDIT_PROJECT_ROOT", project_root);
}

impl ClaudeSchemaAuditProvider for NpmClaudeSchemaAuditProvider {
    fn list_release_versions(&self) -> Result<Vec<String>> {
        Ok(self.cli_catalog.versions.keys().cloned().collect())
    }

    fn uses_host_auth(&self) -> bool {
        self.use_host_auth
    }

    fn collect_snapshot<F>(
        &self,
        version: &str,
        report_progress: &mut F,
    ) -> Result<ClaudeAuditSnapshot>
    where
        F: FnMut(&str),
    {
        let cli_package = self.cli_package(version)?;
        let cli_root = self.extract_cached_package(
            NPM_CLAUDE_CODE_PACKAGE,
            version,
            cli_package,
            report_progress,
        )?;
        Ok(ClaudeAuditSnapshot {
            transcript_manifest: self.collect_transcript_manifest(
                version,
                &cli_root,
                report_progress,
            )?,
            sdk_manifest: self.collect_sdk_manifest(version, &cli_root, report_progress)?,
        })
    }
}

/// Stores one merged transcript fixture session produced by the released Claude CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaudeFixtureSession {
    transcript_lines: Vec<Map<String, Value>>,
    hook_events: Vec<Map<String, Value>>,
    stream_events: Vec<Map<String, Value>>,
}

/// Stores one normalized Claude transcript manifest derived from released fixtures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ClaudeTranscriptSchemaManifest {
    fixture_names: Vec<String>,
    line_types: Vec<String>,
    user_content_types: Vec<String>,
    assistant_content_types: Vec<String>,
    assistant_stop_reasons: Vec<String>,
    progress_types: Vec<String>,
    system_subtypes: Vec<String>,
    tool_names: Vec<String>,
    hook_event_names: Vec<String>,
    stream_event_types: Vec<String>,
    stream_event_subtypes: Vec<String>,
    top_level_keys_by_type: BTreeMap<String, Vec<String>>,
}

/// Stores one normalized supplementary SDK manifest derived from published `.d.ts` files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct ClaudeSdkSchemaManifest {
    agent_sdk_version: Option<String>,
    cli_tool_input_schemas: Vec<String>,
    cli_tool_output_schemas: Vec<String>,
    agent_sdk_message_variants: Vec<String>,
    agent_sdk_hook_events: Vec<String>,
}

/// Stores the selected sparse-sampling plan for one contiguous Claude audit range.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaudeSamplingPlan {
    audited_versions_asc: Vec<StableClaudeReleaseVersion>,
    inspected_versions_desc: Vec<StableClaudeReleaseVersion>,
    inspected_ascending_indices: Vec<usize>,
    assumed_compatible_intervals: Vec<String>,
    sample_stride: usize,
}

/// Tracks the actual and baseline-compatible versions inspected during refine mode.
struct RefinementIndexSets<'a> {
    inspected_ascending_indices: &'a mut BTreeSet<usize>,
    baseline_compatible_ascending_indices: &'a mut BTreeSet<usize>,
}

/// Stores the CLI feature flags available for one released Claude package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClaudeCliCapabilities {
    supports_output_format: bool,
    supports_settings: bool,
    supports_add_dir: bool,
    allowed_tools_flag: Option<ClaudeAllowedToolsFlag>,
    supports_max_turns: bool,
    supports_cwd: bool,
}

/// Stores the accepted spelling of Claude's allowed-tools flag for one release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeAllowedToolsFlag {
    CamelCase,
    Hyphenated,
}

impl ClaudeAllowedToolsFlag {
    /// Returns the exact CLI flag spelling supported by one released Claude build.
    fn as_cli_flag(self) -> &'static str {
        match self {
            Self::CamelCase => "--allowedTools",
            Self::Hyphenated => "--allowed-tools",
        }
    }
}

/// Accumulates one transcript manifest while Claude fixtures are recorded.
#[derive(Debug, Default)]
struct TranscriptManifestBuilder {
    fixture_names: BTreeSet<String>,
    line_types: BTreeSet<String>,
    user_content_types: BTreeSet<String>,
    assistant_content_types: BTreeSet<String>,
    assistant_stop_reasons: BTreeSet<String>,
    progress_types: BTreeSet<String>,
    system_subtypes: BTreeSet<String>,
    tool_names: BTreeSet<String>,
    hook_event_names: BTreeSet<String>,
    stream_event_types: BTreeSet<String>,
    stream_event_subtypes: BTreeSet<String>,
    top_level_keys_by_type: BTreeMap<String, BTreeSet<String>>,
}

impl TranscriptManifestBuilder {
    /// Records one released fixture session into the manifest builder.
    fn record_fixture(&mut self, session: &ClaudeFixtureSession, fixture_name: &str) -> Result<()> {
        self.fixture_names.insert(fixture_name.to_owned());
        for line in &session.transcript_lines {
            self.record_transcript_line(line)?;
        }
        for event in &session.hook_events {
            if let Some(event_name) = event.get("hook_event_name").and_then(Value::as_str) {
                self.hook_event_names.insert(event_name.to_owned());
            }
        }
        for event in &session.stream_events {
            if let Some(event_type) = event.get("type").and_then(Value::as_str) {
                self.stream_event_types.insert(event_type.to_owned());
            }
            if let Some(subtype) = event.get("subtype").and_then(Value::as_str) {
                self.stream_event_subtypes.insert(subtype.to_owned());
            }
        }
        Ok(())
    }

    /// Records one raw transcript line into the manifest builder.
    fn record_transcript_line(&mut self, line: &Map<String, Value>) -> Result<()> {
        let line_type = line
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.line_types.insert(line_type.clone());
        self.top_level_keys_by_type
            .entry(line_type.clone())
            .or_default()
            .extend(line.keys().cloned());

        match line_type.as_str() {
            "user" => record_user_shape(line, &mut self.user_content_types)?,
            "assistant" => record_assistant_shape(
                line,
                &mut self.assistant_content_types,
                &mut self.assistant_stop_reasons,
                &mut self.tool_names,
            )?,
            "progress" => {
                if let Some(kind) = line
                    .get("data")
                    .and_then(Value::as_object)
                    .and_then(|data| data.get("type").and_then(Value::as_str))
                {
                    self.progress_types.insert(kind.to_owned());
                }
            }
            "system" => {
                if let Some(subtype) = line.get("subtype").and_then(Value::as_str) {
                    self.system_subtypes.insert(subtype.to_owned());
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// Finalizes one transcript manifest after all fixture sessions are recorded.
    fn finish(self) -> ClaudeTranscriptSchemaManifest {
        ClaudeTranscriptSchemaManifest {
            fixture_names: self.fixture_names.into_iter().collect(),
            line_types: self.line_types.into_iter().collect(),
            user_content_types: self.user_content_types.into_iter().collect(),
            assistant_content_types: self.assistant_content_types.into_iter().collect(),
            assistant_stop_reasons: self.assistant_stop_reasons.into_iter().collect(),
            progress_types: self.progress_types.into_iter().collect(),
            system_subtypes: self.system_subtypes.into_iter().collect(),
            tool_names: self.tool_names.into_iter().collect(),
            hook_event_names: self.hook_event_names.into_iter().collect(),
            stream_event_types: self.stream_event_types.into_iter().collect(),
            stream_event_subtypes: self.stream_event_subtypes.into_iter().collect(),
            top_level_keys_by_type: self
                .top_level_keys_by_type
                .into_iter()
                .map(|(line_type, keys)| (line_type, keys.into_iter().collect()))
                .collect(),
        }
    }
}

/// Returns the built-in Claude fixture suite used for transcript audit coverage.
fn audit_fixtures() -> &'static [ClaudeAuditFixture] {
    &[
        ClaudeAuditFixture {
            name: "plain_answer",
            prompt: "Reply with exactly READY and do not use any tools.",
            allowed_tools: &[],
            required_tools: &[],
            max_turns: 2,
            require_subagent_signal: false,
        },
        ClaudeAuditFixture {
            name: "read_tool",
            prompt: "Use the Read tool exactly once on README.md and then reply with only the first markdown heading.",
            allowed_tools: &["Read"],
            required_tools: &["Read"],
            max_turns: 4,
            require_subagent_signal: false,
        },
        ClaudeAuditFixture {
            name: "subagent_task",
            prompt: "You must delegate this work with exactly one Agent tool call. Do not use Read yourself. Ask the subagent to inspect README.md and return the first markdown heading, then reply with only that heading.",
            allowed_tools: &["Agent"],
            required_tools: &["Agent"],
            max_turns: 6,
            require_subagent_signal: true,
        },
    ]
}

/// Executes the audit against an abstracted Claude provider.
#[cfg(test)]
fn run_claude_schema_audit_with_provider<P: ClaudeSchemaAuditProvider>(
    release_source: String,
    binary_cache_dir: PathBuf,
    provider: &P,
    sample_stride: usize,
    from_version: Option<String>,
    survey_mode: ClaudeSchemaSurveyMode,
) -> Result<ClaudeSchemaAuditReport> {
    let mut noop = |_: &str| {};
    run_claude_schema_audit_with_provider_and_progress(
        release_source,
        binary_cache_dir,
        provider,
        sample_stride,
        from_version,
        survey_mode,
        &mut noop,
    )
}

/// Executes the audit against an abstracted Claude provider with progress reporting.
fn run_claude_schema_audit_with_provider_and_progress<P, F>(
    release_source: String,
    binary_cache_dir: PathBuf,
    provider: &P,
    sample_stride: usize,
    from_version: Option<String>,
    survey_mode: ClaudeSchemaSurveyMode,
    report_progress: &mut F,
) -> Result<ClaudeSchemaAuditReport>
where
    P: ClaudeSchemaAuditProvider,
    F: FnMut(&str),
{
    report_progress("Listing published Claude Code versions...");
    let stable_versions = collect_stable_release_versions(provider.list_release_versions()?);
    report_progress(&format!(
        "Found {} stable Claude Code version(s).",
        stable_versions.len()
    ));
    let audited_versions =
        select_audited_release_versions(&stable_versions, from_version.as_deref())?;
    let latest_version = audited_versions
        .first()
        .context("selected audit range is unexpectedly empty")?;
    let baseline_version = audited_versions
        .last()
        .context("selected audit range is unexpectedly empty")?;
    let sampling_plan = build_sampling_plan(&audited_versions, sample_stride);
    report_progress(&format!(
        "Auditing {} Claude version(s) from {} down to {}.",
        audited_versions.len(),
        latest_version.raw,
        baseline_version.raw
    ));
    if sampling_plan.sample_stride > 1 {
        report_progress(&format!(
            "Sampling every {} version(s): directly inspecting {} version(s) and assuming {} unsampled interval(s) are schema-stable unless sampled drift is detected.",
            sampling_plan.sample_stride,
            sampling_plan.inspected_versions_desc.len(),
            sampling_plan.assumed_compatible_intervals.len()
        ));
    }

    report_progress(&format!(
        "Collecting baseline Claude transcript manifest from {}...",
        baseline_version.raw
    ));
    let baseline_snapshot = provider.collect_snapshot(&baseline_version.raw, report_progress)?;
    let baseline_transcript = normalize_json(serde_json::to_value(
        &baseline_snapshot.transcript_manifest,
    )?);
    let baseline_sdk = normalize_json(serde_json::to_value(&baseline_snapshot.sdk_manifest)?);
    let mut transcript_drift = None;
    let mut transcript_drift_windows = Vec::new();
    let mut sdk_drift = None;
    let total_comparisons = sampling_plan
        .inspected_versions_desc
        .len()
        .saturating_sub(1);
    let baseline_transcript_value = baseline_transcript.clone();
    let mut epoch_baseline_transcript = baseline_transcript.clone();
    let mut baseline_compatible_ascending_indices = BTreeSet::from([0usize]);
    let mut last_sampled_compatible_asc_index = 0usize;
    let mut inspected_ascending_indices = sampling_plan
        .inspected_ascending_indices
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();

    for (index, sampled_asc_index) in sampling_plan
        .inspected_ascending_indices
        .iter()
        .copied()
        .skip(1)
        .enumerate()
    {
        let version = &sampling_plan.audited_versions_asc[sampled_asc_index];
        report_progress(&format!(
            "Comparing {} against baseline ({}/{})...",
            version.raw,
            index + 1,
            total_comparisons
        ));
        let snapshot = provider.collect_snapshot(&version.raw, report_progress)?;
        let transcript = normalize_json(serde_json::to_value(&snapshot.transcript_manifest)?);
        if transcript == baseline_transcript_value {
            baseline_compatible_ascending_indices.insert(sampled_asc_index);
        }
        if transcript != epoch_baseline_transcript {
            let drift_window = ClaudeSchemaDriftWindow {
                window_start_version: sampling_plan.audited_versions_asc
                    [last_sampled_compatible_asc_index + 1]
                    .raw
                    .clone(),
                window_end_version: version.raw.clone(),
                sampled_compatible_version: sampling_plan.audited_versions_asc
                    [last_sampled_compatible_asc_index]
                    .raw
                    .clone(),
                sampled_drift_version: version.raw.clone(),
                difference_summary: summarize_schema_differences(
                    &epoch_baseline_transcript,
                    &transcript,
                ),
            };
            transcript_drift_windows.push(drift_window.clone());

            if transcript_drift.is_none() {
                let first_drift = if survey_mode == ClaudeSchemaSurveyMode::Refine
                    && sampled_asc_index > last_sampled_compatible_asc_index + 1
                {
                    report_progress(&format!(
                        "Refining sampled drift window {} ..= {} to find the first drifting version...",
                        drift_window.window_start_version, drift_window.window_end_version
                    ));
                    find_first_transcript_drift_in_interval(
                        provider,
                        &sampling_plan.audited_versions_asc,
                        last_sampled_compatible_asc_index + 1,
                        sampled_asc_index,
                        &baseline_transcript_value,
                        &mut RefinementIndexSets {
                            inspected_ascending_indices: &mut inspected_ascending_indices,
                            baseline_compatible_ascending_indices:
                                &mut baseline_compatible_ascending_indices,
                        },
                        report_progress,
                    )?
                } else if survey_mode == ClaudeSchemaSurveyMode::Coarse {
                    ClaudeSchemaDrift {
                        first_drift_version: drift_window.sampled_drift_version.clone(),
                        difference_summary: drift_window.difference_summary.clone(),
                        likely_files_to_update: LIKELY_UPDATE_PATHS
                            .iter()
                            .map(|path| (*path).to_owned())
                            .collect(),
                    }
                } else {
                    ClaudeSchemaDrift {
                        first_drift_version: version.raw.clone(),
                        difference_summary: summarize_schema_differences(
                            &baseline_transcript,
                            &transcript,
                        ),
                        likely_files_to_update: LIKELY_UPDATE_PATHS
                            .iter()
                            .map(|path| (*path).to_owned())
                            .collect(),
                    }
                };
                report_progress(&format!(
                    "Detected Claude transcript drift at {}.",
                    first_drift.first_drift_version
                ));
                transcript_drift = Some(first_drift);
            }

            epoch_baseline_transcript = transcript;
            last_sampled_compatible_asc_index = sampled_asc_index;
        } else {
            last_sampled_compatible_asc_index = sampled_asc_index;
        }

        if survey_mode == ClaudeSchemaSurveyMode::Refine {
            let sdk = normalize_json(serde_json::to_value(&snapshot.sdk_manifest)?);
            if sdk_drift.is_none() && sdk != baseline_sdk {
                sdk_drift = Some(ClaudeSdkSchemaDrift {
                    first_drift_version: version.raw.clone(),
                    difference_summary: summarize_schema_differences(&baseline_sdk, &sdk),
                });
            }

            if transcript_drift.is_some() && sdk_drift.is_some() {
                break;
            }
        }
    }

    if transcript_drift.is_none() {
        report_progress(&format!(
            "No Claude transcript drift detected across {} audited version(s).",
            audited_versions.len()
        ));
    }

    Ok(ClaudeSchemaAuditReport {
        release_source,
        binary_cache_dir,
        latest_published_version: latest_version.raw.clone(),
        latest_exact_covered_version: latest_exact_supported_claude_cli_version().to_string(),
        audited_versions: audited_versions
            .into_iter()
            .map(|version| version.raw)
            .collect(),
        inspected_versions: inspected_ascending_indices
            .iter()
            .copied()
            .rev()
            .map(|index| sampling_plan.audited_versions_asc[index].raw.clone())
            .collect(),
        assumed_compatible_intervals: build_assumed_compatible_intervals(
            &sampling_plan.audited_versions_asc,
            &baseline_compatible_ascending_indices,
        ),
        sample_stride: sampling_plan.sample_stride,
        used_host_auth: provider.uses_host_auth(),
        survey_mode,
        transcript_drift_windows,
        outcome: transcript_drift
            .map(ClaudeSchemaAuditOutcome::Drift)
            .unwrap_or(ClaudeSchemaAuditOutcome::Compatible),
        supplementary_sdk_drift: sdk_drift,
    })
}

/// Stores one parsed stable Claude version with its sortable components.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StableClaudeReleaseVersion {
    raw: String,
    version: ClaudeCliVersion,
}

/// Parses and sorts the stable Claude versions that matter to the audit.
fn collect_stable_release_versions(versions: Vec<String>) -> Vec<StableClaudeReleaseVersion> {
    let mut stable = versions
        .into_iter()
        .filter_map(|version| parse_stable_release_version(&version))
        .collect::<Vec<_>>();
    stable.sort_by(|left, right| {
        right
            .version
            .cmp(&left.version)
            .then_with(|| left.raw.cmp(&right.raw))
    });
    stable.dedup_by(|left, right| left.raw == right.raw);
    stable
}

/// Parses one raw published version into a stable Claude release version.
fn parse_stable_release_version(version: &str) -> Option<StableClaudeReleaseVersion> {
    let parsed = ClaudeCliVersion::parse(version).ok()?;
    parsed.is_stable().then_some(StableClaudeReleaseVersion {
        raw: version.to_owned(),
        version: parsed,
    })
}

/// Selects the audited Claude version range from latest published down to darc's cutoff.
fn select_audited_release_versions(
    stable_versions: &[StableClaudeReleaseVersion],
    from_version: Option<&str>,
) -> Result<Vec<StableClaudeReleaseVersion>> {
    ensure!(
        !stable_versions.is_empty(),
        "no stable Claude Code versions were available from the npm registry"
    );

    let floor_version = match from_version {
        Some(version) => ClaudeCliVersion::parse(version)
            .with_context(|| format!("invalid Claude floor version `{version}`"))?,
        None => latest_exact_supported_claude_cli_version(),
    };
    ensure!(
        floor_version.is_stable(),
        "Claude schema audit floor version `{floor_version}` is not a stable release"
    );

    let exact_index = stable_versions
        .iter()
        .position(|version| version.version == floor_version)
        .with_context(|| {
            format!(
                "npm registry metadata is missing the stable Claude Code version `{floor_version}` required by the selected audit floor"
            )
        })?;

    Ok(stable_versions[..=exact_index].to_vec())
}

/// Builds one sparse sampling plan across the full contiguous Claude audit range.
fn build_sampling_plan(
    audited_versions_desc: &[StableClaudeReleaseVersion],
    sample_stride: usize,
) -> ClaudeSamplingPlan {
    let audited_versions_asc = audited_versions_desc
        .iter()
        .cloned()
        .rev()
        .collect::<Vec<_>>();
    let mut inspected_ascending_indices = Vec::new();
    let len = audited_versions_asc.len();
    if len > 0 {
        inspected_ascending_indices.push(0);
        let mut next = sample_stride.max(1);
        while next < len.saturating_sub(1) {
            inspected_ascending_indices.push(next);
            next += sample_stride.max(1);
        }
        if inspected_ascending_indices.last().copied() != Some(len - 1) {
            inspected_ascending_indices.push(len - 1);
        }
    }

    let mut assumed_compatible_intervals = Vec::new();
    for window in inspected_ascending_indices.windows(2) {
        let left = window[0];
        let right = window[1];
        if right > left + 1 {
            assumed_compatible_intervals.push(format_version_interval(
                &audited_versions_asc[left + 1].raw,
                &audited_versions_asc[right - 1].raw,
            ));
        }
    }

    let inspected_versions_desc = inspected_ascending_indices
        .iter()
        .copied()
        .rev()
        .map(|index| audited_versions_asc[index].clone())
        .collect();

    ClaudeSamplingPlan {
        audited_versions_asc,
        inspected_versions_desc,
        inspected_ascending_indices,
        assumed_compatible_intervals,
        sample_stride: sample_stride.max(1),
    }
}

/// Finds the first transcript drift inside one sampled interval by walking the narrowed window.
fn find_first_transcript_drift_in_interval<P, F>(
    provider: &P,
    audited_versions_asc: &[StableClaudeReleaseVersion],
    start_index: usize,
    end_index: usize,
    baseline_transcript: &Value,
    index_sets: &mut RefinementIndexSets<'_>,
    report_progress: &mut F,
) -> Result<ClaudeSchemaDrift>
where
    P: ClaudeSchemaAuditProvider,
    F: FnMut(&str),
{
    for (index, version) in audited_versions_asc[start_index..=end_index]
        .iter()
        .enumerate()
    {
        index_sets
            .inspected_ascending_indices
            .insert(start_index + index);
        report_progress(&format!(
            "Inspecting {} inside the sampled drift window...",
            version.raw
        ));
        let snapshot = provider.collect_snapshot(&version.raw, report_progress)?;
        let transcript = normalize_json(serde_json::to_value(&snapshot.transcript_manifest)?);
        if transcript == *baseline_transcript {
            index_sets
                .baseline_compatible_ascending_indices
                .insert(start_index + index);
        }
        if transcript != *baseline_transcript {
            return Ok(ClaudeSchemaDrift {
                first_drift_version: version.raw.clone(),
                difference_summary: summarize_schema_differences(baseline_transcript, &transcript),
                likely_files_to_update: LIKELY_UPDATE_PATHS
                    .iter()
                    .map(|path| (*path).to_owned())
                    .collect(),
            });
        }
    }

    bail!("sampled drift window unexpectedly contained no drifting Claude version")
}

/// Formats one Claude version interval compactly for sampling summaries.
fn format_version_interval(first: &str, last: &str) -> String {
    if first == last {
        first.to_owned()
    } else {
        format!("{first} ..= {last}")
    }
}

/// Builds the remaining assumed-compatible gaps from the final inspected index set.
fn build_assumed_compatible_intervals(
    audited_versions_asc: &[StableClaudeReleaseVersion],
    inspected_ascending_indices: &BTreeSet<usize>,
) -> Vec<String> {
    let inspected = inspected_ascending_indices
        .iter()
        .copied()
        .collect::<Vec<_>>();
    let mut intervals = Vec::new();
    for window in inspected.windows(2) {
        let left = window[0];
        let right = window[1];
        if right > left + 1 {
            intervals.push(format_version_interval(
                &audited_versions_asc[left + 1].raw,
                &audited_versions_asc[right - 1].raw,
            ));
        }
    }
    intervals
}

/// Records the shape of one Claude `user` transcript line.
fn record_user_shape(
    object: &Map<String, Value>,
    user_content_types: &mut BTreeSet<String>,
) -> Result<()> {
    let Some(message) = object.get("message").and_then(Value::as_object) else {
        return Ok(());
    };
    let content = message.get("content");
    match content {
        Some(Value::String(_)) => {
            user_content_types.insert("text".to_owned());
        }
        Some(Value::Array(items)) => {
            for item in items {
                let Some(item_object) = item.as_object() else {
                    continue;
                };
                let item_type = item_object
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !item_type.is_empty() {
                    user_content_types.insert(item_type.to_owned());
                }
            }
        }
        Some(other) => {
            user_content_types.insert(format!("unknown:{}", type_name(other)));
        }
        None => {}
    }
    Ok(())
}

/// Records the shape of one Claude `assistant` transcript line.
fn record_assistant_shape(
    object: &Map<String, Value>,
    assistant_content_types: &mut BTreeSet<String>,
    assistant_stop_reasons: &mut BTreeSet<String>,
    tool_names: &mut BTreeSet<String>,
) -> Result<()> {
    let Some(message) = object.get("message").and_then(Value::as_object) else {
        return Ok(());
    };
    if let Some(stop_reason) = message.get("stop_reason").and_then(Value::as_str) {
        assistant_stop_reasons.insert(stop_reason.to_owned());
    }
    let Some(items) = message.get("content").and_then(Value::as_array) else {
        return Ok(());
    };
    for item in items {
        let Some(item_object) = item.as_object() else {
            continue;
        };
        let item_type = item_object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !item_type.is_empty() {
            assistant_content_types.insert(item_type.to_owned());
        }
        if item_type == "tool_use"
            && let Some(name) = item_object.get("name").and_then(Value::as_str)
        {
            tool_names.insert(name.to_owned());
        }
    }
    Ok(())
}

/// Validates that one fixture actually exercised the expected transcript surfaces.
fn validate_fixture_coverage(
    session: &ClaudeFixtureSession,
    fixture: ClaudeAuditFixture,
) -> Result<()> {
    let mut observed_tools = BTreeSet::new();
    let mut hook_event_names = BTreeSet::new();
    for line in &session.transcript_lines {
        if let Some(message) = line.get("message").and_then(Value::as_object)
            && let Some(items) = message.get("content").and_then(Value::as_array)
        {
            for item in items {
                let Some(item_object) = item.as_object() else {
                    continue;
                };
                if item_object.get("type").and_then(Value::as_str) == Some("tool_use")
                    && let Some(name) = item_object.get("name").and_then(Value::as_str)
                {
                    observed_tools.insert(name.to_owned());
                }
            }
        }
    }
    for event in &session.hook_events {
        if let Some(name) = event.get("hook_event_name").and_then(Value::as_str) {
            hook_event_names.insert(name.to_owned());
        }
    }

    for required in fixture.required_tools {
        let satisfied = observed_tools
            .iter()
            .any(|observed| claude_tool_name_matches(observed, required));
        ensure!(
            satisfied,
            "fixture `{}` did not trigger required Claude tool `{required}`",
            fixture.name
        );
    }
    if fixture.require_subagent_signal {
        ensure!(
            hook_event_names.contains("SubagentStop")
                || session
                    .transcript_lines
                    .iter()
                    .any(has_subagent_artifact_line),
            "fixture `{}` did not produce any observable subagent signal",
            fixture.name
        );
    }
    Ok(())
}

/// Returns whether two Claude tool names are equivalent across Task/Agent rename windows.
fn claude_tool_name_matches(observed: &str, expected: &str) -> bool {
    observed == expected || matches!((observed, expected), ("Task", "Agent") | ("Agent", "Task"))
}

/// Returns whether one transcript line came from a real subagent artifact rather than the parent.
fn has_subagent_artifact_line(line: &Map<String, Value>) -> bool {
    line.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || line.get("agentId").and_then(Value::as_str).is_some()
}

/// Collects the optional CLI-embedded SDK type surface without failing the audit.
fn collect_cli_sdk_manifest<F>(
    cli_root: &Path,
    version: &str,
    report_progress: &mut F,
) -> ClaudeSdkSchemaManifest
where
    F: FnMut(&str),
{
    let sdk_tools_path = cli_root.join("sdk-tools.d.ts");
    let sdk_tools = match fs::read_to_string(&sdk_tools_path) {
        Ok(sdk_tools) => sdk_tools,
        Err(error) => {
            report_progress(&format!(
                "Skipping embedded CLI SDK types for Claude Code {version} because {} could not be read: {error}",
                sdk_tools_path.display()
            ));
            return ClaudeSdkSchemaManifest::default();
        }
    };

    ClaudeSdkSchemaManifest {
        cli_tool_input_schemas: collect_type_union_members(
            &sdk_tools,
            "export type ToolInputSchemas =",
        ),
        cli_tool_output_schemas: collect_type_union_members(
            &sdk_tools,
            "export type ToolOutputSchemas =",
        ),
        ..ClaudeSdkSchemaManifest::default()
    }
}

/// Reads the npm package manifest inside one extracted package.
fn read_package_manifest(cli_root: &Path) -> Result<NpmPackageManifest> {
    let package_json_path = cli_root.join("package.json");
    let manifest_text = fs::read_to_string(&package_json_path)
        .with_context(|| format!("failed to read {}", package_json_path.display()))?;
    serde_json::from_str(&manifest_text)
        .with_context(|| format!("failed to parse {}", package_json_path.display()))
}

/// Returns the native optional dependency selected by the Node wrapper runtime.
fn native_cli_dependency(
    manifest: &NpmPackageManifest,
    platform_suffix: &str,
) -> Result<(String, String)> {
    let package_name = format!("{NPM_CLAUDE_CODE_PACKAGE}-{platform_suffix}");
    let package_version = manifest
        .optional_dependencies
        .get(&package_name)
        .with_context(|| {
            format!("Claude package wrapper did not declare optional dependency `{package_name}`")
        })?;
    Ok((package_name, package_version.clone()))
}

/// Returns the platform suffix that `cli-wrapper.cjs` will use under one Node binary.
fn detect_node_native_cli_platform_suffix(node_binary: &Path) -> Result<String> {
    let output = Command::new(node_binary)
        .arg("-e")
        .arg(NODE_PLATFORM_SUFFIX_SCRIPT)
        .output()
        .with_context(|| {
            format!(
                "failed to run {} for platform detection",
                node_binary.display()
            )
        })?;
    ensure!(
        output.status.success(),
        "{} failed platform detection: {}",
        node_binary.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let suffix = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    ensure!(
        matches!(
            suffix.as_str(),
            "darwin-arm64"
                | "darwin-x64"
                | "linux-arm64"
                | "linux-arm64-musl"
                | "linux-x64"
                | "linux-x64-musl"
                | "win32-arm64"
                | "win32-x64"
        ),
        "unsupported Node platform suffix `{suffix}` for Claude native packages"
    );
    Ok(suffix)
}

const NODE_PLATFORM_SUFFIX_SCRIPT: &str = r#"
const { spawnSync } = require('child_process');
const { arch } = require('os');

function detectMusl() {
  if (process.platform !== 'linux') {
    return false;
  }
  const report =
    typeof process.report?.getReport === 'function'
      ? process.report.getReport()
      : null;
  return report != null && report.header?.glibcVersionRuntime === undefined;
}

let cpu = arch();
if (process.platform === 'linux') {
  console.log('linux-' + cpu + (detectMusl() ? '-musl' : ''));
} else {
  if (process.platform === 'darwin' && cpu === 'x64') {
    const r = spawnSync('sysctl', ['-n', 'sysctl.proc_translated'], {
      encoding: 'utf8',
    });
    if (r.stdout?.trim() === '1') {
      cpu = 'arm64';
    }
  }
  console.log(process.platform + '-' + cpu);
}
"#;

/// Returns the package installation path under an extracted package-local `node_modules`.
fn node_modules_package_path(cli_root: &Path, package_name: &str) -> Result<PathBuf> {
    let mut parts = package_name.split('/');
    let scope = parts
        .next()
        .filter(|part| part.starts_with('@'))
        .with_context(|| format!("unsupported scoped npm package name `{package_name}`"))?;
    let name = parts
        .next()
        .filter(|part| !part.is_empty())
        .with_context(|| format!("unsupported scoped npm package name `{package_name}`"))?;
    ensure!(
        parts.next().is_none(),
        "unsupported scoped npm package name `{package_name}`"
    );
    Ok(cli_root.join("node_modules").join(scope).join(name))
}

/// Returns the native Claude binary name for one Node wrapper platform suffix.
fn native_cli_binary_name(platform_suffix: &str) -> &'static str {
    if platform_suffix.starts_with("win32-") {
        "claude.exe"
    } else {
        "claude"
    }
}

/// Marks one extracted package file executable on Unix hosts.
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

/// Builds the released Claude CLI command used for one capability probe.
fn build_cli_capability_probe_command(
    runtime: &AuditRuntime,
    cli_command: &ReleasedClaudeCli,
    working_dir: &Path,
) -> Command {
    let mut command = cli_command.command(runtime, working_dir);
    command.arg("--help");
    command
}

/// Inspects one released Claude CLI for the flags needed by the audit harness.
fn inspect_cli_capabilities(
    command: &mut Command,
    cli_entrypoint: &Path,
) -> Result<ClaudeCliCapabilities> {
    let output = run_command_with_timeout(command, CLI_COMMAND_TIMEOUT)
        .with_context(|| format!("failed to inspect {}", cli_entrypoint.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(ClaudeCliCapabilities {
        supports_output_format: stdout.contains("--output-format"),
        supports_settings: stdout.contains("--settings"),
        supports_add_dir: stdout.contains("--add-dir"),
        allowed_tools_flag: detect_allowed_tools_flag(&stdout),
        supports_max_turns: stdout.contains("--max-turns"),
        supports_cwd: stdout.contains("--cwd"),
    })
}

/// Detects which allowed-tools flag spelling one Claude help screen advertises.
fn detect_allowed_tools_flag(help_text: &str) -> Option<ClaudeAllowedToolsFlag> {
    if help_text.contains("--allowedTools") {
        Some(ClaudeAllowedToolsFlag::CamelCase)
    } else if help_text.contains("--allowed-tools") {
        Some(ClaudeAllowedToolsFlag::Hyphenated)
    } else {
        None
    }
}

/// Builds one hook settings document that captures lifecycle JSON into one local file.
fn build_hook_settings(hook_log_path: &Path, hook_python: &Path) -> String {
    let command = hook_capture_command(hook_log_path, hook_python);
    json!({
        "hooks": {
            "SessionStart": [{
                "hooks": [{ "type": "command", "command": command }]
            }],
            "SessionEnd": [{
                "hooks": [{ "type": "command", "command": command }]
            }],
            "PreToolUse": [{
                "matcher": "*",
                "hooks": [{ "type": "command", "command": command }]
            }],
            "PostToolUse": [{
                "matcher": "*",
                "hooks": [{ "type": "command", "command": command }]
            }],
            "SubagentStop": [{
                "hooks": [{ "type": "command", "command": command }]
            }]
        }
    })
    .to_string()
}

/// Builds one portable-enough hook capture command for the local platform.
fn hook_capture_command(hook_log_path: &Path, hook_python: &Path) -> String {
    format!(
        "{} -c {} {}",
        shell_quote(&hook_python.to_string_lossy()),
        shell_quote(
            "import os, pathlib, sys; path = pathlib.Path(sys.argv[1]); path.parent.mkdir(parents=True, exist_ok=True); data = sys.stdin.buffer.read() + b'\\n'; fd = os.open(path, os.O_APPEND | os.O_CREAT | os.O_WRONLY, 0o666); os.write(fd, data); os.close(fd)"
        ),
        shell_quote(&hook_log_path.to_string_lossy())
    )
}

/// Resolves one runtime binary from a list of candidate names.
fn resolve_runtime_binary(candidates: &[&str]) -> Result<PathBuf> {
    for candidate in candidates {
        let mut command = Command::new(candidate);
        command.arg("--version");
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        if command.status().is_ok_and(|status| status.success()) {
            return Ok(PathBuf::from(candidate));
        }
    }
    bail!(
        "missing required runtime binary from candidates: {}",
        candidates.join(", ")
    )
}

/// Parses one npm registry integrity string.
fn parse_package_integrity(
    integrity: Option<&str>,
    package_name: &str,
    package_version: &str,
) -> Result<PackageIntegrity> {
    let integrity = integrity.with_context(|| {
        format!("npm package `{package_name}@{package_version}` did not include an integrity field")
    })?;
    let (algorithm, base64_digest) = integrity.split_once('-').with_context(|| {
        format!(
            "npm package `{package_name}@{package_version}` returned unsupported integrity `{integrity}`"
        )
    })?;
    let algorithm = match algorithm {
        "sha512" => IntegrityAlgorithm::Sha512,
        other => bail!(
            "npm package `{package_name}@{package_version}` returned unsupported integrity algorithm `{other}`"
        ),
    };
    Ok(PackageIntegrity {
        algorithm,
        base64_digest: base64_digest.to_owned(),
    })
}

impl PackageIntegrity {
    /// Returns the stable cache key fragment for one parsed integrity value.
    fn cache_key(&self) -> String {
        format!(
            "{}-{}",
            match self.algorithm {
                IntegrityAlgorithm::Sha512 => "sha512",
            },
            sanitize_for_path(&self.base64_digest)
        )
    }
}

/// Returns the final file name segment from one npm tarball URL.
fn tarball_file_name(tarball_url: &str) -> Result<String> {
    tarball_url
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("npm tarball URL `{tarball_url}` did not include a file name"))
}

/// Verifies one tarball file against its expected npm integrity value.
fn verify_file_integrity(path: &Path, expected_integrity: &PackageIntegrity) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut buffer = [0_u8; 8192];
    let mut digest = Sha512::new();
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }

    let actual = base64_encode(digest.finalize().as_slice());
    ensure!(
        actual == expected_integrity.base64_digest,
        "integrity mismatch for {}: expected {}-{}, got {}-{}",
        path.display(),
        match expected_integrity.algorithm {
            IntegrityAlgorithm::Sha512 => "sha512",
        },
        expected_integrity.base64_digest,
        match expected_integrity.algorithm {
            IntegrityAlgorithm::Sha512 => "sha512",
        },
        actual
    );
    Ok(())
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

/// Builds one HTTP client for npm metadata and tarball downloads.
fn build_http_client() -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&format!("darc/{}", env!("CARGO_PKG_VERSION")))
            .context("failed to build npm registry user agent header")?,
    );

    Client::builder()
        .default_headers(headers)
        .build()
        .context("failed to build HTTP client for Claude schema audit")
}

/// Resolves the default binary cache directory for released Claude packages.
fn resolve_binary_cache_dir(cache_dir: Option<&Path>) -> Result<PathBuf> {
    if let Some(cache_dir) = cache_dir {
        return Ok(cache_dir.to_path_buf());
    }

    BaseDirs::new()
        .map(|dirs| {
            dirs.cache_dir()
                .join("darc")
                .join("schema-audit")
                .join("claude")
        })
        .context("unable to resolve the user cache directory")
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
            "failed to move cached archive from {} to {}",
            staged_root.display(),
            cache_root.display()
        )
    })?;
    Ok(())
}

/// Extracts one verified npm package archive into a destination directory.
fn extract_package(archive_path: &Path, destination_root: &Path) -> Result<()> {
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

/// Runs one process with a hard timeout and returns its captured output.
fn run_command_with_timeout(command: &mut Command, timeout: Duration) -> Result<Output> {
    let mut child = command
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
        thread::sleep(Duration::from_millis(200));
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

/// Parses newline-delimited JSON bytes into object rows.
fn parse_json_lines(bytes: &[u8], label: &str) -> Result<Vec<Map<String, Value>>> {
    let text = String::from_utf8(bytes.to_vec()).context("expected UTF-8 JSONL output")?;
    parse_jsonl_text(&text, label)
}

/// Parses one JSONL file into object rows.
fn parse_json_file_lines(path: &Path, label: &str) -> Result<Vec<Map<String, Value>>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    parse_jsonl_text(&text, label)
}

/// Parses one JSONL string into object rows.
fn parse_jsonl_text(text: &str, label: &str) -> Result<Vec<Map<String, Value>>> {
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(trimmed)
            .with_context(|| format!("failed to parse {label} line {}", index + 1))?;
        let object = value
            .as_object()
            .cloned()
            .with_context(|| format!("{label} line {} is not a JSON object", index + 1))?;
        rows.push(object);
    }
    Ok(rows)
}

/// Parses the captured Claude hook event log.
fn parse_hook_events(path: &Path) -> Result<Vec<Map<String, Value>>> {
    if !path.is_file() {
        bail!(
            "Claude fixture did not produce any hook output at {}",
            path.display()
        );
    }
    parse_json_file_lines(path, "Claude hook log")
}

/// Collects transcript lines from Claude's live project sessions directory for one fixture cwd.
fn collect_live_transcript_lines<'a>(
    project_root: &Path,
    command_envs: impl Iterator<Item = (&'a OsStr, Option<&'a OsStr>)>,
) -> Result<Vec<Map<String, Value>>> {
    let live_root = resolve_home_from_command_envs(command_envs)?
        .join(".claude")
        .join("projects");
    let project_dir = live_root.join(encode_path_for_claude(project_root));
    ensure!(
        project_dir.is_dir(),
        "Claude did not create a live projects directory for {}",
        project_root.display()
    );

    let mut rollout_paths = Vec::new();
    for entry in walkdir::WalkDir::new(&project_dir) {
        let entry = entry.with_context(|| format!("failed to walk {}", project_dir.display()))?;
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl")
        {
            rollout_paths.push(entry.into_path());
        }
    }
    rollout_paths.sort();
    ensure!(
        !rollout_paths.is_empty(),
        "Claude did not emit any transcript JSONL in {}",
        project_dir.display()
    );

    let mut lines = Vec::new();
    for rollout_path in rollout_paths {
        lines.extend(parse_json_file_lines(
            &rollout_path,
            "Claude live transcript JSONL",
        )?);
    }
    Ok(lines)
}

/// Collects one parent transcript plus any emitted subagent transcripts for one fixture run.
fn collect_fixture_transcript_lines(transcript_path: &Path) -> Result<Vec<Map<String, Value>>> {
    let mut lines = parse_json_file_lines(transcript_path, "Claude transcript JSONL")?;
    let subagent_root = transcript_path.with_extension("").join("subagents");
    if !subagent_root.is_dir() {
        return Ok(lines);
    }

    let mut subagent_paths = fs::read_dir(&subagent_root)
        .with_context(|| format!("failed to read {}", subagent_root.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect::<Vec<_>>();
    subagent_paths.sort();
    for subagent_path in subagent_paths {
        lines.extend(parse_json_file_lines(
            &subagent_path,
            "Claude subagent transcript JSONL",
        )?);
    }
    Ok(lines)
}

/// Resolves the transcript path reported by Claude hook events.
fn transcript_path_from_hook_events<'a>(
    events: &'a [Map<String, Value>],
    command_envs: impl Iterator<Item = (&'a OsStr, Option<&'a OsStr>)>,
) -> Result<PathBuf> {
    let transcript_path = events
        .iter()
        .find_map(|event| event.get("transcript_path").and_then(Value::as_str))
        .context("Claude fixture hook output did not report any transcript_path")?;
    expand_hook_path(transcript_path, command_envs)
}

/// Expands one hook-reported path against the command environment.
fn expand_hook_path<'a>(
    raw_path: &str,
    command_envs: impl Iterator<Item = (&'a OsStr, Option<&'a OsStr>)>,
) -> Result<PathBuf> {
    if let Some(stripped) = raw_path.strip_prefix("~/") {
        let home = resolve_home_from_command_envs(command_envs)?;
        return Ok(home.join(stripped));
    }
    Ok(PathBuf::from(raw_path))
}

/// Resolves the effective HOME directory for one command environment.
fn resolve_home_from_command_envs<'a>(
    mut command_envs: impl Iterator<Item = (&'a OsStr, Option<&'a OsStr>)>,
) -> Result<PathBuf> {
    command_envs
        .find_map(|(name, value)| (name == OsStr::new("HOME")).then(|| value.map(PathBuf::from)))
        .flatten()
        .or_else(|| env::var_os("HOME").map(PathBuf::from))
        .context("Claude runtime required HOME but no home directory was available")
}

/// Extracts the members from one TypeScript union declaration.
fn collect_type_union_members(text: &str, anchor: &str) -> Vec<String> {
    let Some(start) = text.find(anchor) else {
        return Vec::new();
    };
    let mut members = BTreeSet::new();
    let tail = &text[start + anchor.len()..];
    for line in tail.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('|') {
            let member = trimmed
                .trim_start_matches('|')
                .trim()
                .trim_end_matches(';')
                .trim();
            if !member.is_empty() {
                members.insert(member.to_owned());
            }
            continue;
        }
        if !members.is_empty() {
            break;
        }
    }
    members.into_iter().collect()
}

/// Extracts the string literals bound to one TypeScript field name.
fn collect_field_string_literals(text: &str, field_name: &str) -> Vec<String> {
    let needle = format!("{field_name}: '");
    let mut literals = BTreeSet::new();
    let mut rest = text;
    while let Some(index) = rest.find(&needle) {
        let after = &rest[index + needle.len()..];
        let Some(end) = after.find('\'') else {
            break;
        };
        literals.insert(after[..end].to_owned());
        rest = &after[end + 1..];
    }
    literals.into_iter().collect()
}

/// Returns the coarse JSON type name for one value.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Builds one compact command stderr summary.
fn command_output_summary(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "command exited without stderr output".to_owned();
    }
    truncate_text(&trimmed.replace('\n', " "), 240)
}

/// Resolves the dedicated `~/src` root used for Claude audit fixture workspaces.
fn resolve_fixture_workspace_root() -> Result<PathBuf> {
    let home_dir = BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .context("unable to resolve the user home directory for Claude audit workspaces")?;
    Ok(home_dir.join("src").join(FIXTURE_WORKSPACE_DIR_NAME))
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

    /// Creates one unique managed directory beneath an explicit parent path.
    fn new_in(parent: &Path, prefix: &str) -> Result<Self> {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let path = parent.join(format!("{prefix}-{}", unique_suffix()));
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

/// Quotes one string for a shell command embedded in Claude hook settings.
fn shell_quote(text: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", text.replace('"', "\\\""))
    } else {
        format!("'{}'", text.replace('\'', "'\"'\"'"))
    }
}

/// Encodes one raw byte slice as unwrapped standard base64 text.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        encoded.push(TABLE[(b0 >> 2) as usize] as char);
        encoded.push(TABLE[(((b0 & 0b11) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(TABLE[(((b1 & 0b1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(TABLE[(b2 & 0b11_1111) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        env,
        ffi::{OsStr, OsString},
        fs::{self, File},
        io::Write,
        path::{Path, PathBuf},
        process::Command,
        time::Duration,
    };

    use anyhow::{Result, anyhow};
    use flate2::{Compression, write::GzEncoder};
    use serde_json::{Map, Value, json};
    use tar::{Builder, Header};

    use super::{
        ClaudeAuditSnapshot, ClaudeSchemaAuditOutcome, ClaudeSchemaAuditProvider,
        ClaudeSchemaSurveyMode, ClaudeSdkSchemaManifest, ClaudeTranscriptSchemaManifest,
        NpmPackageManifest, TranscriptManifestBuilder, audit_fixtures, build_hook_settings,
        build_sampling_plan, collect_cli_sdk_manifest, collect_field_string_literals,
        collect_fixture_transcript_lines, collect_stable_release_versions,
        collect_type_union_members, configure_host_auth_environment_from_iter,
        detect_allowed_tools_flag, native_cli_dependency, node_modules_package_path,
        parse_hook_events, parse_jsonl_text, run_claude_schema_audit_with_provider,
        run_command_with_timeout, select_audited_release_versions, validate_fixture_coverage,
    };
    use crate::schema_diff::{normalize_json, summarize_schema_differences};

    struct FakeClaudeSchemaAuditProvider {
        versions: Vec<String>,
        snapshots: BTreeMap<String, ClaudeAuditSnapshot>,
    }

    impl FakeClaudeSchemaAuditProvider {
        /// Builds one fake Claude provider for audit tests.
        fn new(versions: &[&str], snapshots: &[(&str, ClaudeAuditSnapshot)]) -> Self {
            Self {
                versions: versions
                    .iter()
                    .map(|version| (*version).to_owned())
                    .collect(),
                snapshots: snapshots
                    .iter()
                    .map(|(version, snapshot)| ((*version).to_owned(), snapshot.clone()))
                    .collect(),
            }
        }
    }

    #[test]
    fn resolves_native_claude_dependency_for_node_platform() {
        let package_name = format!("{}-darwin-arm64", super::NPM_CLAUDE_CODE_PACKAGE);
        let mut optional_dependencies = BTreeMap::new();
        optional_dependencies.insert(package_name.clone(), "2.1.126".to_owned());
        let manifest = NpmPackageManifest {
            bin: BTreeMap::new(),
            optional_dependencies,
        };

        assert_eq!(
            native_cli_dependency(&manifest, "darwin-arm64").unwrap(),
            (package_name, "2.1.126".to_owned())
        );
    }

    #[test]
    fn resolves_native_binary_name_from_node_platform() {
        assert_eq!(super::native_cli_binary_name("darwin-arm64"), "claude");
        assert_eq!(super::native_cli_binary_name("win32-x64"), "claude.exe");
    }

    #[test]
    fn released_cli_command_preserves_native_wrapper_marker_after_environment_config() {
        let runtime = super::AuditRuntime {
            node_binary: PathBuf::from("/usr/local/bin/node"),
            node_platform_suffix: "darwin-arm64".to_owned(),
            hook_python: PathBuf::from("/usr/bin/python3"),
        };
        let cli = super::ReleasedClaudeCli::NativeBinary(PathBuf::from(
            "/tmp/package/node_modules/@anthropic-ai/claude-code-darwin-arm64/claude",
        ));
        let mut command = cli.command(&runtime, Path::new("/tmp/repo"));
        command.arg("--help");
        configure_host_auth_environment_from_iter(
            &mut command,
            Path::new("/tmp/repo"),
            [(OsString::from("PATH"), OsString::from("/usr/bin"))],
        );
        cli.apply_target_environment(&mut command);
        let envs = command_envs(&command);

        assert_eq!(
            command.get_program(),
            OsStr::new("/tmp/package/node_modules/@anthropic-ai/claude-code-darwin-arm64/claude")
        );
        assert_eq!(
            envs.get(&OsString::from(super::CLAUDE_NPM_WRAPPER_ENV_NAME)),
            Some(&Some(OsString::from("1")))
        );
        assert_eq!(command.get_args().collect::<Vec<_>>(), vec!["--help"]);
    }

    #[test]
    fn builds_scoped_node_modules_package_path() {
        assert_eq!(
            node_modules_package_path(
                Path::new("/tmp/package"),
                "@anthropic-ai/claude-code-darwin-arm64"
            )
            .unwrap(),
            Path::new("/tmp/package")
                .join("node_modules")
                .join("@anthropic-ai")
                .join("claude-code-darwin-arm64")
        );
    }

    impl ClaudeSchemaAuditProvider for FakeClaudeSchemaAuditProvider {
        fn list_release_versions(&self) -> Result<Vec<String>> {
            Ok(self.versions.clone())
        }

        fn collect_snapshot<F>(
            &self,
            version: &str,
            _report_progress: &mut F,
        ) -> Result<ClaudeAuditSnapshot>
        where
            F: FnMut(&str),
        {
            self.snapshots
                .get(version)
                .cloned()
                .ok_or_else(|| anyhow!("missing fake snapshot for `{version}`"))
        }
    }

    fn manifest(line_types: &[&str], tool_names: &[&str]) -> ClaudeTranscriptSchemaManifest {
        ClaudeTranscriptSchemaManifest {
            fixture_names: audit_fixtures()
                .iter()
                .map(|fixture| fixture.name.to_owned())
                .collect(),
            line_types: line_types.iter().map(|value| (*value).to_owned()).collect(),
            user_content_types: vec!["text".to_owned(), "tool_result".to_owned()],
            assistant_content_types: vec![
                "thinking".to_owned(),
                "text".to_owned(),
                "tool_use".to_owned(),
            ],
            assistant_stop_reasons: vec!["end_turn".to_owned(), "tool_use".to_owned()],
            progress_types: vec!["hook_progress".to_owned()],
            system_subtypes: vec!["init".to_owned()],
            tool_names: tool_names.iter().map(|value| (*value).to_owned()).collect(),
            hook_event_names: vec![
                "PostToolUse".to_owned(),
                "SessionEnd".to_owned(),
                "SessionStart".to_owned(),
                "SubagentStop".to_owned(),
            ],
            stream_event_types: vec!["assistant".to_owned(), "result".to_owned()],
            stream_event_subtypes: vec!["success".to_owned()],
            top_level_keys_by_type: BTreeMap::from([
                (
                    "assistant".to_owned(),
                    vec!["cwd".to_owned(), "message".to_owned(), "type".to_owned()],
                ),
                (
                    "user".to_owned(),
                    vec!["cwd".to_owned(), "message".to_owned(), "type".to_owned()],
                ),
            ]),
        }
    }

    fn sdk_manifest(version: Option<&str>) -> ClaudeSdkSchemaManifest {
        ClaudeSdkSchemaManifest {
            agent_sdk_version: version.map(str::to_owned),
            cli_tool_input_schemas: vec!["BashInput".to_owned(), "ReadInput".to_owned()],
            cli_tool_output_schemas: vec!["BashOutput".to_owned(), "ReadOutput".to_owned()],
            agent_sdk_message_variants: vec![
                "SDKAssistantMessage".to_owned(),
                "SDKResultMessage".to_owned(),
            ],
            agent_sdk_hook_events: vec!["SessionStart".to_owned(), "SessionEnd".to_owned()],
        }
    }

    fn command_envs(command: &std::process::Command) -> BTreeMap<OsString, Option<OsString>> {
        command
            .get_envs()
            .map(|(name, value)| (name.to_os_string(), value.map(OsString::from)))
            .collect()
    }

    #[test]
    fn parses_and_filters_stable_claude_versions() {
        let versions = collect_stable_release_versions(vec![
            "2.1.92".to_owned(),
            "2.1.87-beta.1".to_owned(),
            "2.1.87".to_owned(),
            "2.1.84".to_owned(),
            "bad-version".to_owned(),
        ]);

        assert_eq!(
            versions
                .iter()
                .map(|version| version.raw.clone())
                .collect::<Vec<_>>(),
            vec![
                "2.1.92".to_owned(),
                "2.1.87".to_owned(),
                "2.1.84".to_owned(),
            ]
        );
    }

    #[test]
    fn selects_audited_range_from_latest_published_down_to_exact_cutoff() {
        let versions = collect_stable_release_versions(vec![
            "2.1.130".to_owned(),
            "2.1.128".to_owned(),
            "2.1.126".to_owned(),
            "2.1.124".to_owned(),
        ]);
        let selected = select_audited_release_versions(&versions, None).unwrap();

        assert_eq!(
            selected
                .iter()
                .map(|version| version.raw.clone())
                .collect::<Vec<_>>(),
            vec![
                "2.1.130".to_owned(),
                "2.1.128".to_owned(),
                "2.1.126".to_owned(),
            ]
        );
    }

    #[test]
    fn selects_audited_range_from_latest_published_down_to_custom_floor() {
        let versions = collect_stable_release_versions(vec![
            "2.1.130".to_owned(),
            "2.1.128".to_owned(),
            "2.1.126".to_owned(),
            "2.1.124".to_owned(),
        ]);
        let selected = select_audited_release_versions(&versions, Some("2.1.124")).unwrap();

        assert_eq!(
            selected
                .iter()
                .map(|version| version.raw.clone())
                .collect::<Vec<_>>(),
            vec![
                "2.1.130".to_owned(),
                "2.1.128".to_owned(),
                "2.1.126".to_owned(),
                "2.1.124".to_owned(),
            ]
        );
    }

    #[test]
    fn sampling_plan_picks_stride_anchors_and_assumed_gaps() {
        let versions = collect_stable_release_versions(vec![
            "2.1.131".to_owned(),
            "2.1.130".to_owned(),
            "2.1.129".to_owned(),
            "2.1.128".to_owned(),
            "2.1.127".to_owned(),
            "2.1.126".to_owned(),
        ]);
        let audited = select_audited_release_versions(&versions, None).unwrap();
        let plan = build_sampling_plan(&audited, 2);

        assert_eq!(
            plan.inspected_versions_desc
                .iter()
                .map(|version| version.raw.clone())
                .collect::<Vec<_>>(),
            vec![
                "2.1.131".to_owned(),
                "2.1.130".to_owned(),
                "2.1.128".to_owned(),
                "2.1.126".to_owned(),
            ]
        );
        assert_eq!(
            plan.assumed_compatible_intervals,
            vec!["2.1.127".to_owned(), "2.1.129".to_owned()]
        );
    }

    #[test]
    fn reports_compatibility_when_transcript_manifests_match() {
        let provider = FakeClaudeSchemaAuditProvider::new(
            &["2.1.130", "2.1.128", "2.1.126"],
            &[
                (
                    "2.1.130",
                    ClaudeAuditSnapshot {
                        transcript_manifest: manifest(
                            &["assistant", "progress", "system", "user"],
                            &["Bash", "Read", "Task"],
                        ),
                        sdk_manifest: sdk_manifest(Some("0.2.130")),
                    },
                ),
                (
                    "2.1.128",
                    ClaudeAuditSnapshot {
                        transcript_manifest: manifest(
                            &["assistant", "progress", "system", "user"],
                            &["Bash", "Read", "Task"],
                        ),
                        sdk_manifest: sdk_manifest(Some("0.2.128")),
                    },
                ),
                (
                    "2.1.126",
                    ClaudeAuditSnapshot {
                        transcript_manifest: manifest(
                            &["assistant", "progress", "system", "user"],
                            &["Bash", "Read", "Task"],
                        ),
                        sdk_manifest: sdk_manifest(Some("0.2.126")),
                    },
                ),
            ],
        );

        let report = run_claude_schema_audit_with_provider(
            "npm".to_owned(),
            PathBuf::from("/tmp/darc-claude-cache"),
            &provider,
            1,
            None,
            ClaudeSchemaSurveyMode::Refine,
        )
        .unwrap();

        assert!(matches!(
            report.outcome,
            ClaudeSchemaAuditOutcome::Compatible
        ));
        assert_eq!(report.latest_published_version, "2.1.130");
        assert_eq!(report.audited_versions.len(), 3);
        assert!(report.supplementary_sdk_drift.is_some());
    }

    #[test]
    fn detects_first_transcript_drift_and_preserves_sdk_signal_separately() {
        let provider = FakeClaudeSchemaAuditProvider::new(
            &["2.1.130", "2.1.128", "2.1.126"],
            &[
                (
                    "2.1.130",
                    ClaudeAuditSnapshot {
                        transcript_manifest: manifest(
                            &["assistant", "mystery-event", "progress", "system", "user"],
                            &["Bash", "Read", "Task"],
                        ),
                        sdk_manifest: sdk_manifest(Some("0.2.130")),
                    },
                ),
                (
                    "2.1.128",
                    ClaudeAuditSnapshot {
                        transcript_manifest: manifest(
                            &["assistant", "mystery-event", "progress", "system", "user"],
                            &["Bash", "Read", "Task"],
                        ),
                        sdk_manifest: sdk_manifest(Some("0.2.128")),
                    },
                ),
                (
                    "2.1.126",
                    ClaudeAuditSnapshot {
                        transcript_manifest: manifest(
                            &["assistant", "progress", "system", "user"],
                            &["Bash", "Read", "Task"],
                        ),
                        sdk_manifest: sdk_manifest(Some("0.2.126")),
                    },
                ),
            ],
        );

        let report = run_claude_schema_audit_with_provider(
            "npm".to_owned(),
            PathBuf::from("/tmp/darc-claude-cache"),
            &provider,
            2,
            None,
            ClaudeSchemaSurveyMode::Refine,
        )
        .unwrap();

        let ClaudeSchemaAuditOutcome::Drift(drift) = report.outcome else {
            panic!("expected transcript drift");
        };
        assert_eq!(drift.first_drift_version, "2.1.128");
        assert!(
            drift
                .difference_summary
                .iter()
                .any(|line| line.contains("mystery-event"))
        );
        assert!(report.supplementary_sdk_drift.is_some());
        assert!(report.inspected_versions.contains(&"2.1.128".to_owned()));
        assert!(
            !report
                .assumed_compatible_intervals
                .contains(&"2.1.128".to_owned())
        );
    }

    #[test]
    fn refine_mode_reports_only_versions_it_actually_walked() {
        let provider = FakeClaudeSchemaAuditProvider::new(
            &[
                "2.1.131", "2.1.130", "2.1.129", "2.1.128", "2.1.127", "2.1.126",
            ],
            &[
                (
                    "2.1.131",
                    ClaudeAuditSnapshot {
                        transcript_manifest: manifest(
                            &["assistant", "mystery-event", "progress", "system", "user"],
                            &["Bash", "Read", "Task"],
                        ),
                        sdk_manifest: sdk_manifest(Some("0.2.131")),
                    },
                ),
                (
                    "2.1.130",
                    ClaudeAuditSnapshot {
                        transcript_manifest: manifest(
                            &["assistant", "mystery-event", "progress", "system", "user"],
                            &["Bash", "Read", "Task"],
                        ),
                        sdk_manifest: sdk_manifest(Some("0.2.130")),
                    },
                ),
                (
                    "2.1.129",
                    ClaudeAuditSnapshot {
                        transcript_manifest: manifest(
                            &["assistant", "mystery-event", "progress", "system", "user"],
                            &["Bash", "Read", "Task"],
                        ),
                        sdk_manifest: sdk_manifest(Some("0.2.129")),
                    },
                ),
                (
                    "2.1.128",
                    ClaudeAuditSnapshot {
                        transcript_manifest: manifest(
                            &["assistant", "progress", "system", "user"],
                            &["Bash", "Read", "Task"],
                        ),
                        sdk_manifest: sdk_manifest(Some("0.2.128")),
                    },
                ),
                (
                    "2.1.127",
                    ClaudeAuditSnapshot {
                        transcript_manifest: manifest(
                            &["assistant", "progress", "system", "user"],
                            &["Bash", "Read", "Task"],
                        ),
                        sdk_manifest: sdk_manifest(Some("0.2.127")),
                    },
                ),
                (
                    "2.1.126",
                    ClaudeAuditSnapshot {
                        transcript_manifest: manifest(
                            &["assistant", "progress", "system", "user"],
                            &["Bash", "Read", "Task"],
                        ),
                        sdk_manifest: sdk_manifest(Some("0.2.126")),
                    },
                ),
            ],
        );

        let report = run_claude_schema_audit_with_provider(
            "npm".to_owned(),
            PathBuf::from("/tmp/darc-claude-cache"),
            &provider,
            5,
            None,
            ClaudeSchemaSurveyMode::Refine,
        )
        .unwrap();

        let ClaudeSchemaAuditOutcome::Drift(drift) = report.outcome else {
            panic!("expected transcript drift");
        };
        assert_eq!(drift.first_drift_version, "2.1.129");
        assert!(report.inspected_versions.contains(&"2.1.131".to_owned()));
        assert!(report.inspected_versions.contains(&"2.1.129".to_owned()));
        assert!(!report.inspected_versions.contains(&"2.1.130".to_owned()));
        assert!(report.assumed_compatible_intervals.is_empty());
    }

    #[test]
    fn extracts_meaningful_shapes_from_transcript_hook_and_stream_artifacts() {
        let transcript = r#"{"type":"user","message":{"role":"user","content":"Inspect README.md"},"cwd":"/tmp/repo","sessionId":"session","version":"2.1.87"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"plan"},{"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"README.md"}},{"type":"text","text":"Done."}],"stop_reason":"end_turn"},"cwd":"/tmp/repo","sessionId":"session","version":"2.1.87"}
{"type":"progress","data":{"type":"hook_progress"},"cwd":"/tmp/repo","sessionId":"session","version":"2.1.87"}
{"type":"system","subtype":"init","cwd":"/tmp/repo","sessionId":"session","version":"2.1.87"}"#;
        let hooks = r#"{"hook_event_name":"SessionStart","transcript_path":"/tmp/repo/transcript.jsonl"}
{"hook_event_name":"PostToolUse","tool_name":"Read"}
{"hook_event_name":"SubagentStop"}"#;
        let stream = r#"{"type":"assistant"}
{"type":"result","subtype":"success"}"#;

        let session = super::ClaudeFixtureSession {
            transcript_lines: parse_jsonl_text(transcript, "transcript").unwrap(),
            hook_events: parse_jsonl_text(hooks, "hooks").unwrap(),
            stream_events: parse_jsonl_text(stream, "stream").unwrap(),
        };
        let mut builder = TranscriptManifestBuilder::default();
        builder.record_fixture(&session, "fixture").unwrap();
        let manifest = builder.finish();

        assert_eq!(manifest.fixture_names, vec!["fixture".to_owned()]);
        assert!(manifest.line_types.contains(&"assistant".to_owned()));
        assert!(manifest.line_types.contains(&"user".to_owned()));
        assert!(manifest.user_content_types.contains(&"text".to_owned()));
        assert!(
            manifest
                .assistant_content_types
                .contains(&"tool_use".to_owned())
        );
        assert!(
            manifest
                .assistant_content_types
                .contains(&"thinking".to_owned())
        );
        assert!(manifest.tool_names.contains(&"Read".to_owned()));
        assert!(
            manifest
                .hook_event_names
                .contains(&"SessionStart".to_owned())
        );
        assert!(
            manifest
                .stream_event_types
                .contains(&"assistant".to_owned())
        );
        assert!(
            manifest
                .stream_event_subtypes
                .contains(&"success".to_owned())
        );
    }

    #[test]
    fn extracts_type_unions_and_hook_events_from_typescript_surfaces() {
        let sdk_tools = r#"
export type ToolInputSchemas =
  | BashInput
  | ReadInput;
export type ToolOutputSchemas =
  | BashOutput
  | ReadOutput;
"#;
        let sdk_dts = r#"
export declare type SDKMessage =
  | SDKAssistantMessage
  | SDKResultMessage;
export declare type SessionStartHookInput = BaseHookInput & {
    hook_event_name: 'SessionStart';
};
export declare type SessionEndHookInput = BaseHookInput & {
    hook_event_name: 'SessionEnd';
};
"#;

        assert_eq!(
            collect_type_union_members(sdk_tools, "export type ToolInputSchemas ="),
            vec!["BashInput".to_owned(), "ReadInput".to_owned()]
        );
        assert_eq!(
            collect_type_union_members(sdk_dts, "export declare type SDKMessage ="),
            vec![
                "SDKAssistantMessage".to_owned(),
                "SDKResultMessage".to_owned()
            ]
        );
        assert_eq!(
            collect_field_string_literals(sdk_dts, "hook_event_name"),
            vec!["SessionEnd".to_owned(), "SessionStart".to_owned()]
        );
    }

    #[test]
    fn hook_settings_embed_the_capture_log_path() {
        let settings = build_hook_settings(
            Path::new("/tmp/hooks.jsonl"),
            Path::new("/usr/local/bin/python"),
        );
        assert!(settings.contains("SessionStart"));
        assert!(settings.contains("/tmp/hooks.jsonl"));
        assert!(settings.contains("/usr/local/bin/python"));
    }

    #[test]
    fn parses_hook_logs_from_disk() {
        let dir = tempfile_dir();
        let hook_path = dir.join("hooks.jsonl");
        let mut file = File::create(&hook_path).unwrap();
        writeln!(
            file,
            "{{\"hook_event_name\":\"SessionStart\",\"transcript_path\":\"/tmp/transcript.jsonl\"}}"
        )
        .unwrap();

        let hooks = parse_hook_events(&hook_path).unwrap();
        assert_eq!(
            hooks[0].get("hook_event_name").and_then(Value::as_str),
            Some("SessionStart")
        );
    }

    #[test]
    fn normalizes_and_summarizes_schema_like_differences() {
        let left = normalize_json(json!({
            "required": ["a", "b"],
            "line_types": ["assistant", "user"]
        }));
        let right = normalize_json(json!({
            "required": ["b", "a"],
            "line_types": ["assistant", "user", "progress"]
        }));

        let diff = summarize_schema_differences(&left, &right);
        assert!(diff.iter().any(|line| line.contains("line_types")));
    }

    #[test]
    fn missing_cli_sdk_types_do_not_fail_supplementary_manifest_collection() {
        let root = tempfile_dir();
        let mut progress = Vec::new();

        let manifest = collect_cli_sdk_manifest(&root, "2.1.92", &mut |message: &str| {
            progress.push(message.to_owned())
        });

        assert!(manifest.agent_sdk_version.is_none());
        assert!(manifest.cli_tool_input_schemas.is_empty());
        assert!(
            progress
                .iter()
                .any(|line: &String| line.contains("Skipping embedded CLI SDK types"))
        );
    }

    #[test]
    fn detects_supported_allowed_tools_flag_spelling() {
        assert!(matches!(
            detect_allowed_tools_flag("Usage: claude --allowedTools Read"),
            Some(super::ClaudeAllowedToolsFlag::CamelCase)
        ));
        assert!(matches!(
            detect_allowed_tools_flag("Usage: claude --allowed-tools Read"),
            Some(super::ClaudeAllowedToolsFlag::Hyphenated)
        ));
        assert!(detect_allowed_tools_flag("Usage: claude --print").is_none());
    }

    #[test]
    fn provider_level_audit_requires_explicit_host_auth() {
        let error = super::run_claude_schema_audit_with_progress(
            super::ClaudeSchemaAuditOptions::default(),
            |_| {},
        )
        .unwrap_err();

        assert!(error.to_string().contains("requires --use-host-auth"));
    }

    #[test]
    fn host_auth_environment_forwards_only_allowlisted_variables() {
        let mut command = Command::new("sh");
        configure_host_auth_environment_from_iter(
            &mut command,
            Path::new("/tmp/repo"),
            [
                (OsString::from("PATH"), OsString::from("/usr/bin")),
                (OsString::from("HOME"), OsString::from("/Users/tester")),
                (
                    OsString::from("XDG_CONFIG_HOME"),
                    OsString::from("/Users/tester/.config"),
                ),
                (
                    OsString::from("ANTHROPIC_API_KEY"),
                    OsString::from("test-key"),
                ),
                (OsString::from("AWS_REGION"), OsString::from("us-east-1")),
                (
                    OsString::from("VERTEX_REGION_CLAUDE_OPUS_4_1"),
                    OsString::from("us-central1"),
                ),
                (OsString::from("GITHUB_TOKEN"), OsString::from("secret")),
                (
                    OsString::from("DATABASE_URL"),
                    OsString::from("postgres://secret"),
                ),
            ],
        );
        let envs = command_envs(&command);

        assert_eq!(
            envs.get(&OsString::from("CLAUDE_CODE_AUDIT_PROJECT_ROOT")),
            Some(&Some(OsString::from("/tmp/repo")))
        );
        assert_eq!(
            envs.get(&OsString::from("ANTHROPIC_API_KEY")),
            Some(&Some(OsString::from("test-key")))
        );
        assert_eq!(
            envs.get(&OsString::from("AWS_REGION")),
            Some(&Some(OsString::from("us-east-1")))
        );
        assert_eq!(
            envs.get(&OsString::from("VERTEX_REGION_CLAUDE_OPUS_4_1")),
            Some(&Some(OsString::from("us-central1")))
        );
        assert!(!envs.contains_key(&OsString::from("GITHUB_TOKEN")));
        assert!(!envs.contains_key(&OsString::from("DATABASE_URL")));
    }

    #[test]
    fn validate_fixture_coverage_accepts_task_alias_for_agent_fixture_with_subagent_artifact() {
        let fixture = audit_fixtures()
            .iter()
            .copied()
            .find(|fixture| fixture.name == "subagent_task")
            .expect("subagent fixture should exist");
        let session = super::ClaudeFixtureSession {
            transcript_lines: parse_jsonl_text(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Task"}]}}
{"type":"user","isSidechain":true,"agentId":"delegated-agent","message":{"role":"user","content":"read README"}}"#,
                "transcript",
            )
            .unwrap(),
            hook_events: Vec::new(),
            stream_events: Vec::new(),
        };

        validate_fixture_coverage(&session, fixture).unwrap();
    }

    #[test]
    fn validate_fixture_coverage_rejects_parent_only_delegation_evidence() {
        let fixture = audit_fixtures()
            .iter()
            .copied()
            .find(|fixture| fixture.name == "subagent_task")
            .expect("subagent fixture should exist");
        let session = super::ClaudeFixtureSession {
            transcript_lines: parse_jsonl_text(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Agent"}]}}"#,
                "transcript",
            )
            .unwrap(),
            hook_events: Vec::new(),
            stream_events: Vec::new(),
        };

        let error = validate_fixture_coverage(&session, fixture).unwrap_err();
        assert!(error.to_string().contains("observable subagent signal"));
    }

    #[test]
    fn timeout_wrapper_drains_large_stdout_without_deadlock() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("yes x | head -c 200000");

        let output = run_command_with_timeout(&mut command, Duration::from_secs(2)).unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 200_000);
    }

    #[test]
    fn collect_fixture_transcript_lines_includes_subagent_logs() {
        let root = tempfile_dir();
        let transcript = root.join("session.jsonl");
        fs::write(
            &transcript,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        )
        .unwrap();
        let subagents = root.join("session/subagents");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(
            subagents.join("agent-a.jsonl"),
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"sub\"}]}}\n",
        )
        .unwrap();

        let lines = collect_fixture_transcript_lines(&transcript).unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().any(|line: &Map<String, Value>| {
            line.get("type").and_then(Value::as_str) == Some("assistant")
        }));
    }

    #[test]
    fn resolve_fixture_workspace_root_targets_src_directory() {
        let root = super::resolve_fixture_workspace_root().unwrap();
        assert!(root.ends_with(Path::new("src").join(super::FIXTURE_WORKSPACE_DIR_NAME)));
    }

    #[test]
    fn extracts_gzipped_npm_packages() {
        let root = tempfile_dir();
        let archive_path = root.join("package.tgz");
        let destination = root.join("extract");
        let archive_file = File::create(&archive_path).unwrap();
        let encoder = GzEncoder::new(archive_file, Compression::default());
        let mut builder = Builder::new(encoder);
        let contents = br#"{"name":"fixture"}"#;
        let mut header = Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(contents.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "package/package.json", &contents[..])
            .unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();

        super::extract_package(&archive_path, &destination).unwrap();
        let package_json = fs::read_to_string(destination.join("package/package.json")).unwrap();
        assert!(package_json.contains("\"fixture\""));
    }

    fn tempfile_dir() -> PathBuf {
        let root =
            env::temp_dir().join(format!("darc-claude-audit-test-{}", super::unique_suffix()));
        fs::create_dir_all(&root).unwrap();
        root
    }
}
