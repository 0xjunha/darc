#[cfg(test)]
mod tests;

use std::{
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{
    Arg, ArgAction, Args, ColorChoice, Command as ClapCommand, CommandFactory, FromArgMatches,
    Parser, Subcommand, ValueEnum,
    builder::styling::{AnsiColor, Styles},
    error::ErrorKind,
};
use darc_core::config::load_config;
use darc_core::query::{
    DEFAULT_MATCHED_PATH_LIMIT, DEFAULT_QUERY_PAGE_LIMIT, DEFAULT_RESOLVE_SESSION_MATCH_LIMIT,
    DEFAULT_SEARCH_MATCH_LIMIT, DEFAULT_SESSION_BUNDLE_TURN_LIMIT, DEFAULT_TURN_STEP_LIMIT,
    DEFAULT_WORKSPACE_RECENT_SESSION_LIMIT, FilesQueryRequest, QueryProtocolError,
    ResolveSessionQueryRequest, ResolvedQueryProject, ResolvedSessionMatch, SearchEvidenceField,
    SearchMode, SearchSnippetMatcher, SearchTurnsQueryData, SearchTurnsRequest,
    SessionBundleQueryRequest, SessionBundleView, SessionsQueryRequest, SessionsView,
    TurnDetailOptions, TurnsQueryRequest, TurnsView, query_files_for_project,
    query_project_insight_report_for_project, query_resolve_sessions,
    query_search_turns_for_project, query_session_bundle_for_project,
    query_session_files_for_project, query_sessions_for_project, query_turn_for_project,
    query_turn_insight_report_for_project, query_turns_for_project, query_workspace,
    query_workspace_insight_report, resolve_query_project,
    resolve_query_search_session_id_for_project, resolve_query_session_for_project,
};
use darc_core::{
    IndexOptions, IndexReport, InitDraft, LinkReport, RefreshAllBestEffortReport, RefreshOptions,
    RefreshProgress, RefreshProjectAttempt, RefreshProjectFailure, RefreshReport, SkippedRollout,
    SourceKind, StatusProject, StatusSource, StatusSyncCheck, StatusSyncPlan, SyncOptions,
    SyncReport, WorkspaceStatusReport, default_root_path, execute_sync, index_project_sessions,
    link_project, prepare_init, prepare_sync, preview_link_project, preview_remove_project,
    preview_rename_project, refresh_all_projects_best_effort_with_progress,
    refresh_project_with_progress, remove_project, rename_project, status_project,
    status_workspace, write_init,
};
use darc_paths::{
    current_utc_timestamp, resolve_query_time_bound as resolve_shared_query_time_bound,
};
use darc_rollout_audit::claude::{
    ClaudeSchemaAuditOptions, ClaudeSchemaAuditOutcome, ClaudeSchemaAuditReport,
    ClaudeSchemaSurveyMode, run_claude_schema_audit_with_progress,
};
use darc_rollout_audit::codex::{
    CodexSchemaAuditOptions, CodexSchemaAuditOutcome, CodexSchemaAuditReport,
    run_codex_schema_audit_with_progress,
};
use fs2::FileExt;
use reqwest::{
    StatusCode,
    blocking::{Client, Response},
    header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

/// Terminal styles for Clap-rendered help reference pages.
const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::BrightGreen.on_default().bold())
    .usage(AnsiColor::BrightGreen.on_default().bold())
    .literal(AnsiColor::BrightWhite.on_default().bold())
    .placeholder(AnsiColor::BrightBlue.on_default())
    .error(AnsiColor::BrightRed.on_default().bold())
    .valid(AnsiColor::BrightGreen.on_default())
    .invalid(AnsiColor::BrightYellow.on_default());

const LINK_LONG_ABOUT: &str = "Link one configured project's historical paths into the current project.\n\nRun this command from the target project directory.\nThe PROJECT argument is the old or source project name already stored in ~/.darc/config.toml.\n\nExample:\n- You renamed `/path/to/old-project` to `/path/to/new-project`.\n- Darc still has a configured project named `old-project`.\n- Run `cd /path/to/new-project && darc project link old-project`.\n\nThis command is non-destructive.\nIt updates config so the current project knows the source project's old local_path and known_paths.\nIt does not run `darc refresh` or remove the source project.\n\nUse `--dry-run` to preview the target project, source project, and known-path changes without writing config.";
const REMOVE_LONG_ABOUT: &str = "Remove one configured project and its archived/indexed data.\n\nThe PROJECT argument is matched against the configured project `name` in ~/.darc/config.toml.\nThe name must identify exactly one configured project.\n\nThis command deletes:\n- the project entry from config.toml\n- the project's archived sessions directory under ~/.darc/projects/...\n- the project's indexed SQLite rows\n\nUse `--dry-run` to preview the resolved project and deletion counts without writing.\nYou can run this command from any directory.";
const RENAME_FROM_LONG_ABOUT: &str = "Rebuild one old project's history into the current renamed project.\n\nUse this when you just renamed a project from one name to another.\nRun the command from the new project directory, and pass the old project name.\n\nExample:\n- Darc config still contains a project named `old-project`.\n- You renamed the checkout to `/path/to/new-project`.\n- Run `cd /path/to/new-project && darc project rename-from old-project`.\n\nThis command bootstraps or reuses the current project as the target, links the old project's paths into it, runs `darc refresh`, and removes the old source project after those steps succeed.\n\nIn other words, it is the safe built-in workflow for:\n`darc project link <old-project> && darc refresh && darc project remove <old-project>`\n\nUse `--dry-run` to preview the link target and cleanup counts without writing.\nIf ~/.darc/config.toml does not exist yet, run `darc init` first.";
const HELP_TRAILER_HEADER_STYLE: &str = "\x1b[1;97m";
const HELP_RESET_STYLE: &str = "\x1b[0m";
const DARC_LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/0xjunha/darc/releases/latest";
const DARC_INSTALLER_COMMAND: &str =
    "curl -fsSL https://github.com/0xjunha/darc/releases/latest/download/darc-installer.sh | sh";
const UPGRADE_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const UPGRADE_NUDGE_TIMEOUT: Duration = Duration::from_secs(2);
const UPGRADE_NUDGE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const UPGRADE_NUDGE_NOTIFY_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Returns one styled help trailer section.
fn styled_help_section(title: &str, body: &str) -> String {
    format!("{HELP_TRAILER_HEADER_STYLE}{title}:{HELP_RESET_STYLE}\n{body}")
}

/// Returns top-level common workflow examples.
fn root_after_help() -> String {
    styled_help_section(
        "Common workflows",
        "  darc status\n  darc refresh\n  darc search \"panic\" --limit 5\n  darc show session <SESSION_ID> --turn-limit 5\n\nRun `darc help <command>` for details on a specific command.",
    )
}

/// Returns sync command examples.
fn sync_after_help() -> String {
    styled_help_section(
        "Examples",
        "  darc sync --dry-run\n  darc sync --provider codex",
    )
}

/// Returns index command examples.
fn index_after_help() -> String {
    styled_help_section("Examples", "  darc index\n  darc index --provider claude")
}

/// Returns canonical search command examples.
fn search_after_help() -> String {
    styled_help_section(
        "Examples",
        "  darc search \"panic unwrap\" --limit 5\n  darc search --mode literal --query \"--output-last-message\" --field user-message\n  darc search --mode regex --query \"error\\s+code\" --since 7d\n  darc search --mode file-path \"docs/**/*.md\" --limit 5\n  darc search --mode path-fragment query-protocol",
    )
}

/// Returns mode-specific guidance for search query text.
fn search_query_help() -> &'static str {
    "Query text or path pattern. The accepted form depends on --mode.\n\nMode-specific query forms:\n  keyword: one or more terms, e.g. \"panic unwrap\"; searches Darc's derived per-turn text.\n  literal: exact plain text, e.g. \"--output-last-message\"; use --query when the text starts with '-'.\n  regex: Rust regex, e.g. \"panic|unwrap\" or \"error\\s+code\"; quote shell metacharacters.\n  file-name: file basename text, e.g. \"lib.rs\".\n  file-path: project-relative glob, e.g. \"docs/**/*.md\".\n  path-fragment: path substring or prefix, e.g. \"query-protocol\"."
}

/// Builds the Clap command tree with Darc-specific help flag placement.
fn cli_command() -> ClapCommand {
    with_explicit_help_arg(Cli::command())
}

/// Adds the Darc help flag into a stable `Help` section for one command tree.
fn with_explicit_help_arg(command: ClapCommand) -> ClapCommand {
    command
        .disable_help_flag(true)
        .arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::Help)
                .help("Print help (see a summary with '-h')")
                .help_heading("Help"),
        )
        .mut_subcommands(with_explicit_help_arg)
}

#[derive(Debug, Parser)]
#[command(
    name = "darc",
    version,
    about = "Archive, index, and query coding-agent sessions",
    color = ColorChoice::Auto,
    styles = HELP_STYLES,
    after_help = root_after_help()
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Supported CLI subcommands.
#[derive(Debug, Subcommand)]
enum Commands {
    /// Detect local sources and create the shared darc config.
    Init(InitArgs),
    #[command(
        about = "Sync then index archived sessions for the active project",
        long_about = "Sync then index archived sessions for the active project.\n\nThis is the daily happy path after `darc init`.\nBy default it refreshes the project resolved from the current directory.\nUse `--provider` to limit both sync and index to selected providers.\nUse `--all` to refresh every registered project in the shared darc workspace.\nWhen `--all` is set, darc continues past per-project failures, prints a workspace summary, and exits non-zero if any project failed."
    )]
    Refresh(RefreshArgs),
    #[command(
        about = "Show Darc status for the active project or workspace",
        long_about = "Show Darc status for the active project or workspace.\n\nBy default this resolves the project from the current directory and prints root, config, source, archive, index, and sync-manifest status.\nUse `--workspace` to summarize every configured project in the shared Darc workspace.\nUse `--check` to run sync planning without writing manifests, config, archives, or SQLite."
    )]
    Status(StatusArgs),
    #[command(
        about = "List projects, sessions, turns, or files from indexed history",
        long_about = "List projects, sessions, turns, or files from indexed history.\n\nList commands emit JSON envelopes on stdout and are the canonical browse surface for coding agents."
    )]
    List(ListArgs),
    #[command(
        about = "Show workspace, session, or turn detail from indexed history",
        long_about = "Show workspace, session, or turn detail from indexed history.\n\nShow commands emit JSON envelopes on stdout. `darc show session` returns a bounded session bundle: compact session summary, a paginated turn-detail page, and a capped session-file preview."
    )]
    Show(ShowArgs),
    #[command(
        about = "Search indexed turns and return matching turn hits",
        long_about = "Search indexed turns and return matching turn hits.\n\nDarc search is turn-scoped: every hit includes the session id and turn ordinal needed for follow-up `darc show turn` or `darc show session` calls. Search defaults to keyword mode. Pass --mode for literal, regex, file-name, file-path, or path-fragment search modes.",
        after_help = search_after_help()
    )]
    Search(SearchArgs),
    #[command(
        about = "Show workspace, project, or turn stats",
        long_about = "Show workspace, project, or turn stats.\n\nStats commands emit JSON envelopes with indexed counts, active time, tool usage, file usage, and related metrics."
    )]
    Stats(StatsArgs),
    #[command(
        about = "Resolve short identifiers into canonical Darc identifiers",
        long_about = "Resolve short identifiers into canonical Darc identifiers.\n\nResolve commands emit JSON envelopes and are useful before session-scoped reads when a prefix is ambiguous."
    )]
    Resolve(ResolveArgs),
    #[command(
        about = "Manage configured Darc projects",
        long_about = "Manage configured Darc projects.\n\nProject commands inspect or update the shared Darc workspace configuration and project archive/index state."
    )]
    Project(ProjectArgs),
    #[command(
        about = "Check for or apply newer Darc releases",
        long_about = "Check for or apply newer Darc releases.\n\n`darc upgrade --check` compares the current CLI version with the latest GitHub Release.\n`darc upgrade` uses the release-installer updater when this installation includes one, and otherwise prints the manual installer command."
    )]
    Upgrade(UpgradeArgs),
    #[command(
        hide = true,
        about = "Link one configured project's historical paths into the current project",
        long_about = LINK_LONG_ABOUT
    )]
    Link(LinkArgs),
    #[command(
        hide = true,
        about = "Remove one configured project and its archived/indexed data",
        long_about = REMOVE_LONG_ABOUT
    )]
    Remove(RemoveArgs),
    #[command(
        hide = true,
        name = "rename-from",
        about = "Rebuild one old project's history into the current renamed project",
        long_about = RENAME_FROM_LONG_ABOUT
    )]
    RenameFrom(RenameArgs),
    #[command(
        about = "Sync matching Claude and Codex sessions into the project archive",
        long_about = "Sync matching Claude and Codex sessions into the active project's Darc archive.\n\nUse `--dry-run` to preview pending copies without writing archive files, manifests, or config.",
        after_help = sync_after_help()
    )]
    Sync(SyncArgs),
    #[command(
        about = "Index archived sessions for the active project into SQLite",
        long_about = "Index archived sessions for the active project into the shared Darc SQLite database.\n\nRun this after `darc sync` when you want to rebuild searchable/queryable state without copying new archive files.",
        after_help = index_after_help()
    )]
    Index(IndexArgs),
    #[command(
        about = "Manage the beta background Darc refresh service",
        long_about = "Manage the beta background Darc refresh service.\n\nThis service feature is currently beta and supports macOS LaunchAgents only.\nUse `darc refresh --watch --all` for the foreground process that the service runs in the background."
    )]
    Service(ServiceArgs),
    #[command(
        hide = true,
        about = "Audit Codex rollout schema compatibility against stable release tags",
        long_about = "Audit Codex rollout schema compatibility against stable release tags.\n\nThe audit fetches release metadata from GitHub Releases and may hit GitHub API rate limits when run anonymously.\n\nGitHub API authentication:\n- Prefer GH_TOKEN when it is set.\n- Otherwise use GITHUB_TOKEN.\n- Personal access tokens are accepted."
    )]
    CodexSchemaAudit(CodexSchemaAuditArgs),
    #[command(
        hide = true,
        about = "Audit Claude rollout transcript compatibility against published npm releases",
        long_about = "Audit Claude rollout transcript compatibility against published npm releases.\n\nThe audit downloads published @anthropic-ai/claude-code packages from the npm registry, runs deterministic fixture prompts against each audited version, and derives transcript schema manifests from the emitted local JSONL transcripts.\n\nSecurity note:\n- Darc does not provide an OS-level sandbox for executing published Claude packages.\n- You must pass `--use-host-auth` to run this hidden maintainer command.\n- When you do, the downloaded package executes with your host Claude login state plus an allowlist of Claude/cloud provider auth environment variables, not your full shell environment.\n\nRuntime requirements:\n- A working `node` runtime must be installed locally.\n- A working Python runtime (`python3` or `python`) must be installed locally for hook capture.\n- Claude authentication must be available through an existing local Claude login or the supported auth environment variables."
    )]
    ClaudeSchemaAudit(ClaudeSchemaAuditArgs),
}

/// Detect local sources and create the shared darc config.
#[derive(Debug, Args)]
struct InitArgs {
    #[arg(
        long,
        help_heading = "Mode",
        help = "Show what would be written without changing files"
    )]
    dry_run: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Create or update config under this Darc root"
    )]
    root: PathBuf,
}

/// Syncs then indexes archived sessions for one or all projects.
#[derive(Debug, Args)]
struct RefreshArgs {
    #[arg(
        long = "provider",
        value_enum,
        help_heading = "Selection",
        help = "Limit both sync and index to the selected providers"
    )]
    provider: Vec<ProviderArg>,

    #[arg(
        long,
        help_heading = "Scope",
        help = "Refresh every registered project, continue past per-project failures, and summarize the results"
    )]
    all: bool,

    #[arg(
        long,
        help_heading = "Mode",
        help = "Keep refreshing when Claude or Codex session files change"
    )]
    watch: bool,

    #[arg(
        long,
        value_name = "DURATION",
        requires = "watch",
        help_heading = "Mode",
        help = "Quiet period before a watched refresh, such as 30s or 2m"
    )]
    debounce: Option<String>,

    #[arg(
        long = "min-interval",
        value_name = "DURATION",
        requires = "watch",
        help_heading = "Mode",
        help = "Minimum time between watched refresh runs, such as 60s or 5m"
    )]
    min_interval: Option<String>,

    #[arg(
        long = "reconcile-interval",
        value_name = "DURATION",
        requires = "watch",
        help_heading = "Mode",
        help = "Periodic safety refresh interval for watch mode, such as 10m"
    )]
    reconcile_interval: Option<String>,

    #[arg(
        long,
        requires = "watch",
        help_heading = "Mode",
        help = "Use periodic polling instead of native filesystem events"
    )]
    poll: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    root: PathBuf,
}

/// Manages the background Darc refresh service.
#[derive(Debug, Args)]
struct ServiceArgs {
    #[arg(
        long,
        default_value_os_t = default_root_path(),
        global = true,
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    root: PathBuf,

    #[command(subcommand)]
    command: ServiceCommands,
}

/// Represents the supported service lifecycle commands.
#[derive(Debug, Subcommand)]
enum ServiceCommands {
    /// Start the macOS background refresh service now.
    Start,
    /// Stop the macOS background refresh service.
    Stop,
    /// Restart the macOS background refresh service.
    Restart,
    /// Show macOS background refresh service status.
    Status,
    /// Enable macOS auto-start on login for the refresh service.
    Enable,
    /// Disable macOS auto-start on login for the refresh service.
    Disable,
}

/// Checks for or applies newer Darc CLI releases.
#[derive(Debug, Args)]
struct UpgradeArgs {
    #[arg(
        long,
        help_heading = "Mode",
        help = "Only check whether a newer Darc release is available"
    )]
    check: bool,

    #[arg(
        long,
        requires = "check",
        help_heading = "Output",
        help = "Write the upgrade check result as a machine-readable JSON envelope"
    )]
    json: bool,
}

/// Shows Darc status for the active project or workspace.
#[derive(Debug, Args)]
struct StatusArgs {
    #[arg(
        long,
        help_heading = "Scope",
        help = "Show status for the shared Darc workspace instead of the active project"
    )]
    workspace: bool,

    #[arg(
        long,
        help_heading = "Mode",
        help = "Run sync planning without writing manifests, config, archives, or SQLite"
    )]
    check: bool,

    #[arg(
        long,
        help_heading = "Output",
        help = "Write status as a machine-readable JSON envelope"
    )]
    json: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    root: PathBuf,
}

/// Sync matching Claude and Codex sessions into the project archive.
#[derive(Debug, Args)]
struct SyncArgs {
    #[arg(
        long,
        help_heading = "Mode",
        help = "Preview pending copies without writing files"
    )]
    dry_run: bool,

    #[arg(
        long = "provider",
        value_enum,
        help_heading = "Selection",
        help = "Limit sync to the selected providers"
    )]
    provider: Vec<ProviderArg>,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    root: PathBuf,
}

/// Link one configured project's historical paths into the active project.
#[derive(Debug, Args)]
struct LinkArgs {
    #[arg(
        long,
        help_heading = "Mode",
        help = "Preview link changes without writing config"
    )]
    dry_run: bool,

    #[arg(
        value_name = "PROJECT",
        help = "Configured source project name to link into the current project"
    )]
    project: String,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    root: PathBuf,
}

/// Remove one configured project and its archived/indexed data.
#[derive(Debug, Args)]
struct RemoveArgs {
    #[arg(
        long,
        help_heading = "Mode",
        help = "Preview removal without writing config, archive, or index changes"
    )]
    dry_run: bool,

    #[arg(value_name = "PROJECT", help = "Configured project name to remove")]
    project: String,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    root: PathBuf,
}

/// Rebuild one configured project's history under the active project's id, then remove the old project.
#[derive(Debug, Args)]
struct RenameArgs {
    #[arg(
        long,
        help_heading = "Mode",
        help = "Preview rename workflow without writing config, archive, or index changes"
    )]
    dry_run: bool,

    #[arg(
        value_name = "PROJECT",
        help = "Old configured project name to rebuild into the current project"
    )]
    project: String,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    root: PathBuf,
}

/// Index archived sessions from selected providers for the active project into SQLite.
#[derive(Debug, Args)]
struct IndexArgs {
    #[arg(
        long = "provider",
        value_enum,
        help_heading = "Selection",
        help = "Limit indexing to the selected providers"
    )]
    provider: Vec<ProviderArg>,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    root: PathBuf,
}

/// Lists indexed Darc resources through the canonical JSON read surface.
#[derive(Debug, Args)]
struct ListArgs {
    #[arg(
        long,
        default_value_os_t = default_root_path(),
        global = true,
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,

    #[arg(
        long,
        value_enum,
        default_value_t = ColorArg::Auto,
        global = true,
        help_heading = "Presentation",
        help = "Control ANSI color in JSON output"
    )]
    color: ColorArg,

    #[command(subcommand)]
    command: ListCommands,
}

/// Represents the supported canonical list commands.
#[derive(Debug, Subcommand)]
enum ListCommands {
    /// List configured projects from the Darc workspace.
    Projects(QueryWorkspaceArgs),
    /// List sessions for one configured project.
    Sessions(ListSessionsArgs),
    /// List turns for one session.
    Turns(QueryTurnsArgs),
    /// List most-touched files, sessions touching one path, session files, or co-touched files.
    #[command(
        long_about = "List most-touched files, sessions touching one path, session files, or co-touched files.\n\nWith no mode flag, this ranks files by touches across the project. Pass PATH or --path to return sessions that touched matching paths. Use `--session` for the paginated per-session file summary. Use `--co-touched-with` for files touched in the same sessions as a seed path."
    )]
    Files(ListFilesArgs),
}

/// Shows indexed Darc resources through the canonical JSON read surface.
#[derive(Debug, Args)]
struct ShowArgs {
    #[arg(
        long,
        default_value_os_t = default_root_path(),
        global = true,
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,

    #[arg(
        long,
        value_enum,
        default_value_t = ColorArg::Auto,
        global = true,
        help_heading = "Presentation",
        help = "Control ANSI color in JSON output"
    )]
    color: ColorArg,

    #[command(subcommand)]
    command: ShowCommands,
}

/// Represents the supported canonical show commands.
#[derive(Debug, Subcommand)]
enum ShowCommands {
    /// Show the workspace/sidebar payload for one Darc root.
    Workspace(QueryWorkspaceArgs),
    /// Show one bounded session bundle.
    #[command(
        long_about = "Show one bounded session bundle.\n\nThe response contains a compact session summary, paginated turn details, and a capped session-file preview. Use --turn-limit/--turn-offset to page turns, --step-limit/--step-offset to page steps inside each returned turn, and `darc list files --session <SESSION_ID>` when the full session file list is needed."
    )]
    Session(QuerySessionBundleArgs),
    /// Show one turn detail payload.
    Turn(QueryTurnArgs),
}

/// Searches indexed turns through the canonical JSON read surface.
#[derive(Debug, Args)]
struct SearchArgs {
    #[arg(
        long,
        value_enum,
        default_value_t = SearchModeArg::Keyword,
        help_heading = "Search",
        help = "Choose how QUERY is interpreted"
    )]
    mode: SearchModeArg,

    #[arg(
        value_name = "QUERY",
        help = "Search query text or path pattern",
        long_help = search_query_help()
    )]
    query_arg: Option<String>,

    #[arg(
        long,
        allow_hyphen_values = true,
        value_name = "QUERY",
        help_heading = "Search",
        help = "Pass QUERY by flag; use this when the value starts with '-'"
    )]
    query: Option<String>,

    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Search this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict search to this provider"
    )]
    provider: Option<ProviderArg>,

    #[arg(
        long = "session-id",
        alias = "session",
        help_heading = "Scope",
        help = "Restrict search to this session id or unambiguous UUID prefix"
    )]
    session_id: Option<String>,

    #[arg(
        long,
        help_heading = "Evidence",
        help = "Include tool output evidence in literal and regex search"
    )]
    include_tool_output: bool,

    #[arg(
        long = "field",
        value_name = "FIELD",
        value_parser = parse_search_evidence_field,
        help_heading = "Evidence",
        help = search_evidence_field_include_help()
    )]
    fields: Vec<SearchEvidenceField>,

    #[arg(
        long = "exclude-field",
        value_name = "FIELD",
        value_parser = parse_search_evidence_field,
        help_heading = "Evidence",
        help = search_evidence_field_exclude_help()
    )]
    excluded_fields: Vec<SearchEvidenceField>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Inclusive started_at lower bound. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    since: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Exclusive started_at upper bound. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    until: Option<String>,

    #[arg(
        long,
        default_value_t = DEFAULT_QUERY_PAGE_LIMIT,
        help_heading = "Result Size",
        help = "Maximum turn hits to return"
    )]
    limit: usize,

    #[arg(
        long,
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of turn hits to skip"
    )]
    offset: usize,

    #[arg(
        long = "matched-path-limit",
        default_value_t = DEFAULT_MATCHED_PATH_LIMIT,
        conflicts_with = "include_all_matched_paths",
        help_heading = "Result Size",
        help = "Maximum matched_paths entries per file-search hit"
    )]
    matched_path_limit: usize,

    #[arg(
        long = "match-limit",
        value_name = "MATCH_LIMIT",
        help_heading = "Result Size",
        help = search_match_limit_help()
    )]
    match_limit: Option<usize>,

    #[arg(
        long = "include-all-matched-paths",
        help_heading = "Result Size",
        help = "Return every matched path in file-search hits"
    )]
    include_all_matched_paths: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,

    #[arg(
        long,
        value_enum,
        default_value_t = ColorArg::Auto,
        help_heading = "Presentation",
        help = "Control ANSI color in JSON output"
    )]
    color: ColorArg,
}

/// Shows indexed stats through the canonical JSON read surface.
#[derive(Debug, Args)]
struct StatsArgs {
    #[arg(
        long,
        default_value_os_t = default_root_path(),
        global = true,
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,

    #[arg(
        long,
        value_enum,
        default_value_t = ColorArg::Auto,
        global = true,
        help_heading = "Presentation",
        help = "Control ANSI color in JSON output"
    )]
    color: ColorArg,

    #[command(subcommand)]
    command: StatsCommands,
}

/// Represents the supported canonical stats commands.
#[derive(Debug, Subcommand)]
enum StatsCommands {
    /// Show workspace stats for one rolling day window.
    Workspace(QueryWorkspaceInsightsArgs),
    /// Show project stats for one configured project.
    Project(QueryProjectInsightsArgs),
    /// Show one turn's derived stats.
    Turn(QueryTurnInsightsArgs),
}

/// Resolves Darc identifiers through the canonical JSON read surface.
#[derive(Debug, Args)]
struct ResolveArgs {
    #[arg(
        long,
        default_value_os_t = default_root_path(),
        global = true,
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,

    #[arg(
        long,
        value_enum,
        default_value_t = ColorArg::Auto,
        global = true,
        help_heading = "Presentation",
        help = "Control ANSI color in JSON output"
    )]
    color: ColorArg,

    #[command(subcommand)]
    command: ResolveCommands,
}

/// Represents the supported canonical resolver commands.
#[derive(Debug, Subcommand)]
enum ResolveCommands {
    /// Resolve a full session id or UUID prefix into canonical matches.
    Session(QueryResolveSessionArgs),
}

/// Manages configured projects through the canonical project namespace.
#[derive(Debug, Args)]
struct ProjectArgs {
    #[command(subcommand)]
    command: ProjectCommands,
}

/// Represents the supported project-management commands.
#[derive(Debug, Subcommand)]
enum ProjectCommands {
    /// Link one configured project's historical paths into the current project.
    #[command(
        about = "Link one configured project's historical paths into the current project",
        long_about = LINK_LONG_ABOUT
    )]
    Link(LinkArgs),
    /// Remove one configured project and its archived/indexed data.
    #[command(
        about = "Remove one configured project and its archived/indexed data",
        long_about = REMOVE_LONG_ABOUT
    )]
    Remove(RemoveArgs),
    /// Rebuild one old project's history into the current renamed project.
    #[command(
        name = "rename-from",
        about = "Rebuild one old project's history into the current renamed project",
        long_about = RENAME_FROM_LONG_ABOUT
    )]
    RenameFrom(RenameArgs),
}

/// Queries the workspace/sidebar payload for one darc root.
#[derive(Debug, Args)]
struct QueryWorkspaceArgs {
    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,
}

/// Resolves one full session id or UUID prefix into canonical project/provider/session matches.
#[derive(Debug, Args)]
struct QueryResolveSessionArgs {
    #[arg(help = "Resolve this full UUID or UUID prefix")]
    input: String,

    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Restrict matches to this configured project id"
    )]
    project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict matches to this provider"
    )]
    provider: Option<ProviderArg>,

    #[arg(
        long,
        help_heading = "Output",
        help = "Require exactly one match and return it as one convenience object"
    )]
    pick_one: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,
}

/// Queries the session list for one configured project.
#[derive(Debug, Args)]
struct QuerySessionsArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict sessions to this provider"
    )]
    provider: Option<ProviderArg>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Inclusive latest_turn_at lower bound. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    since: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Exclusive latest_turn_at upper bound. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    until: Option<String>,

    #[arg(
        long = "touched-path",
        help_heading = "Selection",
        help = "Only keep sessions that touched a file path matching this glob"
    )]
    touched_path: Option<String>,

    #[arg(
        long,
        default_value_t = DEFAULT_QUERY_PAGE_LIMIT,
        help_heading = "Result Size",
        help = "Maximum sessions to return"
    )]
    limit: usize,

    #[arg(
        long,
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of sessions to skip"
    )]
    offset: usize,

    #[arg(
        long,
        value_enum,
        default_value_t = SessionListViewArg::Compact,
        help_heading = "Output",
        help = "Return full session prompts and final messages or compact previews"
    )]
    view: SessionListViewArg,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,
}

/// Lists sessions for one configured project through the canonical read surface.
#[derive(Debug, Args)]
struct ListSessionsArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "List sessions for this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict sessions to this provider"
    )]
    provider: Option<ProviderArg>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Inclusive latest_turn_at lower bound. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    since: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Exclusive latest_turn_at upper bound. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    until: Option<String>,

    #[arg(
        long = "touching",
        alias = "touched-path",
        help_heading = "Selection",
        help = "Only keep sessions that touched a file path matching this glob"
    )]
    touching: Option<String>,

    #[arg(
        long,
        default_value_t = DEFAULT_QUERY_PAGE_LIMIT,
        help_heading = "Result Size",
        help = "Maximum sessions to return"
    )]
    limit: usize,

    #[arg(
        long,
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of sessions to skip"
    )]
    offset: usize,

    #[arg(
        long,
        value_enum,
        default_value_t = SessionListViewArg::Compact,
        help_heading = "Output",
        help = "Return full session prompts and final messages or compact previews"
    )]
    view: SessionListViewArg,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,
}

/// Queries the turn list for one session.
#[derive(Debug, Args)]
struct QueryTurnsArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Disambiguate a session id or UUID prefix by provider"
    )]
    provider: Option<ProviderArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Session id or unambiguous UUID prefix to list turns for; required unless --session-id is set"
    )]
    session_id_arg: Option<String>,

    #[arg(
        long = "session-id",
        value_name = "SESSION_ID",
        help_heading = "Identity",
        help = "Session id or unambiguous UUID prefix to list turns for; alternative to positional SESSION_ID"
    )]
    session_id: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Inclusive started_at lower bound. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    since: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Exclusive started_at upper bound. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    until: Option<String>,

    #[arg(
        long,
        default_value_t = DEFAULT_QUERY_PAGE_LIMIT,
        help_heading = "Result Size",
        help = "Maximum turns to return"
    )]
    limit: usize,

    #[arg(
        long,
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of turns to skip"
    )]
    offset: usize,

    #[arg(
        long,
        value_enum,
        default_value_t = TurnListViewArg::Full,
        help_heading = "Output",
        help = "Return full turn summaries or a compact one-line skim"
    )]
    view: TurnListViewArg,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,
}

/// Lists most-touched files or pivots from one file selector.
#[derive(Debug, Args)]
struct QueryFilesArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict file pivots to this provider"
    )]
    provider: Option<ProviderArg>,

    #[arg(
        long,
        help_heading = "Selection",
        help = "Return sessions that touched file paths matching this glob instead of most-touched files"
    )]
    path: Option<String>,

    #[arg(
        value_name = "PATH",
        help = "Return sessions that touched this path or glob instead of most-touched files"
    )]
    path_arg: Option<String>,

    #[arg(
        long = "co-touched-with",
        help_heading = "Selection",
        help = "Return files touched in the same sessions as this seed path instead of most-touched files"
    )]
    co_touched_with: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Inclusive started_at lower bound for file pivots. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    since: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Exclusive started_at upper bound for file pivots. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    until: Option<String>,

    #[arg(
        long,
        default_value_t = DEFAULT_QUERY_PAGE_LIMIT,
        help_heading = "Result Size",
        help = "Maximum rows to return"
    )]
    limit: usize,

    #[arg(
        long,
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of rows to skip"
    )]
    offset: usize,

    #[arg(
        long = "matched-path-limit",
        default_value_t = DEFAULT_MATCHED_PATH_LIMIT,
        conflicts_with = "include_all_matched_paths",
        help_heading = "Result Size",
        help = "Maximum matched_paths entries per path-mode row"
    )]
    matched_path_limit: usize,

    #[arg(
        long = "include-all-matched-paths",
        help_heading = "Result Size",
        help = "Return every matched path in path-mode rows"
    )]
    include_all_matched_paths: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,
}

/// Lists files for one project or session through the canonical read surface.
#[derive(Debug, Args)]
struct ListFilesArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "List files for this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict file queries to this provider"
    )]
    provider: Option<ProviderArg>,

    #[arg(
        long,
        help_heading = "Selection",
        help = "Return sessions that touched file paths matching this glob instead of most-touched files"
    )]
    path: Option<String>,

    #[arg(
        value_name = "PATH",
        help = "Return sessions that touched this path or glob instead of most-touched files"
    )]
    path_arg: Option<String>,

    #[arg(
        long,
        value_name = "SESSION_ID",
        help_heading = "Selection",
        help = "Return a paginated per-session file summary for this session id or unambiguous UUID prefix"
    )]
    session: Option<String>,

    #[arg(
        long = "co-touched-with",
        help_heading = "Selection",
        help = "Return files touched in the same sessions as this seed path instead of most-touched files"
    )]
    co_touched_with: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Inclusive started_at lower bound for top/path/co-touch modes. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    since: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Exclusive started_at upper bound for top/path/co-touch modes. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    until: Option<String>,

    #[arg(
        long,
        help_heading = "Result Size",
        help = default_query_page_limit_help(
            "Maximum rows to return in top/path/co-touch/session modes"
        )
    )]
    limit: Option<usize>,

    #[arg(
        long,
        help_heading = "Result Size",
        help = "Number of rows to skip in top/path/co-touch/session modes [default: 0]"
    )]
    offset: Option<usize>,

    #[arg(
        long = "matched-path-limit",
        conflicts_with = "include_all_matched_paths",
        help_heading = "Result Size",
        help = "Maximum matched_paths entries per path-mode row [default: 20]"
    )]
    matched_path_limit: Option<usize>,

    #[arg(
        long = "include-all-matched-paths",
        help_heading = "Result Size",
        help = "Return every matched path in path-mode rows"
    )]
    include_all_matched_paths: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,
}

/// Queries one session-scoped per-file access summary payload.
#[derive(Debug, Args)]
struct QuerySessionFilesArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Disambiguate a session id or UUID prefix by provider"
    )]
    provider: Option<ProviderArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Query this session id or unambiguous UUID prefix; required unless --session-id is set"
    )]
    session_id_arg: Option<String>,

    #[arg(
        long = "session-id",
        value_name = "SESSION_ID",
        help_heading = "Identity",
        help = "Query this session id or unambiguous UUID prefix; alternative to positional SESSION_ID"
    )]
    session_id: Option<String>,

    #[arg(
        long,
        default_value_t = DEFAULT_QUERY_PAGE_LIMIT,
        help_heading = "Result Size",
        help = "Maximum file rows to return"
    )]
    limit: usize,

    #[arg(
        long,
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of file rows to skip"
    )]
    offset: usize,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,
}

/// Queries one composite session bundle payload.
#[derive(Debug, Args)]
struct QuerySessionBundleArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Disambiguate a session id or UUID prefix by provider"
    )]
    provider: Option<ProviderArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Query this session id or unambiguous UUID prefix; required unless --session-id is set"
    )]
    session_id_arg: Option<String>,

    #[arg(
        long = "session-id",
        value_name = "SESSION_ID",
        help_heading = "Identity",
        help = "Query this session id or unambiguous UUID prefix; alternative to positional SESSION_ID"
    )]
    session_id: Option<String>,

    #[arg(
        long = "session-view",
        value_enum,
        default_value_t = SessionListViewArg::Compact,
        help_heading = "Output",
        help = "Return full session prompt/final message or compact previews"
    )]
    session_view: SessionListViewArg,

    #[arg(
        long,
        value_enum,
        default_value_t = ViewArg::Narrative,
        help_heading = "Output",
        help = "Turn detail level. `narrative` omits tool arguments, outputs, and payload blobs"
    )]
    view: ViewArg,

    #[arg(
        long = "turn-limit",
        default_value_t = DEFAULT_SESSION_BUNDLE_TURN_LIMIT,
        help_heading = "Result Size",
        help = "Maximum turn details to return"
    )]
    turn_limit: usize,

    #[arg(
        long = "turn-offset",
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of turn details to skip"
    )]
    turn_offset: usize,

    #[arg(
        long = "step-limit",
        default_value_t = DEFAULT_TURN_STEP_LIMIT,
        help_heading = "Result Size",
        help = "Maximum steps to return per turn detail"
    )]
    step_limit: usize,

    #[arg(
        long = "step-offset",
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of steps to skip per turn detail"
    )]
    step_offset: usize,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,
}

/// Queries one turn detail payload.
#[derive(Debug, Args)]
struct QueryTurnArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Disambiguate a session id or UUID prefix by provider"
    )]
    provider: Option<ProviderArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Query this session id or unambiguous UUID prefix; required unless --session-id is set"
    )]
    session_id_arg: Option<String>,

    #[arg(
        value_name = "TURN_ORDINAL",
        help = "Query this turn ordinal; required unless --turn-ordinal is set"
    )]
    turn_ordinal_arg: Option<String>,

    #[arg(
        long = "session-id",
        value_name = "SESSION_ID",
        help_heading = "Identity",
        help = "Query this session id or unambiguous UUID prefix; alternative to positional SESSION_ID"
    )]
    session_id: Option<String>,

    #[arg(
        long = "turn-ordinal",
        value_name = "TURN_ORDINAL",
        help_heading = "Identity",
        help = "Query this turn ordinal; alternative to positional TURN_ORDINAL"
    )]
    turn_ordinal: Option<u64>,

    #[arg(
        long,
        value_enum,
        help_heading = "Output",
        help = "Step detail level. Defaults to narrative unless --include-raw is set; `narrative` omits tool arguments, outputs, and payload blobs"
    )]
    view: Option<ViewArg>,

    #[arg(
        long,
        help_heading = "Output",
        help = "Include optional raw/debug fields such as raw_steps_json"
    )]
    include_raw: bool,

    #[arg(
        long,
        help_heading = "Output",
        help = "Include one derived insights block with metrics plus tool and file analytics"
    )]
    include_insights: bool,

    #[arg(
        long = "step-limit",
        default_value_t = DEFAULT_TURN_STEP_LIMIT,
        help_heading = "Result Size",
        help = "Maximum steps to return"
    )]
    step_limit: usize,

    #[arg(
        long = "step-offset",
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of steps to skip"
    )]
    step_offset: usize,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,
}

/// Queries paginated turn search results for one configured project.
#[derive(Debug, Args)]
struct QuerySearchTurnsArgs {
    #[arg(
        long,
        value_enum,
        default_value_t = SearchModeArg::Keyword,
        help_heading = "Search",
        help = "Choose how QUERY is interpreted"
    )]
    mode: SearchModeArg,

    #[arg(
        value_name = "QUERY",
        help = "Search query text or path pattern",
        long_help = search_query_help()
    )]
    query_arg: Option<String>,

    #[arg(
        long,
        allow_hyphen_values = true,
        value_name = "QUERY",
        help_heading = "Search",
        help = "Pass QUERY by flag; use this when the value starts with '-'"
    )]
    query: Option<String>,

    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict search to this provider"
    )]
    provider: Option<ProviderArg>,

    #[arg(
        long = "session-id",
        help_heading = "Scope",
        help = "Restrict search to this session id or unambiguous UUID prefix"
    )]
    session_id: Option<String>,

    #[arg(
        long,
        help_heading = "Evidence",
        help = "Include tool output evidence in literal and regex search"
    )]
    include_tool_output: bool,

    #[arg(
        long = "field",
        value_name = "FIELD",
        value_parser = parse_search_evidence_field,
        help_heading = "Evidence",
        help = search_evidence_field_include_help()
    )]
    fields: Vec<SearchEvidenceField>,

    #[arg(
        long = "exclude-field",
        value_name = "FIELD",
        value_parser = parse_search_evidence_field,
        help_heading = "Evidence",
        help = search_evidence_field_exclude_help()
    )]
    excluded_fields: Vec<SearchEvidenceField>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Inclusive started_at lower bound. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    since: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Exclusive started_at upper bound. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    until: Option<String>,

    #[arg(
        long,
        default_value_t = DEFAULT_QUERY_PAGE_LIMIT,
        help_heading = "Result Size",
        help = "Maximum turn hits to return"
    )]
    limit: usize,

    #[arg(
        long,
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of turn hits to skip"
    )]
    offset: usize,

    #[arg(
        long = "matched-path-limit",
        default_value_t = DEFAULT_MATCHED_PATH_LIMIT,
        conflicts_with = "include_all_matched_paths",
        help_heading = "Result Size",
        help = "Maximum matched_paths entries per file-search hit"
    )]
    matched_path_limit: usize,

    #[arg(
        long = "match-limit",
        value_name = "MATCH_LIMIT",
        help_heading = "Result Size",
        help = search_match_limit_help()
    )]
    match_limit: Option<usize>,

    #[arg(
        long = "include-all-matched-paths",
        help_heading = "Result Size",
        help = "Return every matched path in file-search hits"
    )]
    include_all_matched_paths: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,
}

/// Queries the workspace insights payload for one rolling day window.
#[derive(Debug, Args)]
struct QueryWorkspaceInsightsArgs {
    #[arg(
        long = "window",
        default_value = "7d",
        value_parser = parse_window_days,
        help_heading = "Time Window",
        help = "Rolling host-local day window in `<days>d` format"
    )]
    window_days: u32,

    #[arg(
        long = "recent-session-limit",
        default_value_t = DEFAULT_WORKSPACE_RECENT_SESSION_LIMIT,
        help_heading = "Result Size",
        help = "Maximum recent sessions to return"
    )]
    recent_session_limit: usize,

    #[arg(
        long = "recent-session-offset",
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of recent sessions to skip"
    )]
    recent_session_offset: usize,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,
}

/// Queries the project insights payload for one configured project.
#[derive(Debug, Args)]
struct QueryProjectInsightsArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict project insights to this provider"
    )]
    provider: Option<ProviderArg>,

    #[arg(
        long = "turn-limit",
        alias = "limit",
        default_value_t = 1000,
        help_heading = "Result Size",
        help = "Maximum indexed turns to inspect"
    )]
    turn_limit: usize,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,
}

/// Queries one turn insights payload.
#[derive(Debug, Args)]
struct QueryTurnInsightsArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Disambiguate a session id or UUID prefix by provider"
    )]
    provider: Option<ProviderArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Query this session id or unambiguous UUID prefix; required unless --session-id is set"
    )]
    session_id_arg: Option<String>,

    #[arg(
        value_name = "TURN_ORDINAL",
        help = "Query this turn ordinal; required unless --turn-ordinal is set"
    )]
    turn_ordinal_arg: Option<String>,

    #[arg(
        long = "session-id",
        value_name = "SESSION_ID",
        help_heading = "Identity",
        help = "Query this session id or unambiguous UUID prefix; alternative to positional SESSION_ID"
    )]
    session_id: Option<String>,

    #[arg(
        long = "turn-ordinal",
        value_name = "TURN_ORDINAL",
        help_heading = "Identity",
        help = "Query this turn ordinal; alternative to positional TURN_ORDINAL"
    )]
    turn_ordinal: Option<u64>,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    root: PathBuf,
}

/// Audit Codex rollout schema compatibility against stable release tags.
#[derive(Debug, Args)]
struct CodexSchemaAuditArgs {
    #[arg(long, value_name = "DIR", help_heading = "Cache")]
    cache_dir: Option<PathBuf>,
}

/// Audit Claude rollout transcript compatibility against published npm releases.
#[derive(Debug, Args)]
struct ClaudeSchemaAuditArgs {
    #[arg(long, value_name = "DIR", help_heading = "Cache")]
    cache_dir: Option<PathBuf>,

    #[arg(long, default_value_t = 1, value_name = "N", help_heading = "Sampling")]
    sample_stride: usize,

    #[arg(long, help_heading = "Runtime")]
    use_host_auth: bool,

    #[arg(long, value_name = "VERSION", help_heading = "Scope")]
    from_version: Option<String>,

    #[arg(
        long,
        value_enum,
        default_value_t = ClaudeSurveyModeArg::Refine,
        help_heading = "Mode"
    )]
    survey_mode: ClaudeSurveyModeArg,
}

/// Represents the supported provider filters for index and sync.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderArg {
    Claude,
    Codex,
}

/// Represents the supported search modes for machine-readable turn search.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum SearchModeArg {
    Keyword,
    Literal,
    Regex,
    FileName,
    FilePath,
    PathFragment,
}

/// Represents when query JSON output should include ANSI color.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum ColorArg {
    Auto,
    Always,
    Never,
}

/// Stores the resolved output behavior for one query invocation.
#[derive(Debug, Clone, Copy)]
struct QueryOutput {
    color: ColorArg,
}

impl QueryOutput {
    /// Builds one query output context from parsed CLI arguments.
    fn new(color: ColorArg) -> Self {
        Self { color }
    }

    /// Returns whether stdout JSON should be ANSI-colored.
    fn should_color_stdout(self) -> bool {
        should_color_output(
            self.color,
            io::stdout().is_terminal(),
            env::var_os("NO_COLOR").is_some(),
            env::var("TERM").ok().as_deref(),
        )
    }
}

/// Stores metadata returned by the GitHub latest-release endpoint.
#[derive(Debug, Clone, Deserialize)]
struct GitHubLatestRelease {
    tag_name: String,
    html_url: String,
}

/// Stores the resolved Darc upgrade state.
#[derive(Debug, Clone)]
struct UpgradeStatus {
    current_version: String,
    latest_version: Option<String>,
    upgrade_available: bool,
    latest_release_url: Option<String>,
}

/// Stores the best-effort cached state for passive upgrade nudges.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct UpgradeNudgeCache {
    checked_at_unix: Option<u64>,
    last_notified_at_unix: Option<u64>,
    latest_version: Option<String>,
    latest_release_url: Option<String>,
    upgrade_available: bool,
}

/// Stores the machine-readable payload for `darc upgrade --check --json`.
#[derive(Debug, Serialize)]
struct UpgradeCheckJson<'a> {
    current_version: &'a str,
    latest_version: Option<&'a str>,
    upgrade_available: bool,
    latest_release_url: Option<&'a str>,
    install_command: &'static str,
}

impl<'a> From<&'a UpgradeStatus> for UpgradeCheckJson<'a> {
    /// Builds one JSON payload from a resolved upgrade status.
    fn from(status: &'a UpgradeStatus) -> Self {
        Self {
            current_version: &status.current_version,
            latest_version: status.latest_version.as_deref(),
            upgrade_available: status.upgrade_available,
            latest_release_url: status.latest_release_url.as_deref(),
            install_command: DARC_INSTALLER_COMMAND,
        }
    }
}

/// Stores one parsed semantic-ish release version.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedReleaseVersion {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Option<String>,
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
                (Some(left), Some(right)) => left.cmp(right),
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

/// Represents the supported session-list projections.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum SessionListViewArg {
    Compact,
    Full,
}

/// Represents the supported turn-list projections for machine-readable turn queries.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum TurnListViewArg {
    Full,
    Oneline,
}

/// Represents the supported turn-detail projection modes.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ViewArg {
    Full,
    Narrative,
}

/// Represents the supported Claude schema audit survey modes.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ClaudeSurveyModeArg {
    Refine,
    Coarse,
}

/// Parses CLI arguments and dispatches the selected command.
pub fn run() -> i32 {
    run_from(env::args_os())
}

/// Parses the provided CLI arguments and dispatches the selected command.
fn run_from<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    let mut command = cli_command();
    match command.try_get_matches_from_mut(args.clone()) {
        Ok(matches) => match Cli::from_arg_matches(&matches) {
            Ok(cli) => run_cli(cli),
            Err(error) => clap_error_exit(error, &args),
        },
        Err(error) => clap_error_exit(error, &args),
    }
}

/// Dispatches one already parsed CLI command.
fn run_cli(cli: Cli) -> i32 {
    match cli.command {
        Commands::Init(args) => standard_exit(run_init(args)),
        Commands::Refresh(args) => standard_exit(run_refresh(args)),
        Commands::Status(args) if args.json => json_exit(run_status(args)),
        Commands::Status(args) => standard_exit(run_status(args)),
        Commands::List(args) => query_exit(run_list(args)),
        Commands::Show(args) => query_exit(run_show(args)),
        Commands::Search(args) => query_exit(run_search(args)),
        Commands::Stats(args) => query_exit(run_stats(args)),
        Commands::Resolve(args) => query_exit(run_resolve(args)),
        Commands::Project(args) => standard_exit(run_project(args)),
        Commands::Upgrade(args) if args.json => json_exit(run_upgrade(args)),
        Commands::Upgrade(args) => standard_exit(run_upgrade(args)),
        Commands::Link(args) => standard_exit(run_link(args)),
        Commands::Remove(args) => standard_exit(run_remove(args)),
        Commands::RenameFrom(args) => standard_exit(run_rename_from(args)),
        Commands::Sync(args) => standard_exit(run_sync(args)),
        Commands::Index(args) => standard_exit(run_index(args)),
        Commands::Service(args) => standard_exit(run_service(args)),
        Commands::CodexSchemaAudit(args) => run_codex_schema_audit_command(args),
        Commands::ClaudeSchemaAudit(args) => run_claude_schema_audit_command(args),
    }
}

/// Maps Clap parse errors to the correct command-family output format.
fn clap_error_exit(error: clap::Error, args: &[OsString]) -> i32 {
    if is_json_read_invocation(args) && !is_clap_display_request(error.kind()) {
        eprintln!("{}", format_query_clap_error(&error));
        return error.exit_code();
    }

    if let Err(print_error) = error.print() {
        eprintln!("error: failed to write CLI error: {print_error}");
        return 1;
    }
    error.exit_code()
}

/// Returns whether the raw CLI arguments target one JSON output surface.
fn is_json_read_invocation(args: &[OsString]) -> bool {
    match args.get(1).and_then(|arg| arg.to_str()) {
        Some("list" | "show" | "search" | "stats" | "resolve") => true,
        Some("status" | "upgrade") => args.iter().any(|arg| arg == "--json"),
        _ => false,
    }
}

/// Returns whether Clap is carrying a normal display request instead of an error.
fn is_clap_display_request(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::DisplayHelp | ErrorKind::DisplayVersion)
}

/// Maps standard command results to the default CLI exit code convention.
fn standard_exit(result: Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error:#}");
            1
        }
    }
}

/// Maps JSON command results to machine-readable output.
fn json_exit(result: Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            let message = format_query_error(&error);
            eprintln!("{message}");
            1
        }
    }
}

/// Maps canonical query command results to JSON-only machine-readable output.
fn query_exit(result: Result<()>) -> i32 {
    json_exit(result)
}

/// Dispatches the supported canonical list commands.
fn run_list(args: ListArgs) -> Result<()> {
    let output = QueryOutput::new(args.color);
    let root = args.root;
    match args.command {
        ListCommands::Projects(mut args) => {
            args.root = root;
            run_query_workspace(&output, args)
        }
        ListCommands::Sessions(mut args) => {
            args.root = root;
            run_query_sessions(&output, args.into())
        }
        ListCommands::Turns(mut args) => {
            args.root = root;
            run_query_turns(&output, args)
        }
        ListCommands::Files(mut args) => {
            args.root = root;
            run_list_files(&output, args)
        }
    }
}

/// Dispatches the supported canonical show commands.
fn run_show(args: ShowArgs) -> Result<()> {
    let output = QueryOutput::new(args.color);
    let root = args.root;
    match args.command {
        ShowCommands::Workspace(mut args) => {
            args.root = root;
            run_query_workspace(&output, args)
        }
        ShowCommands::Session(mut args) => {
            args.root = root;
            run_query_session_bundle(&output, args)
        }
        ShowCommands::Turn(mut args) => {
            args.root = root;
            run_query_turn(&output, args)
        }
    }
}

/// Dispatches canonical turn search.
fn run_search(args: SearchArgs) -> Result<()> {
    let output = QueryOutput::new(args.color);
    run_query_search_turns(&output, args.into_query_search_turns_args()?)
}

/// Dispatches the supported canonical stats commands.
fn run_stats(args: StatsArgs) -> Result<()> {
    let output = QueryOutput::new(args.color);
    let root = args.root;
    match args.command {
        StatsCommands::Workspace(mut args) => {
            args.root = root;
            run_query_workspace_insights(&output, args)
        }
        StatsCommands::Project(mut args) => {
            args.root = root;
            run_query_project_insights(&output, args)
        }
        StatsCommands::Turn(mut args) => {
            args.root = root;
            run_query_turn_insights(&output, args)
        }
    }
}

/// Dispatches the supported canonical resolver commands.
fn run_resolve(args: ResolveArgs) -> Result<()> {
    let output = QueryOutput::new(args.color);
    let root = args.root;
    match args.command {
        ResolveCommands::Session(mut args) => {
            args.root = root;
            run_query_resolve_session(&output, args)
        }
    }
}

/// Dispatches the supported project-management commands.
fn run_project(args: ProjectArgs) -> Result<()> {
    match args.command {
        ProjectCommands::Link(args) => run_link(args),
        ProjectCommands::Remove(args) => run_remove(args),
        ProjectCommands::RenameFrom(args) => run_rename_from(args),
    }
}

/// Runs the explicit Darc CLI upgrade command.
fn run_upgrade(args: UpgradeArgs) -> Result<()> {
    let status = check_darc_upgrade(UPGRADE_CHECK_TIMEOUT)?;
    if args.json {
        return print_upgrade_check_json(&status);
    }

    if args.check {
        print_upgrade_check_report(&status);
        return Ok(());
    }

    run_darc_upgrade(status)
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
        print_line(2, format!("Run: {DARC_INSTALLER_COMMAND}"));
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
fn check_darc_upgrade(timeout: Duration) -> Result<UpgradeStatus> {
    let current_version = env!("CARGO_PKG_VERSION").to_owned();
    let Some(release) = fetch_latest_darc_release(timeout)? else {
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
fn fetch_latest_darc_release(timeout: Duration) -> Result<Option<GitHubLatestRelease>> {
    let client = build_upgrade_http_client(timeout)?;
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
fn build_upgrade_http_client(timeout: Duration) -> Result<Client> {
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
        .timeout(timeout)
        .build()
        .context("failed to build HTTP client for Darc upgrade check")
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
    let detail = body.trim();
    if detail.is_empty() {
        bail!("failed to {context_message}: GitHub returned HTTP {status}");
    }
    bail!("failed to {context_message}: GitHub returned HTTP {status}: {detail}")
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
fn release_version_is_newer(latest: &str, current: &str) -> Result<bool> {
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
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .and_then(|dir| upgrade_executable_at(&dir.join(upgrade_executable_name())))
}

/// Returns one updater path when the candidate exists as a file.
fn upgrade_executable_at(path: &Path) -> Option<PathBuf> {
    path.is_file().then(|| path.to_path_buf())
}

/// Returns the cargo-dist updater executable name for this platform.
fn upgrade_executable_name() -> String {
    format!("darc-update{}", env::consts::EXE_SUFFIX)
}

/// Best-effort passive nudge for newer Darc CLI releases.
fn maybe_print_upgrade_nudge(root: &Path) {
    if !upgrade_nudge_enabled_from_env() {
        return;
    }
    let Some(now) = current_unix_seconds() else {
        return;
    };
    let mut cache = read_upgrade_nudge_cache(root);
    if should_check_upgrade_nudge(now, &cache) {
        cache.checked_at_unix = Some(now);
        if let Ok(status) = check_darc_upgrade(UPGRADE_NUDGE_TIMEOUT) {
            cache.latest_version = status.latest_version;
            cache.latest_release_url = status.latest_release_url;
            cache.upgrade_available = status.upgrade_available;
        }
    }

    if should_notify_upgrade_nudge(now, &cache)
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
    }

    let _ = write_upgrade_nudge_cache(root, &cache);
}

/// Returns whether the current process should try passive upgrade nudges.
fn upgrade_nudge_enabled_from_env() -> bool {
    upgrade_nudge_enabled(
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
        env::var("TERM").ok().as_deref(),
        env::var_os("CI").is_some(),
        env::var_os("DARC_NO_UPDATE_CHECK").is_some(),
    )
}

/// Returns whether passive upgrade nudges are enabled for resolved process facts.
fn upgrade_nudge_enabled(
    stdout_is_terminal: bool,
    stderr_is_terminal: bool,
    term: Option<&str>,
    ci: bool,
    disabled: bool,
) -> bool {
    stdout_is_terminal && stderr_is_terminal && term != Some("dumb") && !ci && !disabled
}

/// Returns whether the cache is old enough for another network upgrade check.
fn should_check_upgrade_nudge(now: u64, cache: &UpgradeNudgeCache) -> bool {
    cache.checked_at_unix.is_none_or(|checked_at| {
        now.saturating_sub(checked_at) >= UPGRADE_NUDGE_CHECK_INTERVAL.as_secs()
    })
}

/// Returns whether a cached available upgrade should be shown again.
fn should_notify_upgrade_nudge(now: u64, cache: &UpgradeNudgeCache) -> bool {
    cache.upgrade_available
        && cache.latest_version.is_some()
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

/// Lists files through either project-wide or session-scoped query payloads.
fn run_list_files(output: &QueryOutput, args: ListFilesArgs) -> Result<()> {
    let path_selector_count =
        usize::from(args.path.is_some()) + usize::from(args.path_arg.is_some());
    let selector_count = path_selector_count
        + usize::from(args.session.is_some())
        + usize::from(args.co_touched_with.is_some());
    if selector_count > 1 {
        bail!("list files accepts at most one of PATH/--path, --session, or --co-touched-with");
    }
    let path = args.path.or(args.path_arg);
    if let Some(session_id) = args.session {
        if args.since.is_some()
            || args.until.is_some()
            || args.matched_path_limit.is_some()
            || args.include_all_matched_paths
        {
            bail!(
                "list files --session does not accept --since, --until, --matched-path-limit, or --include-all-matched-paths"
            );
        }
        return run_query_session_files(
            output,
            QuerySessionFilesArgs {
                root: args.root,
                project_id: args.project_id,
                provider: args.provider,
                session_id_arg: None,
                session_id: Some(session_id),
                limit: args.limit.unwrap_or(DEFAULT_QUERY_PAGE_LIMIT),
                offset: args.offset.unwrap_or(0),
            },
        );
    }
    if path.is_none() && (args.matched_path_limit.is_some() || args.include_all_matched_paths) {
        bail!("list files matched-path controls require PATH or --path");
    }
    run_query_files(
        output,
        QueryFilesArgs {
            root: args.root,
            project_id: args.project_id,
            provider: args.provider,
            path,
            path_arg: None,
            co_touched_with: args.co_touched_with,
            since: args.since,
            until: args.until,
            limit: args.limit.unwrap_or(DEFAULT_QUERY_PAGE_LIMIT),
            offset: args.offset.unwrap_or(0),
            matched_path_limit: args
                .matched_path_limit
                .unwrap_or(DEFAULT_MATCHED_PATH_LIMIT),
            include_all_matched_paths: args.include_all_matched_paths,
        },
    )
}

/// Queries the workspace/sidebar payload for one darc root.
fn run_query_workspace(output: &QueryOutput, args: QueryWorkspaceArgs) -> Result<()> {
    print_json_envelope(
        output,
        "darc.query.workspace.v1",
        &query_workspace(Some(args.root)),
    )
}

/// Resolves one full session id or UUID prefix into canonical matches.
fn run_query_resolve_session(output: &QueryOutput, args: QueryResolveSessionArgs) -> Result<()> {
    let data = query_resolve_sessions(
        Some(args.root),
        ResolveSessionQueryRequest {
            query: &args.input,
            project_id: args.project_id.as_deref(),
            provider: args.provider.map(provider_arg_to_source_kind),
            limit: DEFAULT_RESOLVE_SESSION_MATCH_LIMIT,
        },
    )?;
    if !args.pick_one {
        if data.matches.is_empty() && is_full_uuid_text(&data.query) {
            return Err(QueryProtocolError::unknown_resolve_session(&data.query, false).into());
        }
        return print_json_envelope(output, "darc.query.resolve_session.v1", &data);
    }

    match data.matches.as_slice() {
        [] => Err(QueryProtocolError::unknown_resolve_session(
            &data.query,
            !is_full_uuid_text(&data.query),
        )
        .into()),
        [resolved] => print_json_envelope(
            output,
            "darc.query.resolve_session.v1",
            &ResolveSessionPickOneQueryData::new(&data.query, resolved.clone()),
        ),
        _ => Err(
            QueryProtocolError::ambiguous_session(&data.query, data.matches, data.truncated).into(),
        ),
    }
}

/// Queries the session list for one configured project.
fn run_query_sessions(output: &QueryOutput, args: QuerySessionsArgs) -> Result<()> {
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let since = args
        .since
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let until = args
        .until
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let data = query_sessions_for_project(
        &project,
        SessionsQueryRequest {
            project_id: "",
            project_root: None,
            provider: args.provider.map(provider_arg_to_source_kind),
            since: since.as_deref(),
            until: until.as_deref(),
            touched_path: args.touched_path.as_deref(),
            view: session_list_view_arg_to_view(args.view),
            limit: args.limit,
            offset: args.offset,
        },
    )?;
    print_json_envelope(output, "darc.query.sessions.v1", &data)
}

/// Lists most-touched files or pivots from one file selector for one configured project.
fn run_query_files(output: &QueryOutput, args: QueryFilesArgs) -> Result<()> {
    let path = optional_named_or_positional(
        "file path",
        "--path",
        args.path.as_deref(),
        "PATH",
        args.path_arg.as_deref(),
    )?;
    if path.is_some() && args.co_touched_with.is_some() {
        bail!("list files accepts either PATH/--path or --co-touched-with, not both");
    }
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let since = args
        .since
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let until = args
        .until
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let data = query_files_for_project(
        &project,
        FilesQueryRequest {
            project_id: "",
            project_root: None,
            provider: args.provider.map(provider_arg_to_source_kind),
            path,
            co_touched_with: args.co_touched_with.as_deref(),
            since: since.as_deref(),
            until: until.as_deref(),
            limit: args.limit,
            offset: args.offset,
            matched_path_limit: matched_path_limit_arg(
                args.include_all_matched_paths,
                args.matched_path_limit,
            ),
        },
    )?;
    print_json_envelope(output, "darc.query.files.v1", &data)
}

/// Queries one session-scoped per-file access summary payload.
fn run_query_session_files(output: &QueryOutput, args: QuerySessionFilesArgs) -> Result<()> {
    let session_id = required_named_or_positional(
        "session id",
        "--session-id",
        args.session_id.as_deref(),
        "SESSION_ID",
        args.session_id_arg.as_deref(),
    )?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let session = resolve_query_session_for_project(
        &project,
        args.provider.map(provider_arg_to_source_kind),
        session_id,
    )?;
    let data = query_session_files_for_project(
        &project,
        session.provider,
        &session.session_id,
        args.limit,
        args.offset,
    )?;
    print_json_envelope(output, "darc.query.session_files.v1", &data)
}

/// Queries one composite session bundle payload.
fn run_query_session_bundle(output: &QueryOutput, args: QuerySessionBundleArgs) -> Result<()> {
    let session_id = required_named_or_positional(
        "session id",
        "--session-id",
        args.session_id.as_deref(),
        "SESSION_ID",
        args.session_id_arg.as_deref(),
    )?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let session = resolve_query_session_for_project(
        &project,
        args.provider.map(provider_arg_to_source_kind),
        session_id,
    )?;
    let data = query_session_bundle_for_project(
        &project,
        SessionBundleQueryRequest {
            project_id: "",
            provider: session.provider,
            session_id: &session.session_id,
            project_root: None,
            session_view: session_list_view_arg_to_view(args.session_view),
            view: view_arg_to_session_bundle_view(args.view),
            turn_limit: args.turn_limit,
            turn_offset: args.turn_offset,
            step_limit: args.step_limit,
            step_offset: args.step_offset,
        },
    )?;
    print_json_envelope(output, "darc.query.session_bundle.v1", &data)
}

/// Queries the turn list for one session.
fn run_query_turns(output: &QueryOutput, args: QueryTurnsArgs) -> Result<()> {
    let session_id = required_named_or_positional(
        "session id",
        "--session-id",
        args.session_id.as_deref(),
        "SESSION_ID",
        args.session_id_arg.as_deref(),
    )?;
    let since = args
        .since
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let until = args
        .until
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let session = resolve_query_session_for_project(
        &project,
        args.provider.map(provider_arg_to_source_kind),
        session_id,
    )?;
    let data = query_turns_for_project(
        &project,
        TurnsQueryRequest {
            project_id: "",
            provider: session.provider,
            session_id: &session.session_id,
            since: since.as_deref(),
            until: until.as_deref(),
            view: turn_list_view_arg_to_view(args.view),
            limit: args.limit,
            offset: args.offset,
        },
    )?;
    print_turns_query_envelope(output, &data)
}

/// Queries one turn detail payload.
fn run_query_turn(output: &QueryOutput, args: QueryTurnArgs) -> Result<()> {
    let (session_id, turn_ordinal) = resolve_turn_identity_args(
        args.session_id.as_deref(),
        args.turn_ordinal,
        args.session_id_arg.as_deref(),
        args.turn_ordinal_arg.as_deref(),
    )?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let session = resolve_query_session_for_project(
        &project,
        args.provider.map(provider_arg_to_source_kind),
        session_id,
    )?;
    let view = match (args.view, args.include_raw) {
        (Some(ViewArg::Narrative), true) => {
            bail!("--include-raw requires --view full; omit --view to let --include-raw imply full")
        }
        (Some(view), _) => view,
        (None, true) => ViewArg::Full,
        (None, false) => ViewArg::Narrative,
    };
    let data = query_turn_for_project(
        &project,
        session.provider,
        &session.session_id,
        turn_ordinal,
        TurnDetailOptions {
            include_raw: args.include_raw,
            include_insights: args.include_insights,
            narrative: matches!(view, ViewArg::Narrative),
            step_limit: args.step_limit,
            step_offset: args.step_offset,
        },
    )?;
    print_json_envelope(output, "darc.query.turn.v1", &data)
}

/// Queries one paginated turn-search payload.
fn run_query_search_turns(output: &QueryOutput, args: QuerySearchTurnsArgs) -> Result<()> {
    let query = required_named_or_positional(
        "query text",
        "--query",
        args.query.as_deref(),
        "QUERY",
        args.query_arg.as_deref(),
    )?;
    let since = args
        .since
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let until = args
        .until
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let session_id = args
        .session_id
        .as_deref()
        .map(|session_id| {
            resolve_query_search_session_id_for_project(
                &project,
                args.provider.map(provider_arg_to_source_kind),
                session_id,
            )
        })
        .transpose()?;
    let mode = search_mode_arg_to_search_mode(args.mode);
    let data = query_search_turns_for_project(
        &project,
        SearchTurnsRequest {
            project_id: "",
            project_root: None,
            mode,
            query,
            include_tool_output: args.include_tool_output,
            fields: &args.fields,
            excluded_fields: &args.excluded_fields,
            provider: args.provider.map(provider_arg_to_source_kind),
            session_id: session_id.as_deref(),
            since: since.as_deref(),
            until: until.as_deref(),
            limit: args.limit,
            offset: args.offset,
            matched_path_limit: matched_path_limit_arg(
                args.include_all_matched_paths,
                args.matched_path_limit,
            ),
            match_limit: args.match_limit,
        },
    )?;
    print_search_turns_json_envelope(output, &data)
}

/// Queries the workspace insights payload for one rolling host-local day window.
fn run_query_workspace_insights(
    output: &QueryOutput,
    args: QueryWorkspaceInsightsArgs,
) -> Result<()> {
    let data = query_workspace_insight_report(
        Some(args.root),
        args.window_days,
        args.recent_session_limit,
        args.recent_session_offset,
    )?;
    print_json_envelope(output, "darc.query.insights.workspace.v1", &data)
}

/// Queries the project insights payload for one configured project.
fn run_query_project_insights(output: &QueryOutput, args: QueryProjectInsightsArgs) -> Result<()> {
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let data = query_project_insight_report_for_project(
        &project,
        args.provider.map(provider_arg_to_source_kind),
        args.turn_limit,
    )?;
    print_json_envelope(output, "darc.query.insights.project.v1", &data)
}

/// Queries the turn insights payload for one session turn.
fn run_query_turn_insights(output: &QueryOutput, args: QueryTurnInsightsArgs) -> Result<()> {
    let (session_id, turn_ordinal) = resolve_turn_identity_args(
        args.session_id.as_deref(),
        args.turn_ordinal,
        args.session_id_arg.as_deref(),
        args.turn_ordinal_arg.as_deref(),
    )?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let session = resolve_query_session_for_project(
        &project,
        args.provider.map(provider_arg_to_source_kind),
        session_id,
    )?;
    let data = query_turn_insight_report_for_project(
        &project,
        session.provider,
        &session.session_id,
        turn_ordinal,
    )?;
    print_json_envelope(output, "darc.query.insights.turn.v1", &data)
}

/// Writes one machine-readable JSON envelope to stdout.
fn print_json_envelope<T: Serialize>(
    output: &QueryOutput,
    schema: &'static str,
    data: &T,
) -> Result<()> {
    let json = render_json_envelope(schema, data)?;
    print_query_json(output, &json);
    Ok(())
}

/// Returns one serialized machine-readable JSON envelope.
fn render_json_envelope<T: Serialize>(schema: &'static str, data: &T) -> Result<String> {
    let payload = JsonEnvelope {
        schema,
        generated_at: current_utc_timestamp(),
        darc_version: env!("CARGO_PKG_VERSION"),
        data,
    };
    serde_json::to_string_pretty(&payload).context("failed to serialize query response JSON")
}

/// Writes one rendered query JSON document to stdout.
fn print_query_json(output: &QueryOutput, json: &str) {
    if output.should_color_stdout() {
        println!("{}", color_json(json));
    } else {
        println!("{json}");
    }
}

/// Writes one search-turns envelope with optional snippet match highlighting.
fn print_search_turns_json_envelope(
    output: &QueryOutput,
    data: &SearchTurnsQueryData,
) -> Result<()> {
    let json = render_json_envelope("darc.query.search.turns.v1", data)?;
    if output.should_color_stdout() {
        println!("{}", color_search_turns_json(&json, data)?);
    } else {
        println!("{json}");
    }
    Ok(())
}

/// Writes one `darc.query.turns.v1` envelope, compacting rows when `view` is `oneline`.
fn print_turns_query_envelope(
    output: &QueryOutput,
    data: &darc_core::query::TurnsQueryData,
) -> Result<()> {
    match data.view {
        TurnsView::Full => print_json_envelope(output, "darc.query.turns.v1", data),
        TurnsView::Oneline => print_json_envelope(
            output,
            "darc.query.turns.v1",
            &TurnsOnelineQueryData::from_turns_query(data),
        ),
    }
}

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";

// JSON syntax colors intentionally stay separate from human report colors.
const ANSI_KEY: &str = "\x1b[1;34m";
const ANSI_STRING: &str = "\x1b[32m";
const ANSI_NUMBER: &str = "\x1b[33m";
const ANSI_BOOLEAN: &str = "\x1b[35m";
const ANSI_NULL: &str = "\x1b[36m";
const ANSI_MATCH: &str = "\x1b[1;95m";

// Runtime report colors keep structure quiet and reserve hues for state.
const ANSI_RED: &str = "\x1b[31m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_GREEN: &str = ANSI_STRING;
const ANSI_YELLOW: &str = ANSI_NUMBER;
const ANSI_CYAN: &str = ANSI_NULL;

/// Stores whether human-oriented CLI output should use terminal styling.
#[derive(Debug, Clone, Copy)]
struct HumanStyle {
    enabled: bool,
}

impl HumanStyle {
    /// Builds one style context for stdout.
    fn stdout() -> Self {
        Self::new(
            io::stdout().is_terminal(),
            env::var_os("NO_COLOR").is_some(),
            env::var("TERM").ok().as_deref(),
        )
    }

    /// Builds one style context for stderr.
    fn stderr() -> Self {
        Self::new(
            io::stderr().is_terminal(),
            env::var_os("NO_COLOR").is_some(),
            env::var("TERM").ok().as_deref(),
        )
    }

    /// Builds one style context from resolved terminal environment facts.
    fn new(is_terminal: bool, no_color: bool, term: Option<&str>) -> Self {
        Self {
            enabled: should_auto_color_output(is_terminal, no_color, term),
        }
    }

    /// Returns one string wrapped with an ANSI style when styling is enabled.
    fn color(self, code: &str, value: impl std::fmt::Display) -> String {
        if self.enabled {
            format!("{code}{value}{ANSI_RESET}")
        } else {
            value.to_string()
        }
    }

    /// Returns one bold display string.
    fn bold(self, value: impl std::fmt::Display) -> String {
        self.color(ANSI_BOLD, value)
    }

    /// Returns one field label display string.
    fn label(self, value: impl std::fmt::Display) -> String {
        self.bold(value)
    }

    /// Returns one success display string.
    fn ok(self, value: impl std::fmt::Display) -> String {
        self.color(ANSI_GREEN, value)
    }

    /// Returns one warning display string.
    fn warn(self, value: impl std::fmt::Display) -> String {
        self.color(ANSI_YELLOW, value)
    }

    /// Returns one error display string.
    fn error(self, value: impl std::fmt::Display) -> String {
        self.color(ANSI_RED, value)
    }

    /// Returns one lower-emphasis display string.
    fn muted(self, value: impl std::fmt::Display) -> String {
        self.color(ANSI_DIM, value)
    }

    /// Returns one path display string.
    fn path(self, value: impl std::fmt::Display) -> String {
        self.color(ANSI_CYAN, value)
    }

    /// Returns one count display string.
    fn count(self, value: impl std::fmt::Display) -> String {
        self.color(ANSI_BOLD, value)
    }
}

/// Renders refresh progress events for interactive terminals.
struct RefreshProgressPrinter<W> {
    writer: W,
    style: HumanStyle,
    enabled: bool,
    total_projects: usize,
}

impl RefreshProgressPrinter<io::Stderr> {
    /// Builds one refresh progress printer for the current stderr stream.
    fn stderr() -> Self {
        let term = env::var("TERM").ok();
        Self::new(
            io::stderr(),
            HumanStyle::stderr(),
            io::stderr().is_terminal() && term.as_deref() != Some("dumb"),
        )
    }
}

impl<W: Write> RefreshProgressPrinter<W> {
    /// Builds one refresh progress printer from resolved terminal facts.
    fn new(writer: W, style: HumanStyle, enabled: bool) -> Self {
        Self {
            writer,
            style,
            enabled,
            total_projects: 1,
        }
    }

    /// Records one refresh progress event, ignoring presentation write failures.
    fn record(&mut self, event: RefreshProgress) {
        if self.enabled {
            let _ = self.write_event(event);
            let _ = self.writer.flush();
        }
    }

    /// Writes one refresh progress event to the configured stream.
    fn write_event(&mut self, event: RefreshProgress) -> io::Result<()> {
        match event {
            RefreshProgress::WorkspaceStarted { total_projects } => {
                self.total_projects = total_projects;
                writeln!(
                    self.writer,
                    "Refreshing workspace ({} project{})",
                    self.style.count(total_projects),
                    if total_projects == 1 { "" } else { "s" }
                )
            }
            RefreshProgress::ProjectStarted {
                project_name,
                project_root: _project_root,
                project_index,
                total_projects,
            } => {
                self.total_projects = total_projects;
                if total_projects > 1 {
                    writeln!(
                        self.writer,
                        "  [{}/{}] {}",
                        self.style.count(project_index),
                        self.style.count(total_projects),
                        self.style.bold(project_name)
                    )
                } else {
                    writeln!(self.writer, "Refreshing {}", self.style.bold(project_name))
                }
            }
            RefreshProgress::SyncStarted { project_name: _ } => {
                writeln!(self.writer, "{}[1/2] Syncing archive...", self.indent())
            }
            RefreshProgress::SyncFinished { project_name: _ } => Ok(()),
            RefreshProgress::IndexStarted { project_name: _ } => {
                writeln!(self.writer, "{}[2/2] Indexing sessions...", self.indent())
            }
            RefreshProgress::IndexFinished { project_name: _ } => Ok(()),
            RefreshProgress::ProjectFinished { project_name: _ } => {
                writeln!(self.writer, "{}{}", self.indent(), self.style.ok("done"))?;
                writeln!(self.writer)
            }
            RefreshProgress::ProjectFailed { project_name: _ } => {
                writeln!(
                    self.writer,
                    "{}{}",
                    self.indent(),
                    self.style.error("failed")
                )?;
                writeln!(self.writer)
            }
        }
    }

    /// Returns the current phase indentation for project or workspace progress.
    fn indent(&self) -> &'static str {
        if self.total_projects > 1 {
            "    "
        } else {
            "  "
        }
    }
}

/// Returns whether automatic terminal color should be enabled.
fn should_auto_color_output(is_terminal: bool, no_color: bool, term: Option<&str>) -> bool {
    is_terminal && !no_color && term != Some("dumb")
}

/// Returns whether one query output stream should include ANSI color.
fn should_color_output(
    policy: ColorArg,
    stdout_is_terminal: bool,
    no_color: bool,
    term: Option<&str>,
) -> bool {
    match policy {
        ColorArg::Always => true,
        ColorArg::Never => false,
        ColorArg::Auto => should_auto_color_output(stdout_is_terminal, no_color, term),
    }
}

/// Adds ANSI syntax color to one pretty-printed JSON string.
fn color_json(json: &str) -> String {
    let mut output = String::with_capacity(json.len());
    let mut index = 0;
    while index < json.len() {
        let ch = json[index..]
            .chars()
            .next()
            .expect("index should be in bounds");
        if ch == '"' {
            let end = json_string_end(json, index);
            let color = if json_string_is_key(json, end) {
                ANSI_KEY
            } else {
                ANSI_STRING
            };
            push_colored(&mut output, color, &json[index..end]);
            index = end;
        } else if ch == '-' || ch.is_ascii_digit() {
            let end = json_number_end(json, index);
            push_colored(&mut output, ANSI_NUMBER, &json[index..end]);
            index = end;
        } else if json[index..].starts_with("true") {
            push_colored(&mut output, ANSI_BOOLEAN, "true");
            index += "true".len();
        } else if json[index..].starts_with("false") {
            push_colored(&mut output, ANSI_BOOLEAN, "false");
            index += "false".len();
        } else if json[index..].starts_with("null") {
            push_colored(&mut output, ANSI_NULL, "null");
            index += "null".len();
        } else if matches!(ch, '{' | '}' | '[' | ']' | ':' | ',') {
            push_colored(&mut output, ANSI_BOLD, &json[index..index + ch.len_utf8()]);
            index += ch.len_utf8();
        } else {
            output.push(ch);
            index += ch.len_utf8();
        }
    }
    output
}

/// Adds ANSI match highlighting to mode-specific search result strings.
fn color_search_turns_json(json: &str, data: &SearchTurnsQueryData) -> Result<String> {
    let mut colored = color_json(json);
    let matcher = SearchSnippetMatcher::new(data.mode, &data.query)?;
    match data.mode {
        SearchMode::Keyword => {
            color_search_snippets(&mut colored, data, &matcher);
        }
        SearchMode::Literal | SearchMode::Regex => {
            color_search_match_snippets(&mut colored, data, &matcher);
        }
        SearchMode::FileName | SearchMode::PathFragment => {
            color_search_matched_paths(&mut colored, data, &matcher);
        }
        SearchMode::FilePath => {
            color_search_matched_path_items(&mut colored, data, |path| Some(0..path.len()));
        }
    }
    Ok(colored)
}

/// Highlights top-level keyword search snippets where a visible query term appears.
fn color_search_snippets(
    colored: &mut String,
    data: &SearchTurnsQueryData,
    matcher: &SearchSnippetMatcher,
) {
    let mut cursor = 0;
    for hit in &data.hits {
        let Some(snippet) = &hit.snippet else {
            continue;
        };
        let Some(range) = non_empty_match(matcher.find(snippet)) else {
            continue;
        };
        let Some((value_start, token_len)) = find_colored_snippet_value(colored, snippet, cursor)
        else {
            continue;
        };
        let highlighted = color_json_string_with_match(snippet, range);
        colored.replace_range(value_start..value_start + token_len, &highlighted);
        cursor = value_start + highlighted.len();
    }
}

/// Highlights nested exact-search match snippets where the exact matcher still finds the term.
fn color_search_match_snippets(
    colored: &mut String,
    data: &SearchTurnsQueryData,
    matcher: &SearchSnippetMatcher,
) {
    let mut cursor = 0;
    for hit in &data.hits {
        for matched in &hit.matches {
            let Some(range) = non_empty_match(matcher.find(&matched.snippet)) else {
                continue;
            };
            let Some((value_start, token_len)) =
                find_colored_snippet_value(colored, &matched.snippet, cursor)
            else {
                continue;
            };
            let highlighted = color_json_string_with_match(&matched.snippet, range);
            colored.replace_range(value_start..value_start + token_len, &highlighted);
            cursor = value_start + highlighted.len();
        }
    }
}

/// Highlights matched file path strings for file-search modes with literal display spans.
fn color_search_matched_paths(
    colored: &mut String,
    data: &SearchTurnsQueryData,
    matcher: &SearchSnippetMatcher,
) {
    color_search_matched_path_items(colored, data, |path| matcher.find(path));
}

/// Highlights matched path items with ranges selected by the caller.
fn color_search_matched_path_items(
    colored: &mut String,
    data: &SearchTurnsQueryData,
    path_range: impl Fn(&str) -> Option<std::ops::Range<usize>>,
) {
    let mut cursor = 0;
    for hit in &data.hits {
        let Some(mut path_cursor) = find_colored_array_start(colored, "matched_paths", cursor)
        else {
            continue;
        };
        for path in &hit.matched_paths {
            let Some(range) = non_empty_match(path_range(path)) else {
                continue;
            };
            let Some((value_start, token_len)) =
                find_colored_string_value(colored, path, path_cursor)
            else {
                continue;
            };
            let highlighted = color_json_string_with_match(path, range);
            colored.replace_range(value_start..value_start + token_len, &highlighted);
            path_cursor = value_start + highlighted.len();
        }
        cursor = path_cursor;
    }
}

/// Drops empty presentation matches before rendering highlight escape codes.
fn non_empty_match(range: Option<std::ops::Range<usize>>) -> Option<std::ops::Range<usize>> {
    range.filter(|range| !range.is_empty())
}

/// Appends one ANSI-colored JSON token to the rendered output.
fn push_colored(output: &mut String, color: &str, token: &str) {
    output.push_str(color);
    output.push_str(token);
    output.push_str(ANSI_RESET);
}

/// Returns the next colored `snippet` string value from one colored JSON document.
fn find_colored_snippet_value(
    colored: &str,
    snippet: &str,
    cursor: usize,
) -> Option<(usize, usize)> {
    let key_prefix = format!("{ANSI_KEY}\"snippet\"{ANSI_RESET}{ANSI_BOLD}:{ANSI_RESET} ");
    let token = color_json_string(snippet);
    let target = format!("{key_prefix}{token}");
    let value_start = cursor + colored.get(cursor..)?.find(&target)? + key_prefix.len();
    Some((value_start, token.len()))
}

/// Returns the byte index after one colored array key prefix.
fn find_colored_array_start(colored: &str, key: &str, cursor: usize) -> Option<usize> {
    let key_prefix =
        format!("{ANSI_KEY}\"{key}\"{ANSI_RESET}{ANSI_BOLD}:{ANSI_RESET} {ANSI_BOLD}[{ANSI_RESET}");
    Some(cursor + colored.get(cursor..)?.find(&key_prefix)? + key_prefix.len())
}

/// Returns the next colored string value matching `value`.
fn find_colored_string_value(colored: &str, value: &str, cursor: usize) -> Option<(usize, usize)> {
    let token = color_json_string(value);
    let value_start = cursor + colored.get(cursor..)?.find(&token)?;
    Some((value_start, token.len()))
}

/// Returns one syntax-colored JSON string literal.
fn color_json_string(value: &str) -> String {
    format!("{ANSI_STRING}{}{ANSI_RESET}", json_string_literal(value))
}

/// Returns one syntax-colored JSON string literal with a highlighted inner byte range.
fn color_json_string_with_match(value: &str, range: std::ops::Range<usize>) -> String {
    let prefix = json_string_inner(&value[..range.start]);
    let matched = json_string_inner(&value[range.clone()]);
    let suffix = json_string_inner(&value[range.end..]);
    format!(
        "{ANSI_STRING}\"{prefix}{ANSI_MATCH}{matched}{ANSI_RESET}{ANSI_STRING}{suffix}\"{ANSI_RESET}"
    )
}

/// Returns one JSON string literal for a known UTF-8 string.
fn json_string_literal(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string should not fail")
}

/// Returns the unquoted escaped content for one JSON string literal.
fn json_string_inner(value: &str) -> String {
    let literal = json_string_literal(value);
    literal[1..literal.len() - 1].to_owned()
}

/// Returns the byte index after one JSON string literal.
fn json_string_end(json: &str, start: usize) -> usize {
    let mut escaped = false;
    for (offset, ch) in json[start + 1..].char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return start + 1 + offset + ch.len_utf8();
        }
    }
    json.len()
}

/// Returns whether one JSON string literal is followed by an object-key colon.
fn json_string_is_key(json: &str, end: usize) -> bool {
    json[end..]
        .chars()
        .find(|ch| !ch.is_whitespace())
        .is_some_and(|ch| ch == ':')
}

/// Returns the byte index after one JSON number token.
fn json_number_end(json: &str, start: usize) -> usize {
    let mut end = start;
    for ch in json[start..].chars() {
        if matches!(ch, '-' | '+' | '.' | 'e' | 'E') || ch.is_ascii_digit() {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// Returns one machine-readable JSON error envelope string.
fn format_query_error(error: &anyhow::Error) -> String {
    let causes = error
        .chain()
        .skip(1)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let structured = error.downcast_ref::<QueryProtocolError>();
    let read_validation = error.downcast_ref::<ReadValidationError>();
    let status_json = error.downcast_ref::<StatusJsonError>();
    let payload = QueryErrorEnvelope {
        schema: "darc.error.v1",
        generated_at: current_utc_timestamp(),
        darc_version: env!("CARGO_PKG_VERSION"),
        error: QueryErrorData {
            message: error.to_string(),
            code: structured
                .map(QueryProtocolError::code)
                .or_else(|| read_validation.map(|error| error.code))
                .or_else(|| status_json.map(|error| error.code)),
            details: structured
                .map(QueryProtocolError::details)
                .or_else(|| read_validation.map(|error| error.details.clone()))
                .or_else(|| status_json.map(|error| error.details.clone())),
            causes,
        },
    };
    serde_json::to_string_pretty(&payload).unwrap_or_else(|serialization_error| {
        format!(r#"{{"schema":"darc.error.v1","error":"{serialization_error}"}}"#)
    })
}

/// Returns one machine-readable JSON error envelope string for query parse failures.
fn format_query_clap_error(error: &clap::Error) -> String {
    let payload = QueryErrorEnvelope {
        schema: "darc.error.v1",
        generated_at: current_utc_timestamp(),
        darc_version: env!("CARGO_PKG_VERSION"),
        error: QueryErrorData {
            message: error.to_string().trim_end().to_owned(),
            code: Some("invalid_arguments"),
            details: Some(json!({
                "clap_kind": format!("{:?}", error.kind()),
            })),
            causes: Vec::new(),
        },
    };
    serde_json::to_string_pretty(&payload).unwrap_or_else(|serialization_error| {
        format!(r#"{{"schema":"darc.error.v1","error":"{serialization_error}"}}"#)
    })
}

/// Resolves one project-scoped query target from an explicit id or the active project.
fn resolve_database_query_project_target(
    root: &std::path::Path,
    project_id: Option<&str>,
) -> Result<ResolvedQueryProject> {
    resolve_query_project(Some(root.to_path_buf()), project_id)
}

/// Resolves one optional value supplied either as a flag or a positional argument.
fn optional_named_or_positional<'a>(
    value_label: &str,
    flag_name: &str,
    flag_value: Option<&'a str>,
    positional_name: &str,
    positional_value: Option<&'a str>,
) -> Result<Option<&'a str>> {
    match (flag_value, positional_value) {
        (Some(_), Some(_)) => Err(ReadValidationError::conflicting_identity_arguments(
            format!("pass {value_label} either as {positional_name} or {flag_name}, not both"),
            &[value_label, positional_name, flag_name],
        )
        .into()),
        (Some(value), None) | (None, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

/// Resolves one required value supplied either as a flag or a positional argument.
fn required_named_or_positional<'a>(
    value_label: &str,
    flag_name: &str,
    flag_value: Option<&'a str>,
    positional_name: &str,
    positional_value: Option<&'a str>,
) -> Result<&'a str> {
    optional_named_or_positional(
        value_label,
        flag_name,
        flag_value,
        positional_name,
        positional_value,
    )?
    .ok_or_else(|| {
        ReadValidationError::missing_required_identity(value_label, flag_name, positional_name)
            .into()
    })
}

/// Returns the matched-path preview limit selected by CLI flags.
fn matched_path_limit_arg(
    include_all_matched_paths: bool,
    matched_path_limit: usize,
) -> Option<usize> {
    (!include_all_matched_paths).then_some(matched_path_limit)
}

/// Resolves session-id and turn-ordinal values from flag and positional forms.
fn resolve_turn_identity_args<'a>(
    session_id: Option<&'a str>,
    turn_ordinal: Option<u64>,
    session_id_arg: Option<&'a str>,
    turn_ordinal_arg: Option<&'a str>,
) -> Result<(&'a str, u64)> {
    match (session_id, turn_ordinal, session_id_arg, turn_ordinal_arg) {
        (Some(session_id), Some(turn_ordinal), None, None) => Ok((session_id, turn_ordinal)),
        (Some(session_id), None, Some(turn_ordinal_arg), None) => {
            Ok((session_id, parse_turn_ordinal_arg(turn_ordinal_arg)?))
        }
        (None, Some(turn_ordinal), Some(session_id_arg), None) => {
            Ok((session_id_arg, turn_ordinal))
        }
        (None, None, Some(session_id_arg), Some(turn_ordinal_arg)) => {
            Ok((session_id_arg, parse_turn_ordinal_arg(turn_ordinal_arg)?))
        }
        (Some(_), Some(_), Some(_), _) | (Some(_), Some(_), None, Some(_)) => {
            Err(ReadValidationError::conflicting_identity_arguments(
                "pass turn identity either as SESSION_ID TURN_ORDINAL or with --session-id/--turn-ordinal, not both",
                &["SESSION_ID", "TURN_ORDINAL", "--session-id", "--turn-ordinal"],
            )
            .into())
        }
        (Some(_), None, None, None) => Err(ReadValidationError::missing_turn_identity(
            "read command requires turn ordinal as TURN_ORDINAL or --turn-ordinal",
            &["turn_ordinal"],
        )
        .into()),
        (None, Some(_), None, None) => Err(ReadValidationError::missing_turn_identity(
            "read command requires session id as SESSION_ID or --session-id",
            &["session_id"],
        )
        .into()),
        (None, None, None, None) => Err(ReadValidationError::missing_turn_identity(
            "read command requires session id and turn ordinal as SESSION_ID TURN_ORDINAL or --session-id/--turn-ordinal",
            &["session_id", "turn_ordinal"],
        )
        .into()),
        _ => Err(ReadValidationError::conflicting_identity_arguments(
            "unexpected extra positional turn identity arguments",
            &["SESSION_ID", "TURN_ORDINAL", "--session-id", "--turn-ordinal"],
        )
        .into()),
    }
}

/// Parses one turn ordinal positional value.
fn parse_turn_ordinal_arg(value: &str) -> Result<u64> {
    value
        .parse()
        .with_context(|| format!("invalid turn ordinal `{value}`"))
}

impl From<ListSessionsArgs> for QuerySessionsArgs {
    /// Converts canonical list-session arguments into the shared query-session shape.
    fn from(args: ListSessionsArgs) -> Self {
        Self {
            root: args.root,
            project_id: args.project_id,
            provider: args.provider,
            view: args.view,
            since: args.since,
            until: args.until,
            touched_path: args.touching,
            limit: args.limit,
            offset: args.offset,
        }
    }
}

impl SearchArgs {
    /// Converts canonical search flags into the existing turn-search query shape.
    fn into_query_search_turns_args(self) -> Result<QuerySearchTurnsArgs> {
        let query = required_named_or_positional(
            "query text",
            "--query",
            self.query.as_deref(),
            "QUERY",
            self.query_arg.as_deref(),
        )?
        .to_owned();
        Ok(QuerySearchTurnsArgs {
            root: self.root,
            project_id: self.project_id,
            provider: self.provider,
            session_id: self.session_id,
            mode: self.mode,
            query_arg: Some(query),
            query: None,
            include_tool_output: self.include_tool_output,
            fields: self.fields,
            excluded_fields: self.excluded_fields,
            since: self.since,
            until: self.until,
            limit: self.limit,
            offset: self.offset,
            matched_path_limit: self.matched_path_limit,
            match_limit: self.match_limit,
            include_all_matched_paths: self.include_all_matched_paths,
        })
    }
}

/// Returns whether one string is a full canonical UUID text value.
fn is_full_uuid_text(input: &str) -> bool {
    input.len() == 36
        && input
            .chars()
            .enumerate()
            .all(|(index, ch)| matches_uuid_character(index, ch))
}

/// Returns whether one character matches the canonical UUID grammar at one fixed position.
fn matches_uuid_character(index: usize, ch: char) -> bool {
    match index {
        8 | 13 | 18 | 23 => ch == '-',
        _ => ch.is_ascii_hexdigit(),
    }
}

/// Parses one rolling day-window argument such as `7d`.
fn parse_window_days(value: &str) -> Result<u32, String> {
    let Some(days) = value.strip_suffix('d') else {
        return Err("window must use the `<days>d` format, for example `7d`".to_owned());
    };
    let days = days
        .parse::<u32>()
        .map_err(|_| format!("invalid day window `{value}`"))?;
    if days == 0 {
        return Err("window must be at least 1 day".to_owned());
    }
    Ok(days)
}

/// Resolves one query time bound from relative shorthand or absolute ISO-like text.
fn resolve_query_time_bound(value: &str) -> Result<String> {
    resolve_shared_query_time_bound(value).map_err(|message| anyhow!(message))
}

/// Resolves one query time bound against one fixed clock for deterministic tests.
#[cfg(test)]
fn resolve_query_time_bound_at(
    value: &str,
    now: std::time::SystemTime,
) -> std::result::Result<String, String> {
    darc_paths::resolve_query_time_bound_at(value, now)
}

/// Parses one exact-search evidence field from snake_case or CLI kebab-case text.
fn parse_search_evidence_field(value: &str) -> Result<SearchEvidenceField, String> {
    SearchEvidenceField::parse_label(value).ok_or_else(|| {
        format!(
            "unsupported evidence field `{value}`; expected one of {}",
            supported_search_evidence_fields()
        )
    })
}

/// Formats the accepted exact-search evidence field names for CLI errors.
fn supported_search_evidence_fields() -> String {
    SearchEvidenceField::ALL
        .iter()
        .map(|field| field.as_str().replace('_', "-"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Returns help text for exact-search field inclusion.
fn search_evidence_field_include_help() -> String {
    "Restrict literal and regex search to an evidence field. Repeat to include multiple fields.\n\nAccepted fields:\n  messages: user-message, final-answer, commentary, reasoning-summary\n  tools: tool-name, tool-arguments, tool-output\n  other: delegation-summary, delegation-metadata, hook-summary, attachment-metadata, provider-response-item-metadata"
        .to_owned()
}

/// Returns help text for exact-search field exclusion.
fn search_evidence_field_exclude_help() -> String {
    "Exclude an evidence field from literal and regex search. Repeat to exclude multiple fields.\n\nAccepted fields:\n  messages: user-message, final-answer, commentary, reasoning-summary\n  tools: tool-name, tool-arguments, tool-output\n  other: delegation-summary, delegation-metadata, hook-summary, attachment-metadata, provider-response-item-metadata"
        .to_owned()
}

/// Returns help text for the literal/regex per-hit match preview cap.
fn search_match_limit_help() -> String {
    format!(
        "Maximum nested matches per literal/regex turn hit [default: {DEFAULT_SEARCH_MATCH_LIMIT}]"
    )
}

/// Returns help text for one default row-page limit.
fn default_query_page_limit_help(prefix: &str) -> String {
    format!("{prefix} [default: {DEFAULT_QUERY_PAGE_LIMIT}]")
}

/// Converts one parsed provider argument back into the shared source kind.
fn provider_arg_to_source_kind(provider: ProviderArg) -> SourceKind {
    match provider {
        ProviderArg::Claude => SourceKind::Claude,
        ProviderArg::Codex => SourceKind::Codex,
    }
}

/// Converts one parsed turn-detail view argument into the shared session-bundle view.
fn view_arg_to_session_bundle_view(view: ViewArg) -> SessionBundleView {
    match view {
        ViewArg::Full => SessionBundleView::Full,
        ViewArg::Narrative => SessionBundleView::Narrative,
    }
}

/// Converts one parsed search-mode argument back into the shared query enum.
fn search_mode_arg_to_search_mode(mode: SearchModeArg) -> SearchMode {
    match mode {
        SearchModeArg::Keyword => SearchMode::Keyword,
        SearchModeArg::Literal => SearchMode::Literal,
        SearchModeArg::Regex => SearchMode::Regex,
        SearchModeArg::FileName => SearchMode::FileName,
        SearchModeArg::FilePath => SearchMode::FilePath,
        SearchModeArg::PathFragment => SearchMode::PathFragment,
    }
}

/// Converts one parsed session-list view argument into the shared query projection enum.
fn session_list_view_arg_to_view(view: SessionListViewArg) -> SessionsView {
    match view {
        SessionListViewArg::Compact => SessionsView::Compact,
        SessionListViewArg::Full => SessionsView::Full,
    }
}

/// Converts one parsed turn-list view argument into the shared query projection enum.
fn turn_list_view_arg_to_view(view: TurnListViewArg) -> TurnsView {
    match view {
        TurnListViewArg::Full => TurnsView::Full,
        TurnListViewArg::Oneline => TurnsView::Oneline,
    }
}

/// Stores one compact row for session-scoped `darc list turns --view oneline`.
#[derive(Debug, Clone, Serialize)]
struct TurnsOnelineTurnRow {
    turn_ordinal: u64,
    role: &'static str,
    user_prompt_preview: String,
    user_prompt_preview_chars: u64,
    user_prompt_total_chars: u64,
    agent_answer_preview: Option<String>,
    agent_answer_preview_chars: Option<u64>,
    agent_answer_total_chars: Option<u64>,
    step_count: u64,
    tool_call_count: u64,
}

/// Stores one compact top-level payload for session-scoped turn skims.
#[derive(Debug, Clone, Serialize)]
struct TurnsOnelineQueryData {
    project_id: String,
    provider: SourceKind,
    session_id: String,
    since: Option<String>,
    until: Option<String>,
    view: TurnsView,
    limit: u64,
    offset: u64,
    has_more: bool,
    turns: Vec<TurnsOnelineTurnRow>,
}

impl TurnsOnelineQueryData {
    /// Builds one compact session-turn payload from the full shared query result.
    fn from_turns_query(data: &darc_core::query::TurnsQueryData) -> Self {
        Self {
            project_id: data.project_id.clone(),
            provider: data.provider,
            session_id: data.session_id.clone(),
            since: data.since.clone(),
            until: data.until.clone(),
            view: data.view,
            limit: data.limit,
            offset: data.offset,
            has_more: data.has_more,
            turns: data
                .turns
                .iter()
                .map(|turn| TurnsOnelineTurnRow {
                    turn_ordinal: turn.turn_ordinal,
                    role: "user",
                    user_prompt_preview: turn.oneline_user_prompt_preview.clone(),
                    user_prompt_preview_chars: turn.oneline_user_prompt_preview_chars,
                    user_prompt_total_chars: turn.oneline_user_prompt_total_chars,
                    agent_answer_preview: turn.oneline_agent_answer_preview.clone(),
                    agent_answer_preview_chars: turn.oneline_agent_answer_preview_chars,
                    agent_answer_total_chars: turn.oneline_agent_answer_total_chars,
                    step_count: turn.step_count,
                    tool_call_count: turn.tool_call_count,
                })
                .collect(),
        }
    }
}

/// Stores the `--pick-one` success payload for `darc resolve session`.
#[derive(Debug, Clone, Serialize)]
struct ResolveSessionPickOneQueryData {
    query: String,
    #[serde(rename = "match")]
    r#match: ResolvedSessionMatch,
}

impl ResolveSessionPickOneQueryData {
    /// Builds one single-match convenience payload from one resolved candidate.
    fn new(query: &str, r#match: ResolvedSessionMatch) -> Self {
        Self {
            query: query.to_owned(),
            r#match,
        }
    }
}

/// Stores one machine-readable query success envelope.
#[derive(Debug, Serialize)]
struct JsonEnvelope<'a, T> {
    schema: &'a str,
    generated_at: String,
    darc_version: &'a str,
    data: &'a T,
}

/// Stores one machine-readable query error envelope.
#[derive(Debug, Serialize)]
struct QueryErrorEnvelope<'a> {
    schema: &'a str,
    generated_at: String,
    darc_version: &'a str,
    error: QueryErrorData,
}

/// Stores one machine-readable query error payload.
#[derive(Debug, Serialize)]
struct QueryErrorData {
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<JsonValue>,
    causes: Vec<String>,
}

/// Stores one structured validation error raised by canonical JSON read commands.
#[derive(Debug)]
struct ReadValidationError {
    message: String,
    code: &'static str,
    details: JsonValue,
}

impl ReadValidationError {
    /// Builds one missing identity error for a read command.
    fn missing_required_identity(
        value_label: &str,
        flag_name: &str,
        positional_name: &str,
    ) -> Self {
        let message =
            format!("read command requires {value_label} as {positional_name} or {flag_name}");
        Self {
            message,
            code: "missing_required_identity",
            details: json!({
                "value": value_label,
                "flag": flag_name,
                "positional": positional_name,
            }),
        }
    }

    /// Builds one missing turn identity error for a read command.
    fn missing_turn_identity(message: &'static str, missing: &[&str]) -> Self {
        Self {
            message: message.to_owned(),
            code: "missing_required_identity",
            details: json!({ "missing": missing }),
        }
    }

    /// Builds one conflicting identity error for a read command.
    fn conflicting_identity_arguments(message: impl Into<String>, conflicts: &[&str]) -> Self {
        Self {
            message: message.into(),
            code: "conflicting_identity_arguments",
            details: json!({ "conflicts": conflicts }),
        }
    }
}

impl std::fmt::Display for ReadValidationError {
    /// Writes the user-facing validation message.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ReadValidationError {}

/// Stores one structured status JSON error.
#[derive(Debug)]
struct StatusJsonError {
    message: String,
    code: &'static str,
    details: JsonValue,
}

impl StatusJsonError {
    /// Builds one failed status check error.
    fn check_failed(scope: &'static str, message: &'static str) -> Self {
        Self {
            message: message.to_owned(),
            code: "status_check_failed",
            details: json!({
                "scope": scope,
                "check": true,
            }),
        }
    }
}

impl std::fmt::Display for StatusJsonError {
    /// Writes the user-facing status error message.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StatusJsonError {}

/// Prepares and optionally writes the shared init draft.
fn run_init(args: InitArgs) -> Result<()> {
    let draft = prepare_init(Some(args.root))?;

    if !args.dry_run {
        write_init(&draft)?;
    }

    let style = HumanStyle::stdout();
    print_init_draft(style, &draft);
    if args.dry_run {
        println!();
        print_init_status(style, &draft, true);
        println!();
        print_section(style, "Config Preview");
        println!("{}", draft.config_toml()?);
    } else {
        println!();
        print_init_status(style, &draft, false);
    }

    Ok(())
}

/// Prints the prepared init summary.
fn print_init_draft(style: HumanStyle, draft: &InitDraft) {
    print_section(style, "Darc");
    print_field(
        style,
        2,
        "Config",
        if draft.global_config_exists {
            style.ok("existing")
        } else {
            style.warn("not found")
        },
    );
    print_field(style, 2, "Root", style.path(draft.root().display()));
    print_field(
        style,
        2,
        "Config path",
        style.path(draft.root().join("config.toml").display()),
    );
    print_field(
        style,
        2,
        "Index DB path",
        style.path(draft.root().join("index.sqlite").display()),
    );

    if !draft.global_config_exists {
        println!();
        print_section(style, "Detected Sources");
        if draft.sources.is_empty() {
            print_line(2, style.muted("none"));
        }
        for source in &draft.sources {
            print_line(2, style.bold(source.kind.title()));
            print_field(style, 4, "Path", style.path(source.root.display()));
            print_field(style, 4, "Rollouts", style.count(source.rollout_files));
            if source.subagent_rollout_files > 0 {
                print_field(
                    style,
                    4,
                    "Subagents",
                    style.count(source.subagent_rollout_files),
                );
            }
        }
    }

    println!();
    print_section(style, "Project");
    print_field(style, 2, "Name", &draft.project.name);
    print_field(
        style,
        2,
        "Root",
        style.path(draft.project.local_path.display()),
    );
    print_field(
        style,
        2,
        "State",
        if draft.project_exists {
            style.ok("already configured")
        } else {
            style.warn("new")
        },
    );
    if let Some(upstream) = &draft.project.git_upstream {
        print_field(style, 2, "Upstream", style.path(upstream));
    }
}

/// Prints the final init status block.
fn print_init_status(style: HumanStyle, draft: &InitDraft, dry_run: bool) {
    print_section(style, "Status");
    for line in format_init_status(draft, dry_run).lines() {
        let line = if dry_run {
            style.warn(line)
        } else {
            style.ok(line)
        };
        print_line(2, line);
    }
}

/// Formats the post-summary status lines for `init`.
fn format_init_status(draft: &InitDraft, dry_run: bool) -> String {
    if dry_run {
        return if draft.global_config_exists {
            if draft.project_exists {
                "Dry run only. Existing darc config was left unchanged.".to_owned()
            } else {
                "Dry run only. Project was not added to darc.".to_owned()
            }
        } else {
            "Dry run only. Global darc config and project registration were not written.".to_owned()
        };
    }

    let mut lines = Vec::new();
    if !draft.global_config_exists {
        lines.push("Initialized global darc config.".to_owned());
    }
    lines.push(if draft.project_exists {
        "Project is already configured in darc.".to_owned()
    } else {
        "Added project to darc.".to_owned()
    });
    lines.join("\n")
}

/// Links one configured project's historical paths into the active project.
fn run_link(args: LinkArgs) -> Result<()> {
    let style = HumanStyle::stdout();
    if args.dry_run {
        let report = preview_link_project(Some(args.root), &args.project)?;
        print_section(style, "Link Preview");
        print_link_report(style, &report);
        println!();
        print_section(style, "Would Update");
        if report.config_written {
            print_field(style, 2, "Config", style.warn("yes"));
        } else {
            print_field(style, 2, "Config", style.muted("unchanged"));
        }
        println!();
        print_section(style, "Status");
        print_field(style, 2, "Overall", style.ok("dry run only"));
        return Ok(());
    }

    let report = link_project(Some(args.root), &args.project)?;
    print_section(style, "Link");
    print_link_report(style, &report);
    println!();
    print_section(style, "Status");
    if report.config_written {
        print_field(style, 2, "Config", style.ok("updated"));
    } else {
        print_field(style, 2, "Config", style.ok("already covered linked paths"));
    }

    Ok(())
}

/// Prints the shared project-link identity and known-path summary.
fn print_link_report(style: HumanStyle, report: &LinkReport) {
    print_field(style, 2, "Target project", &report.target_project_name);
    print_field(
        style,
        2,
        "Target ID",
        style.muted(&report.target_project_id),
    );
    print_field(style, 2, "Linked from", &report.source_project_name);
    print_field(
        style,
        2,
        "Source ID",
        style.muted(&report.source_project_id),
    );
    print_field(
        style,
        2,
        "Project root",
        style.path(report.target_project_root.display()),
    );
    print_field(
        style,
        2,
        "Known paths",
        format!(
            "{} total, {} added",
            style.count(report.total_known_paths),
            style.count(report.new_known_paths.len())
        ),
    );
}

/// Removes one configured project and its archived/indexed data.
fn run_remove(args: RemoveArgs) -> Result<()> {
    let style = HumanStyle::stdout();
    if args.dry_run {
        let report = preview_remove_project(Some(args.root), &args.project)?;
        print_section(style, "Remove Preview");
        print_field(style, 2, "Project", &report.project_name);
        print_field(style, 2, "Project ID", style.muted(&report.project_id));
        print_field(
            style,
            2,
            "Archive",
            style.path(report.sessions_root.display()),
        );
        println!();
        print_section(style, "Would Delete");
        if report.archive_would_delete {
            print_field(style, 2, "Archive", style.warn("yes"));
        } else {
            print_field(style, 2, "Archive", style.muted("not present"));
        }
        print_field(
            style,
            2,
            "Indexed sessions",
            style.count(report.indexed_sessions_would_remove),
        );
        print_field(
            style,
            2,
            "Indexed turns",
            style.count(report.indexed_turns_would_remove),
        );
        print_field(
            style,
            2,
            "Config",
            if report.config_would_change {
                style.warn("would update")
            } else {
                style.muted("unchanged")
            },
        );
        println!();
        print_section(style, "Status");
        print_field(style, 2, "Overall", style.ok("dry run only"));
        return Ok(());
    }

    let report = remove_project(Some(args.root), &args.project)?;
    print_section(style, "Remove");
    print_field(style, 2, "Project", &report.project_name);
    print_field(style, 2, "Project ID", style.muted(&report.project_id));
    print_field(
        style,
        2,
        "Archive",
        style.path(report.sessions_root.display()),
    );
    println!();
    print_section(style, "Deleted Data");
    if report.archive_deleted {
        print_field(style, 2, "Archive", style.warn("deleted"));
    } else {
        print_field(style, 2, "Archive", style.muted("did not exist"));
    }
    print_field(
        style,
        2,
        "Indexed sessions",
        style.count(report.indexed_sessions_removed),
    );
    print_field(
        style,
        2,
        "Indexed turns",
        style.count(report.indexed_turns_removed),
    );
    println!();
    print_section(style, "Status");
    if report.config_written {
        print_field(style, 2, "Config", style.ok("updated"));
    }

    Ok(())
}

/// Rebuilds one configured project's history under the active project's id.
fn run_rename_from(args: RenameArgs) -> Result<()> {
    let style = HumanStyle::stdout();
    if args.dry_run {
        let report = preview_rename_project(Some(args.root), &args.project)?;
        print_section(style, "Rename Preview");
        print_field(style, 2, "Project", &report.target_project_name);
        print_field(
            style,
            2,
            "Project ID",
            style.muted(&report.target_project_id),
        );
        print_field(style, 2, "Renamed from", &report.source_project_name);
        print_field(
            style,
            2,
            "Source ID",
            style.muted(&report.source_project_id),
        );
        print_field(
            style,
            2,
            "Project root",
            style.path(report.target_project_root.display()),
        );
        print_field(
            style,
            2,
            "Known paths",
            format!(
                "{} total, {} would add",
                style.count(report.total_known_paths),
                style.count(report.new_known_paths.len())
            ),
        );
        println!();
        print_section(style, "Would Run");
        print_field(style, 2, "Refresh", "sync and index target project");
        print_field(
            style,
            2,
            "Source archive",
            style.path(report.source_sessions_root.display()),
        );
        if report.source_archive_would_delete {
            print_field(
                style,
                2,
                "Source archive cleanup",
                style.warn("would delete"),
            );
        } else {
            print_field(
                style,
                2,
                "Source archive cleanup",
                style.muted("not present"),
            );
        }
        print_field(
            style,
            2,
            "Indexed sessions cleanup",
            style.count(report.indexed_sessions_would_remove),
        );
        print_field(
            style,
            2,
            "Indexed turns cleanup",
            style.count(report.indexed_turns_would_remove),
        );
        print_field(
            style,
            2,
            "Config",
            if report.config_would_change {
                style.warn("would update")
            } else {
                style.muted("unchanged")
            },
        );
        println!();
        print_section(style, "Status");
        print_field(style, 2, "Overall", style.ok("dry run only"));
        return Ok(());
    }

    let report = rename_project(Some(args.root), &args.project)?;
    print_section(style, "Rename");
    print_field(style, 2, "Project", &report.link.target_project_name);
    print_field(style, 2, "Renamed from", &report.link.source_project_name);
    print_field(
        style,
        2,
        "Known paths",
        format!(
            "{} total, {} added",
            style.count(report.link.total_known_paths),
            style.count(report.link.new_known_paths.len())
        ),
    );
    println!();
    print_section(style, "Sync");
    print_field(
        style,
        2,
        "Sessions",
        format!(
            "{} copied, {} unchanged",
            style.count(report.sync.sessions_copied),
            style.count(report.sync.sessions_unchanged)
        ),
    );
    print_field(
        style,
        2,
        "Auxiliary",
        format!(
            "{} copied, {} unchanged",
            style.count(report.sync.auxiliary_copied),
            style.count(report.sync.auxiliary_unchanged)
        ),
    );
    println!();
    print_index_summary(style, &report.index);
    println!();
    print_section(style, "Cleanup");
    print_field(
        style,
        2,
        "Old archive",
        if report.remove.archive_deleted {
            style.warn("deleted")
        } else {
            style.muted("did not exist")
        },
    );
    print_field(
        style,
        2,
        "Indexed sessions",
        style.count(report.remove.indexed_sessions_removed),
    );
    println!();
    print_section(style, "Status");
    print_field(style, 2, "Overall", style.ok("renamed"));

    Ok(())
}

const DEFAULT_WATCH_DEBOUNCE: Duration = Duration::from_secs(30);
const DEFAULT_WATCH_MIN_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_WATCH_RECONCILE_INTERVAL: Duration = Duration::from_secs(600);

/// Stores one parsed refresh invocation for one-shot and watch modes.
#[derive(Debug, Clone)]
struct RefreshRunRequest {
    root: PathBuf,
    all: bool,
    provider_filter: Vec<SourceKind>,
}

/// Stores command-line watch overrides before config defaults are applied.
#[derive(Debug, Clone, Default)]
struct WatchOverrides {
    debounce: Option<String>,
    min_interval: Option<String>,
    reconcile_interval: Option<String>,
    poll: bool,
}

/// Stores resolved continuous refresh settings.
#[derive(Debug, Clone)]
struct WatchSettings {
    debounce: Duration,
    min_interval: Duration,
    reconcile_interval: Duration,
    provider_filter: Vec<SourceKind>,
    poll: bool,
    watch_paths: Vec<PathBuf>,
}

/// Stores the latest foreground or service refresh state.
#[derive(Debug, Default, Clone)]
struct WatchState {
    last_event_at: Option<String>,
    last_refresh_reason: Option<String>,
    last_refresh_started_at: Option<String>,
    last_refresh_completed_at: Option<String>,
    last_refresh_succeeded: Option<bool>,
    last_error: Option<String>,
}

/// Stores the status JSON written by continuous refresh mode.
#[derive(Debug, Serialize)]
struct WatchStatus<'a> {
    schema: &'a str,
    generated_at: String,
    root: String,
    mode: &'a str,
    running: bool,
    debounce: Option<String>,
    min_interval: Option<String>,
    reconcile_interval: Option<String>,
    poll: Option<bool>,
    last_event_at: Option<&'a str>,
    last_refresh_reason: Option<&'a str>,
    last_refresh_started_at: Option<&'a str>,
    last_refresh_completed_at: Option<&'a str>,
    last_refresh_succeeded: Option<bool>,
    last_error: Option<&'a str>,
}

/// Holds an advisory refresh lock until dropped.
struct RefreshLock {
    file: File,
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Represents filesystem watcher notifications consumed by the refresh loop.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum WatchSignal {
    Changed,
    Warning(String),
}

/// Describes why the watch loop should run a refresh cycle.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum WatchRefreshReason {
    Change,
    Reconcile,
}

impl WatchRefreshReason {
    /// Returns the stable status/log label for this refresh reason.
    fn as_str(self) -> &'static str {
        match self {
            Self::Change => "change",
            Self::Reconcile => "reconcile",
        }
    }
}

/// Runs the daily refresh workflow for one or all projects.
fn run_refresh(args: RefreshArgs) -> Result<()> {
    let provider_filter = args.provider.into_iter().map(ProviderArg::into).collect();
    let request = RefreshRunRequest {
        root: args.root,
        all: args.all,
        provider_filter,
    };

    if args.watch {
        return run_refresh_watch(
            request,
            WatchOverrides {
                debounce: args.debounce,
                min_interval: args.min_interval,
                reconcile_interval: args.reconcile_interval,
                poll: args.poll,
            },
        );
    }

    run_refresh_once(&request)
}

/// Runs one refresh cycle under the shared workspace lock.
fn run_refresh_once(request: &RefreshRunRequest) -> Result<()> {
    let _lock = if request.root.join("config.toml").exists() {
        Some(acquire_refresh_lock(&request.root)?)
    } else {
        None
    };
    let options = RefreshOptions {
        provider_filter: request.provider_filter.clone(),
    };
    let mut progress = RefreshProgressPrinter::stderr();

    if request.all {
        let report = refresh_all_projects_best_effort_with_progress(
            Some(request.root.clone()),
            options,
            |event| progress.record(event),
        )?;
        print_refresh_all_report(&report);
        let result = refresh_all_exit_status(&report);
        if result.is_ok() {
            maybe_print_upgrade_nudge(&request.root);
        }
        return result;
    }

    let report = refresh_project_with_progress(Some(request.root.clone()), options, |event| {
        progress.record(event);
    })
    .map_err(add_init_hint_for_unconfigured_project)?;
    print_refresh_report(&report);
    maybe_print_upgrade_nudge(&request.root);
    Ok(())
}

/// Runs continuous foreground refresh until interrupted.
fn run_refresh_watch(mut request: RefreshRunRequest, overrides: WatchOverrides) -> Result<()> {
    let settings = load_watch_settings(&request.root, &request.provider_filter, &overrides)?;
    request.provider_filter = settings.provider_filter.clone();
    let style = HumanStyle::stdout();

    print_section(style, "Watch");
    print_field(
        style,
        2,
        "Scope",
        if request.all {
            "the shared workspace"
        } else {
            "the active project"
        },
    );
    print_field(style, 2, "Root", style.path(request.root.display()));
    print_field(style, 2, "Debounce", format_duration(settings.debounce));
    print_field(
        style,
        2,
        "Minimum interval",
        format_duration(settings.min_interval),
    );
    print_field(
        style,
        2,
        "Reconcile interval",
        format_duration(settings.reconcile_interval),
    );
    print_field(
        style,
        2,
        "Watcher",
        if settings.poll {
            style.warn("polling reconcile")
        } else {
            style.ok("macOS filesystem events")
        },
    );
    println!();
    print_section(style, "Watch Paths");
    for path in &settings.watch_paths {
        print_line(2, style.path(path.display()));
    }
    println!();

    let (event_tx, rx) = mpsc::channel();
    #[cfg(target_os = "macos")]
    let _watcher = if settings.poll {
        None
    } else {
        Some(install_native_watchers(
            &settings.watch_paths,
            event_tx.clone(),
        )?)
    };
    #[cfg(not(target_os = "macos"))]
    let _event_tx = event_tx;
    #[cfg(not(target_os = "macos"))]
    if !settings.poll {
        bail!(
            "native watch mode is currently supported only on macOS; pass `--poll` to use periodic reconcile mode"
        );
    }

    let mut state = WatchState::default();
    write_watch_status(
        &request.root,
        &state,
        true,
        "refresh-watch",
        Some(&settings),
    )?;
    run_refresh_cycle(&request, &mut state, &settings, "initial")?;

    let mut dirty_since: Option<Instant> = None;
    let mut last_refresh_at = Some(Instant::now());
    loop {
        let timeout = watch_loop_timeout(dirty_since, last_refresh_at, &settings);
        match rx.recv_timeout(timeout) {
            Ok(WatchSignal::Changed) => {
                state.last_event_at = Some(current_utc_timestamp());
                dirty_since.get_or_insert_with(Instant::now);
                write_watch_status(
                    &request.root,
                    &state,
                    true,
                    "refresh-watch",
                    Some(&settings),
                )?;
            }
            Ok(WatchSignal::Warning(warning)) => {
                let style = HumanStyle::stderr();
                eprintln!("{}", style.warn(format!("warning [watch]: {warning}")));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("watch event channel disconnected");
            }
        }

        let now = Instant::now();
        if let Some(reason) = next_watch_refresh(dirty_since, last_refresh_at, now, &settings) {
            run_refresh_cycle(&request, &mut state, &settings, reason.as_str())?;
            last_refresh_at = Some(Instant::now());
            dirty_since = None;
        }
    }
}

/// Runs one watched refresh cycle and records status without terminating on refresh failure.
fn run_refresh_cycle(
    request: &RefreshRunRequest,
    state: &mut WatchState,
    settings: &WatchSettings,
    reason: &str,
) -> Result<()> {
    let style = HumanStyle::stdout();
    println!(
        "[{}] {} ({reason}).",
        style.muted(current_utc_timestamp()),
        style.bold("Running Darc refresh")
    );
    state.last_refresh_reason = Some(reason.to_owned());
    state.last_refresh_started_at = Some(current_utc_timestamp());
    write_watch_status(&request.root, state, true, "refresh-watch", Some(settings))?;

    match run_refresh_once(request) {
        Ok(()) => {
            state.last_refresh_completed_at = Some(current_utc_timestamp());
            state.last_refresh_succeeded = Some(true);
            state.last_error = None;
            write_watch_status(&request.root, state, true, "refresh-watch", Some(settings))?;
            println!(
                "[{}] {}.",
                style.muted(current_utc_timestamp()),
                style.ok("Refresh completed")
            );
        }
        Err(error) => {
            let style = HumanStyle::stderr();
            let message = format!("{error:#}");
            state.last_refresh_completed_at = Some(current_utc_timestamp());
            state.last_refresh_succeeded = Some(false);
            state.last_error = Some(message.clone());
            write_watch_status(&request.root, state, true, "refresh-watch", Some(settings))?;
            eprintln!("{}", style.error(format!("error [watch]: {message}")));
        }
    }
    Ok(())
}

/// Returns the current timeout for the watch loop.
fn watch_loop_timeout(
    dirty_since: Option<Instant>,
    last_refresh_at: Option<Instant>,
    settings: &WatchSettings,
) -> Duration {
    watch_loop_timeout_at(Instant::now(), dirty_since, last_refresh_at, settings)
}

/// Returns the timeout for a watch loop iteration at one instant.
fn watch_loop_timeout_at(
    now: Instant,
    dirty_since: Option<Instant>,
    last_refresh_at: Option<Instant>,
    settings: &WatchSettings,
) -> Duration {
    let mut deadline = last_refresh_at
        .map(|last_refresh_at| last_refresh_at + settings.reconcile_interval)
        .unwrap_or(now);
    if let Some(dirty_since) = dirty_since {
        let mut dirty_deadline = dirty_since + settings.debounce;
        if let Some(last_refresh_at) = last_refresh_at {
            dirty_deadline = dirty_deadline.max(last_refresh_at + settings.min_interval);
        }
        if dirty_deadline < deadline {
            deadline = dirty_deadline;
        }
    }
    deadline.saturating_duration_since(now)
}

/// Returns the refresh reason that is due at the given instant.
fn next_watch_refresh(
    dirty_since: Option<Instant>,
    last_refresh_at: Option<Instant>,
    now: Instant,
    settings: &WatchSettings,
) -> Option<WatchRefreshReason> {
    if should_run_reconcile_refresh(last_refresh_at, now, settings) {
        Some(WatchRefreshReason::Reconcile)
    } else if should_run_watched_refresh(dirty_since, last_refresh_at, now, settings) {
        Some(WatchRefreshReason::Change)
    } else {
        None
    }
}

/// Returns whether the periodic safety refresh is due.
fn should_run_reconcile_refresh(
    last_refresh_at: Option<Instant>,
    now: Instant,
    settings: &WatchSettings,
) -> bool {
    last_refresh_at
        .map(|last_refresh_at| now.duration_since(last_refresh_at) >= settings.reconcile_interval)
        .unwrap_or(true)
}

/// Returns whether the watch loop should run a refresh now.
fn should_run_watched_refresh(
    dirty_since: Option<Instant>,
    last_refresh_at: Option<Instant>,
    now: Instant,
    settings: &WatchSettings,
) -> bool {
    dirty_since.is_some_and(|started| {
        let refresh_ready = last_refresh_at
            .map(|last_refresh_at| now.duration_since(last_refresh_at) >= settings.min_interval)
            .unwrap_or(true);
        now.duration_since(started) >= settings.debounce && refresh_ready
    })
}

/// Resolves watch settings from CLI overrides and the shared config.
fn load_watch_settings(
    root: &Path,
    cli_providers: &[SourceKind],
    overrides: &WatchOverrides,
) -> Result<WatchSettings> {
    let config_path = root.join("config.toml");
    let config = load_config(&config_path)
        .with_context(|| format!("failed to load watch config from {}", config_path.display()))?;
    let watch = config.watch.clone();
    let debounce = parse_watch_duration(
        "debounce",
        overrides.debounce.as_ref().or(watch.debounce.as_ref()),
        DEFAULT_WATCH_DEBOUNCE,
    )?;
    let min_interval = parse_watch_duration(
        "min_interval",
        overrides
            .min_interval
            .as_ref()
            .or(watch.min_interval.as_ref()),
        DEFAULT_WATCH_MIN_INTERVAL,
    )?;
    let reconcile_interval = parse_watch_duration(
        "reconcile_interval",
        overrides
            .reconcile_interval
            .as_ref()
            .or(watch.reconcile_interval.as_ref()),
        DEFAULT_WATCH_RECONCILE_INTERVAL,
    )?;
    let provider_filter = if cli_providers.is_empty() {
        watch.providers.clone()
    } else {
        cli_providers.to_vec()
    };
    let poll = overrides.poll || watch.poll;
    let watch_paths = watch_paths(root, &config, &provider_filter)?;

    Ok(WatchSettings {
        debounce,
        min_interval,
        reconcile_interval,
        provider_filter,
        poll,
        watch_paths,
    })
}

/// Builds the source and config paths that can trigger a watched refresh.
fn watch_paths(
    root: &Path,
    config: &darc_core::config::SharedConfig,
    provider_filter: &[SourceKind],
) -> Result<Vec<PathBuf>> {
    let mut paths = vec![root.join("config.toml")];
    if (provider_filter.is_empty() || provider_filter.contains(&SourceKind::Claude))
        && let Some(source) = &config.sources.claude
        && source.enabled
    {
        paths.push(source.projects_root.clone());
    }
    if (provider_filter.is_empty() || provider_filter.contains(&SourceKind::Codex))
        && let Some(source) = &config.sources.codex
        && source.enabled
    {
        paths.push(source.sessions_root.clone());
        paths.push(source.home.join("archived_sessions"));
    }

    let existing = paths
        .into_iter()
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    if existing.is_empty() {
        bail!("no existing Darc config or source paths are available to watch");
    }
    Ok(existing)
}

/// Parses one watch duration setting.
fn parse_watch_duration(name: &str, value: Option<&String>, default: Duration) -> Result<Duration> {
    match value {
        Some(value) => parse_duration(value)
            .with_context(|| format!("invalid watch `{name}` duration `{value}`")),
        None => Ok(default),
    }
}

/// Parses a compact duration such as `500ms`, `30s`, `5m`, or `1h`.
fn parse_duration(value: &str) -> Result<Duration> {
    let value = value.trim();
    if value.is_empty() {
        bail!("duration must not be empty");
    }
    let digit_len = value.bytes().take_while(u8::is_ascii_digit).count();
    if digit_len == 0 || digit_len == value.len() {
        bail!("duration must use a unit: ms, s, m, or h");
    }
    let amount = value[..digit_len]
        .parse::<u64>()
        .context("duration amount must be an unsigned integer")?;
    let duration = match &value[digit_len..] {
        "ms" => Duration::from_millis(amount),
        "s" => Duration::from_secs(amount),
        "m" => Duration::from_secs(amount.saturating_mul(60)),
        "h" => Duration::from_secs(amount.saturating_mul(3_600)),
        unit => bail!("unsupported duration unit `{unit}`; use ms, s, m, or h"),
    };
    if duration.is_zero() {
        bail!("duration must be greater than zero");
    }
    Ok(duration)
}

/// Formats one duration in a compact CLI-friendly form.
fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis.is_multiple_of(3_600_000) {
        format!("{}h", millis / 3_600_000)
    } else if millis.is_multiple_of(60_000) {
        format!("{}m", millis / 60_000)
    } else if millis.is_multiple_of(1_000) {
        format!("{}s", millis / 1_000)
    } else {
        format!("{millis}ms")
    }
}

/// Acquires the shared refresh lock for this Darc root.
fn acquire_refresh_lock(root: &Path) -> Result<RefreshLock> {
    let run_dir = root.join("run");
    fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create {}", run_dir.display()))?;
    let lock_path = run_dir.join("refresh.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    file.try_lock_exclusive().with_context(|| {
        format!(
            "another Darc refresh is already running ({})",
            lock_path.display()
        )
    })?;
    Ok(RefreshLock { file })
}

/// Writes the current continuous refresh status JSON.
fn write_watch_status(
    root: &Path,
    state: &WatchState,
    running: bool,
    mode: &str,
    settings: Option<&WatchSettings>,
) -> Result<()> {
    let run_dir = root.join("run");
    fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create {}", run_dir.display()))?;
    let status = WatchStatus {
        schema: "darc.watch.status.v1",
        generated_at: current_utc_timestamp(),
        root: root.display().to_string(),
        mode,
        running,
        debounce: settings.map(|settings| format_duration(settings.debounce)),
        min_interval: settings.map(|settings| format_duration(settings.min_interval)),
        reconcile_interval: settings.map(|settings| format_duration(settings.reconcile_interval)),
        poll: settings.map(|settings| settings.poll),
        last_event_at: state.last_event_at.as_deref(),
        last_refresh_reason: state.last_refresh_reason.as_deref(),
        last_refresh_started_at: state.last_refresh_started_at.as_deref(),
        last_refresh_completed_at: state.last_refresh_completed_at.as_deref(),
        last_refresh_succeeded: state.last_refresh_succeeded,
        last_error: state.last_error.as_deref(),
    };
    let content = serde_json::to_vec_pretty(&status).context("failed to serialize watch status")?;
    let status_path = run_dir.join("status.json");
    fs::write(&status_path, content)
        .with_context(|| format!("failed to write {}", status_path.display()))
}

/// Installs native macOS watchers for the selected paths.
#[cfg(target_os = "macos")]
fn install_native_watchers(
    paths: &[PathBuf],
    tx: mpsc::Sender<WatchSignal>,
) -> Result<notify::RecommendedWatcher> {
    use notify::{Config, RecursiveMode, Watcher};

    let mut watcher = notify::RecommendedWatcher::new(
        move |event: notify::Result<notify::Event>| match event {
            Ok(_event) => {
                let _ = tx.send(WatchSignal::Changed);
            }
            Err(error) => {
                let _ = tx.send(WatchSignal::Warning(error.to_string()));
            }
        },
        Config::default(),
    )
    .context("failed to create macOS filesystem watcher")?;

    for path in paths {
        watcher
            .watch(path, RecursiveMode::Recursive)
            .with_context(|| format!("failed to watch {}", path.display()))?;
    }
    Ok(watcher)
}

#[cfg(target_os = "macos")]
const MACOS_SERVICE_LABEL: &str = "com.0xjunha.darc.refresh";

/// Dispatches one service lifecycle command.
fn run_service(args: ServiceArgs) -> Result<()> {
    run_platform_service(args)
}

/// Runs one macOS LaunchAgent service command.
#[cfg(target_os = "macos")]
fn run_platform_service(args: ServiceArgs) -> Result<()> {
    match args.command {
        ServiceCommands::Start => start_macos_service(&args.root),
        ServiceCommands::Stop => stop_macos_service(&args.root),
        ServiceCommands::Restart => {
            stop_macos_service(&args.root)?;
            start_macos_service(&args.root)
        }
        ServiceCommands::Status => print_macos_service_status(&args.root),
        ServiceCommands::Enable => enable_macos_service(&args.root),
        ServiceCommands::Disable => disable_macos_service(&args.root),
    }
}

/// Reports unsupported service management on non-macOS platforms.
#[cfg(not(target_os = "macos"))]
fn run_platform_service(_args: ServiceArgs) -> Result<()> {
    bail!("`darc service` is currently supported only on macOS")
}

/// Enables the macOS LaunchAgent for future logins.
#[cfg(target_os = "macos")]
fn enable_macos_service(root: &Path) -> Result<()> {
    let plist_path = write_macos_launch_agent(root, true)?;
    let style = HumanStyle::stdout();
    print_section(style, "Service");
    print_field(style, 2, "Status", style.ok("enabled"));
    print_field(style, 2, "LaunchAgent", style.path(plist_path.display()));
    print_line(
        2,
        style.muted("Run `darc service start` to start it in this login session."),
    );
    Ok(())
}

/// Disables and unloads the macOS LaunchAgent.
#[cfg(target_os = "macos")]
fn disable_macos_service(root: &Path) -> Result<()> {
    stop_macos_service(root)?;
    let plist_path = macos_launch_agent_path()?;
    if plist_path.exists() {
        fs::remove_file(&plist_path)
            .with_context(|| format!("failed to remove {}", plist_path.display()))?;
        let style = HumanStyle::stdout();
        print_section(style, "Service");
        print_field(style, 2, "Status", style.warn("disabled"));
        print_field(
            style,
            2,
            "Removed LaunchAgent",
            style.path(plist_path.display()),
        );
    } else {
        let style = HumanStyle::stdout();
        print_section(style, "Service");
        print_field(style, 2, "Status", style.muted("already disabled"));
    }
    remove_macos_runtime_plist(root)?;
    Ok(())
}

/// Starts or restarts the macOS LaunchAgent in the current login session.
#[cfg(target_os = "macos")]
fn start_macos_service(root: &Path) -> Result<()> {
    let launch_agent_path = macos_launch_agent_path()?;
    let plist_path = if launch_agent_path.exists() {
        launch_agent_path
    } else {
        write_macos_runtime_plist(root)?
    };
    if !macos_service_loaded()? {
        run_launchctl(&[
            "bootstrap".to_owned(),
            macos_launch_domain()?,
            plist_path.display().to_string(),
        ])?;
    }
    run_launchctl(&[
        "kickstart".to_owned(),
        "-k".to_owned(),
        macos_launch_target()?,
    ])?;
    let style = HumanStyle::stdout();
    print_section(style, "Service");
    print_field(style, 2, "Status", style.ok("started"));
    print_field(
        style,
        2,
        "Command",
        format!(
            "darc refresh --watch --all --root {}",
            style.path(root.display())
        ),
    );
    Ok(())
}

/// Stops the macOS LaunchAgent in the current login session.
#[cfg(target_os = "macos")]
fn stop_macos_service(root: &Path) -> Result<()> {
    let style = HumanStyle::stdout();
    if macos_service_loaded()? {
        run_launchctl(&["bootout".to_owned(), macos_launch_target()?])?;
        print_section(style, "Service");
        print_field(style, 2, "Status", style.warn("stopped"));
    } else {
        print_section(style, "Service");
        print_field(style, 2, "Status", style.muted("not running"));
    }
    remove_macos_runtime_plist(root)?;
    Ok(())
}

/// Prints macOS LaunchAgent and Darc watch status.
#[cfg(target_os = "macos")]
fn print_macos_service_status(root: &Path) -> Result<()> {
    let plist_path = macos_launch_agent_path()?;
    let runtime_plist_path = macos_runtime_plist_path(root);
    let enabled = plist_path.exists();
    let running = macos_service_loaded()?;
    let style = HumanStyle::stdout();
    print_section(style, "Service");
    print_field(style, 2, "Name", "Darc refresh");
    print_field(style, 2, "Platform", "macOS LaunchAgent");
    print_field(style, 2, "Label", style.muted(MACOS_SERVICE_LABEL));
    print_field(style, 2, "Enabled", yes_no(style, enabled));
    print_field(style, 2, "Running", yes_no(style, running));
    let launch_agent = if enabled {
        style.path(plist_path.display())
    } else if running && runtime_plist_path.exists() {
        format!(
            "{} {}",
            style.path(runtime_plist_path.display()),
            style.muted("(runtime)")
        )
    } else {
        style.path(plist_path.display())
    };
    print_field(style, 2, "LaunchAgent", launch_agent);

    println!();
    print_section(style, "Watch Status");
    let status_path = root.join("run/status.json");
    if status_path.exists() {
        let content = fs::read_to_string(&status_path)
            .with_context(|| format!("failed to read {}", status_path.display()))?;
        let status: JsonValue =
            serde_json::from_str(&content).context("failed to parse watch status JSON")?;
        print_field(style, 2, "Status file", style.path(status_path.display()));
        print_field(
            style,
            2,
            "Debounce",
            json_string_or_dash(style, &status["debounce"]),
        );
        print_field(
            style,
            2,
            "Minimum interval",
            json_string_or_dash(style, &status["min_interval"]),
        );
        print_field(
            style,
            2,
            "Reconcile interval",
            json_string_or_dash(style, &status["reconcile_interval"]),
        );
        print_field(style, 2, "Poll", json_bool_or_dash(style, &status["poll"]));
        print_field(
            style,
            2,
            "Last event",
            json_string_or_dash(style, &status["last_event_at"]),
        );
        print_field(
            style,
            2,
            "Last refresh reason",
            json_string_or_dash(style, &status["last_refresh_reason"]),
        );
        print_field(
            style,
            2,
            "Last refresh started",
            json_string_or_dash(style, &status["last_refresh_started_at"]),
        );
        print_field(
            style,
            2,
            "Last refresh completed",
            json_string_or_dash(style, &status["last_refresh_completed_at"]),
        );
        print_field(
            style,
            2,
            "Last refresh succeeded",
            json_success_or_dash(style, &status["last_refresh_succeeded"]),
        );
        print_field(
            style,
            2,
            "Last error",
            json_error_or_dash(style, &status["last_error"]),
        );
    } else {
        print_field(
            style,
            2,
            "Status file",
            format!(
                "{} ({})",
                style.muted("not found"),
                style.path(status_path.display())
            ),
        );
    }
    Ok(())
}

/// Writes the LaunchAgent plist used to run `darc refresh --watch --all`.
#[cfg(target_os = "macos")]
fn write_macos_launch_agent(root: &Path, run_at_load: bool) -> Result<PathBuf> {
    let plist_path = macos_launch_agent_path()?;
    write_macos_service_plist(&plist_path, root, run_at_load)
}

/// Writes a runtime-only launchd plist for `service start` without auto-start.
#[cfg(target_os = "macos")]
fn write_macos_runtime_plist(root: &Path) -> Result<PathBuf> {
    let plist_path = macos_runtime_plist_path(root);
    write_macos_service_plist(&plist_path, root, false)
}

/// Writes one launchd plist to the requested path.
#[cfg(target_os = "macos")]
fn write_macos_service_plist(plist_path: &Path, root: &Path, run_at_load: bool) -> Result<PathBuf> {
    let parent = plist_path
        .parent()
        .context("LaunchAgent path is missing a parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::create_dir_all(root.join("log"))
        .with_context(|| format!("failed to create {}", root.join("log").display()))?;
    fs::create_dir_all(root.join("run"))
        .with_context(|| format!("failed to create {}", root.join("run").display()))?;

    let executable = env::current_exe().context("failed to resolve current executable")?;
    let plist = macos_launch_agent_plist(root, &executable, run_at_load);
    fs::write(plist_path, plist.as_bytes())
        .with_context(|| format!("failed to write {}", plist_path.display()))?;
    Ok(plist_path.to_path_buf())
}

/// Removes the runtime-only launchd plist when present.
#[cfg(target_os = "macos")]
fn remove_macos_runtime_plist(root: &Path) -> Result<()> {
    let plist_path = macos_runtime_plist_path(root);
    if plist_path.exists() {
        fs::remove_file(&plist_path)
            .with_context(|| format!("failed to remove {}", plist_path.display()))?;
    }
    Ok(())
}

/// Returns the runtime-only LaunchAgent plist path.
#[cfg(target_os = "macos")]
fn macos_runtime_plist_path(root: &Path) -> PathBuf {
    root.join("run")
        .join(format!("{MACOS_SERVICE_LABEL}.plist"))
}

/// Builds the LaunchAgent plist XML.
#[cfg(target_os = "macos")]
fn macos_launch_agent_plist(root: &Path, executable: &Path, run_at_load: bool) -> String {
    let stdout = root.join("log/refresh-watch.out.log");
    let stderr = root.join("log/refresh-watch.err.log");
    let run_at_load = if run_at_load { "true" } else { "false" };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{executable}</string>
    <string>refresh</string>
    <string>--watch</string>
    <string>--all</string>
    <string>--root</string>
    <string>{root}</string>
  </array>
  <key>RunAtLoad</key>
  <{run_at_load}/>
  <key>StandardOutPath</key>
  <string>{stdout}</string>
  <key>StandardErrorPath</key>
  <string>{stderr}</string>
</dict>
</plist>
"#,
        label = xml_escape(MACOS_SERVICE_LABEL),
        executable = xml_escape(&executable.display().to_string()),
        root = xml_escape(&root.display().to_string()),
        stdout = xml_escape(&stdout.display().to_string()),
        stderr = xml_escape(&stderr.display().to_string()),
    )
}

/// Returns the per-user LaunchAgent plist path.
#[cfg(target_os = "macos")]
fn macos_launch_agent_path() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{MACOS_SERVICE_LABEL}.plist")))
}

/// Returns the current launchd GUI domain.
#[cfg(target_os = "macos")]
fn macos_launch_domain() -> Result<String> {
    Ok(format!("gui/{}", current_uid()?))
}

/// Returns the launchd service target.
#[cfg(target_os = "macos")]
fn macos_launch_target() -> Result<String> {
    Ok(format!(
        "{}/{}",
        macos_launch_domain()?,
        MACOS_SERVICE_LABEL
    ))
}

/// Returns whether the LaunchAgent is loaded.
#[cfg(target_os = "macos")]
fn macos_service_loaded() -> Result<bool> {
    let output = Command::new("launchctl")
        .arg("print")
        .arg(macos_launch_target()?)
        .output()
        .context("failed to run launchctl print")?;
    Ok(output.status.success())
}

/// Runs `launchctl` and fails on a non-zero exit.
#[cfg(target_os = "macos")]
fn run_launchctl(args: &[String]) -> Result<()> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .context("failed to run launchctl")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("launchctl {} failed: {}", args.join(" "), stderr.trim());
}

/// Returns the current numeric user id.
#[cfg(target_os = "macos")]
fn current_uid() -> Result<String> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .context("failed to run id -u")?;
    if !output.status.success() {
        bail!("id -u failed");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Escapes one value for XML text content.
#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Formats a boolean as a styled yes or no.
#[cfg(target_os = "macos")]
fn yes_no(style: HumanStyle, value: bool) -> String {
    if value {
        style.ok("yes")
    } else {
        style.muted("no")
    }
}

/// Formats one JSON string value or a muted dash.
#[cfg(target_os = "macos")]
fn json_string_or_dash(style: HumanStyle, value: &JsonValue) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| style.muted("-"))
}

/// Formats one JSON boolean value or a muted dash.
#[cfg(target_os = "macos")]
fn json_bool_or_dash(style: HumanStyle, value: &JsonValue) -> String {
    value
        .as_bool()
        .map(|value| value.to_string())
        .unwrap_or_else(|| style.muted("-"))
}

/// Formats a JSON success boolean with state coloring or a muted dash.
#[cfg(target_os = "macos")]
fn json_success_or_dash(style: HumanStyle, value: &JsonValue) -> String {
    match value.as_bool() {
        Some(true) => style.ok("true"),
        Some(false) => style.error("false"),
        None => style.muted("-"),
    }
}

/// Formats a JSON error string with error coloring or a muted dash.
#[cfg(target_os = "macos")]
fn json_error_or_dash(style: HumanStyle, value: &JsonValue) -> String {
    value
        .as_str()
        .map(|value| style.error(value))
        .unwrap_or_else(|| style.muted("-"))
}

/// Converts one workspace refresh report into the final CLI exit result.
fn refresh_all_exit_status(report: &RefreshAllBestEffortReport) -> Result<()> {
    if report.has_failures() {
        bail!(
            "{} project(s) failed during workspace refresh",
            report.failed_count()
        );
    }
    Ok(())
}

/// Shows Darc status for the active project or shared workspace.
fn run_status(args: StatusArgs) -> Result<()> {
    if args.workspace {
        let report = status_workspace(Some(args.root), args.check)?;
        if args.json {
            let output = QueryOutput::new(ColorArg::Never);
            print_json_envelope(&output, "darc.status.workspace.v1", &report)?;
            return status_check_exit(
                report.has_failed_check(),
                "workspace",
                "workspace status check failed",
            );
        }
        print_workspace_status(&report);
        return status_check_exit(
            report.has_failed_check(),
            "workspace",
            "workspace status check failed",
        );
    }

    let report = status_project(Some(args.root), args.check)
        .map_err(add_init_hint_for_unconfigured_project)?;
    if args.json {
        let output = QueryOutput::new(ColorArg::Never);
        print_json_envelope(&output, "darc.status.project.v1", &report)?;
        return status_check_exit(report.has_failed_check(), "project", "status check failed");
    }
    print_project_status(&report);
    status_check_exit(report.has_failed_check(), "project", "status check failed")
}

/// Converts an optional status sync-check failure into the final CLI exit result.
fn status_check_exit(
    has_failed_check: bool,
    scope: &'static str,
    message: &'static str,
) -> Result<()> {
    if has_failed_check {
        return Err(StatusJsonError::check_failed(scope, message).into());
    }
    Ok(())
}

/// Prints one active-project status report.
fn print_project_status(report: &darc_core::ProjectStatusReport) {
    let style = HumanStyle::stdout();
    print_status_header(style, &report.root, None);
    println!();
    print_sources(style, &report.sources);
    println!();
    print_active_project_identity(style, &report.project);
    println!();
    print_project_index_status(style, &report.project, 0);
    if report.project.sync_check.is_some() {
        println!();
        print_sync_check(style, report.project.sync_check.as_ref(), "Sync Check", 0);
    }
    if !report.project.issues.is_empty() {
        println!();
        print_project_issues(style, &report.project, 0);
    }
    println!();
    print_overall_status(
        style,
        format_overall_status(
            &report.root.issues,
            &report.sources,
            std::slice::from_ref(&report.project),
        ),
    );
}

/// Prints one workspace status report.
fn print_workspace_status(report: &WorkspaceStatusReport) {
    let style = HumanStyle::stdout();
    print_status_header(style, &report.root, Some(report.projects.len()));
    println!();
    print_sources(style, &report.sources);
    println!();
    print_workspace_summary(style, report);
    println!();
    print_workspace_projects(style, &report.projects);
    println!();
    print_overall_status(
        style,
        format_overall_status(&report.root.issues, &report.sources, &report.projects),
    );
}

/// Prints a plain section heading.
fn print_section(style: HumanStyle, title: &str) {
    println!("{}", style.bold(title));
}

/// Prints one indented label/value field.
fn print_field(style: HumanStyle, indent: usize, label: &str, value: impl std::fmt::Display) {
    println!("{}{}: {}", " ".repeat(indent), style.label(label), value);
}

/// Prints one indented continuation line.
fn print_line(indent: usize, value: impl std::fmt::Display) {
    println!("{}{}", " ".repeat(indent), value);
}

/// Prints one warning to stderr using human-output styling when available.
fn print_warning(message: impl std::fmt::Display) {
    let style = HumanStyle::stderr();
    eprintln!("{}", style.warn(format!("warning: {message}")));
}

/// Prints one project-scoped warning to stderr using human-output styling when available.
fn print_project_warning(project_name: &str, message: impl std::fmt::Display) {
    let style = HumanStyle::stderr();
    eprintln!(
        "{}",
        style.warn(format!("warning [{project_name}]: {message}"))
    );
}

/// Returns a count phrase for one singular/plural noun pair.
fn count_label(count: usize, singular: &str, plural: &str) -> String {
    let noun = if count == 1 { singular } else { plural };
    format!("{count} {noun}")
}

/// Returns one archive availability label.
fn archive_status(style: HumanStyle, project: &StatusProject) -> String {
    if project.archive_exists {
        style.ok("ok")
    } else {
        style.error("missing")
    }
}

/// Returns one configured-source state label.
fn source_state(style: HumanStyle, source: &StatusSource) -> String {
    if !source.configured {
        style.muted("not configured")
    } else if source.enabled {
        style.ok("enabled")
    } else {
        style.muted("disabled")
    }
}

/// Returns one configured-source path availability label.
fn source_path_state(style: HumanStyle, source: &StatusSource) -> String {
    if source.path_exists {
        style.ok("ok")
    } else {
        style.error("missing")
    }
}

/// Returns one configured-source path label.
fn source_path(style: HumanStyle, source: &StatusSource) -> String {
    let path = source
        .path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "none".to_owned());
    style.path(path)
}

/// Returns one formatted source path with availability.
fn source_path_with_state(style: HumanStyle, source: &StatusSource) -> String {
    format!(
        "{} ({})",
        source_path(style, source),
        source_path_state(style, source)
    )
}

/// Returns one formatted indexed count summary.
fn indexed_summary(style: HumanStyle, project: &StatusProject) -> String {
    format!(
        "{} sessions, {} turns",
        style.count(project.session_count),
        style.count(project.turn_count)
    )
}

/// Prints the common root/config/database status header.
fn print_status_header(
    style: HumanStyle,
    root: &darc_core::query::RootInfo,
    project_count: Option<usize>,
) {
    print_section(style, "Darc");
    print_field(style, 2, "Version", env!("CARGO_PKG_VERSION"));
    print_field(
        style,
        2,
        "Root",
        style.path(root.resolved_root_path.display()),
    );
    let config_status = if !root.available.config_exists {
        style.error("missing")
    } else {
        match project_count {
            Some(count) => style.ok(format!(
                "ok ({})",
                count_label(count, "project", "projects")
            )),
            None => style.ok("ok"),
        }
    };
    print_field(style, 2, "Config", config_status);
    print_field(
        style,
        2,
        "Index DB",
        if root.available.database_exists {
            style.ok("ok")
        } else {
            style.error("missing")
        },
    );
}

/// Prints all supported source availability rows.
fn print_sources(style: HumanStyle, sources: &[StatusSource]) {
    print_section(style, "Sources");
    for source in sources {
        print_line(2, style.bold(source.kind.title()));
        print_field(style, 4, "State", source_state(style, source));
        if source.configured {
            print_field(style, 4, "Path", source_path_with_state(style, source));
        }
    }
}

/// Prints the active project identity and storage block.
fn print_active_project_identity(style: HumanStyle, project: &StatusProject) {
    print_section(style, "Active Project");
    print_field(style, 2, "Name", &project.name);
    print_field(style, 2, "ID", style.muted(&project.id));
    print_field(
        style,
        2,
        "Root",
        style.path(
            project
                .resolved_project_root
                .as_ref()
                .unwrap_or(&project.local_path)
                .display(),
        ),
    );
    print_field(style, 2, "Archive", archive_status(style, project));
    print_field(
        style,
        2,
        "Archive path",
        style.path(project.sessions_root.display()),
    );
    print_field(
        style,
        2,
        "Known paths",
        style.count(project.known_path_count),
    );
    if let Some(upstream) = &project.git_upstream {
        print_field(style, 2, "Upstream", style.path(upstream));
    }
}

/// Prints one indexed-data status block.
fn print_project_index_status(style: HumanStyle, project: &StatusProject, indent: usize) {
    let heading = if indent == 0 {
        "Indexed Data"
    } else {
        "Indexed"
    };
    if indent == 0 {
        print_section(style, heading);
    } else {
        print_line(indent, style.bold(heading));
    }
    print_field(
        style,
        indent + 2,
        "Sessions",
        style.count(project.session_count),
    );
    print_field(style, indent + 2, "Turns", style.count(project.turn_count));
    print_field(
        style,
        indent + 2,
        "Last activity",
        project
            .last_activity_at
            .as_ref()
            .map(|value| value.to_owned())
            .unwrap_or_else(|| style.muted("none")),
    );
    print_field(
        style,
        indent + 2,
        "Last sync",
        project
            .last_sync_at
            .as_ref()
            .map(|value| value.to_owned())
            .unwrap_or_else(|| style.muted("unknown")),
    );
}

/// Prints the workspace aggregate status block.
fn print_workspace_summary(style: HumanStyle, report: &WorkspaceStatusReport) {
    print_section(style, "Workspace Summary");
    print_field(style, 2, "Projects", style.count(report.projects.len()));
    print_field(
        style,
        2,
        "Indexed sessions",
        style.count(report.total_session_count()),
    );
    print_field(
        style,
        2,
        "Indexed turns",
        style.count(report.total_turn_count()),
    );
    print_field(
        style,
        2,
        "Last activity",
        report
            .latest_activity_at()
            .map(str::to_owned)
            .unwrap_or_else(|| style.muted("none")),
    );
}

/// Prints every workspace project as a readable multi-line block.
fn print_workspace_projects(style: HumanStyle, projects: &[StatusProject]) {
    print_section(style, "Projects");
    if projects.is_empty() {
        print_line(2, style.muted("none"));
        return;
    }

    for (index, project) in projects.iter().enumerate() {
        if index > 0 {
            println!();
        }
        print_workspace_project_status(style, project);
    }
}

/// Prints one compact workspace project row.
fn print_workspace_project_status(style: HumanStyle, project: &StatusProject) {
    print_line(2, style.bold(&project.name));
    print_field(style, 4, "ID", style.muted(&project.id));
    print_field(style, 4, "Path", style.path(project.local_path.display()));
    print_field(style, 4, "Archive", archive_status(style, project));
    print_field(
        style,
        4,
        "Archive path",
        style.path(project.sessions_root.display()),
    );
    print_field(style, 4, "Indexed", indexed_summary(style, project));
    print_field(
        style,
        4,
        "Last activity",
        project
            .last_activity_at
            .as_ref()
            .map(|value| value.to_owned())
            .unwrap_or_else(|| style.muted("none")),
    );
    print_field(
        style,
        4,
        "Last sync",
        project
            .last_sync_at
            .as_ref()
            .map(|value| value.to_owned())
            .unwrap_or_else(|| style.muted("unknown")),
    );
    if project.sync_check.is_some() {
        print_sync_check(style, project.sync_check.as_ref(), "Sync Check", 4);
    }
    if !project.issues.is_empty() {
        print_project_issues(style, project, 4);
    }
}

/// Prints one optional sync dry-run block.
fn print_sync_check(
    style: HumanStyle,
    check: Option<&StatusSyncCheck>,
    label: &str,
    indent: usize,
) {
    let Some(check) = check else {
        return;
    };

    match check {
        StatusSyncCheck::Planned(plan) => print_sync_plan(style, plan, label, indent),
        StatusSyncCheck::Failed(failure) => {
            print_line(
                indent,
                format!("{}: {}", style.bold(label), style.error("failed")),
            );
            print_field(style, indent + 2, "Error", style.error(&failure.message));
        }
    }
}

/// Prints one successful sync dry-run summary.
fn print_sync_plan(style: HumanStyle, plan: &StatusSyncPlan, label: &str, indent: usize) {
    print_line(indent, style.bold(label));
    print_field(
        style,
        indent + 2,
        "Providers",
        format_sources(&plan.sources),
    );
    print_field(
        style,
        indent + 2,
        "Sessions",
        format!(
            "{} pending, {} unchanged",
            style.count(plan.sessions_to_copy),
            style.count(plan.sessions_unchanged)
        ),
    );
    print_field(
        style,
        indent + 2,
        "Auxiliary",
        format!(
            "{} pending, {} unchanged",
            style.count(plan.auxiliary_to_copy),
            style.count(plan.auxiliary_unchanged)
        ),
    );
    print_field(
        style,
        indent + 2,
        "Known paths",
        format!("{} new", style.count(plan.new_known_path_count)),
    );
    print_field(
        style,
        indent + 2,
        "Manifest",
        if plan.manifest_written {
            style.warn("would update")
        } else {
            style.ok("up to date")
        },
    );
    print_field(
        style,
        indent + 2,
        "Config",
        if plan.config_written {
            style.warn("would update")
        } else {
            style.ok("up to date")
        },
    );
    if !plan.warnings.is_empty() {
        print_line(indent + 2, style.warn("Warnings"));
        for warning in &plan.warnings {
            print_line(indent + 4, style.warn(format!("- {warning}")));
        }
    }
}

/// Prints project-local issues when present.
fn print_project_issues(style: HumanStyle, project: &StatusProject, indent: usize) {
    if project.issues.is_empty() {
        return;
    }
    print_line(indent, style.error("Issues"));
    for issue in &project.issues {
        print_line(indent + 2, style.error(format!("- {issue}")));
    }
}

/// Prints the final overall status block.
fn print_overall_status(style: HumanStyle, status: &'static str) {
    print_section(style, "Status");
    let status = if status == "ok" {
        style.ok(status)
    } else {
        style.warn(status)
    };
    print_field(style, 2, "Overall", status);
}

/// Returns the overall human status label for one report.
fn format_overall_status(
    root_issues: &[String],
    sources: &[StatusSource],
    projects: &[StatusProject],
) -> &'static str {
    if root_issues.is_empty()
        && !sources.iter().any(source_needs_attention)
        && !projects.iter().any(project_needs_attention)
    {
        "ok"
    } else {
        "needs attention"
    }
}

/// Returns whether one source row deserves attention.
fn source_needs_attention(source: &StatusSource) -> bool {
    source.configured && source.enabled && !source.path_exists
}

/// Returns whether one project row deserves attention.
fn project_needs_attention(project: &StatusProject) -> bool {
    !project.issues.is_empty() || project.has_failed_check()
}

/// Prepares and optionally executes the project-scoped sync workflow.
fn run_sync(args: SyncArgs) -> Result<()> {
    let plan = prepare_sync(
        Some(args.root),
        SyncOptions {
            provider_filter: args.provider.into_iter().map(ProviderArg::into).collect(),
        },
    )
    .map_err(add_init_hint_for_unconfigured_project)?;
    let style = HumanStyle::stdout();

    print_project_run_header(
        style,
        "Sync",
        &plan.project_name,
        &plan.project_root,
        Some(plan.sessions_root.as_path()),
    );
    println!();
    print_section(style, "Plan");
    print_field(style, 2, "Providers", format_sources(&plan.sources));
    print_field(
        style,
        2,
        "Sessions",
        format!(
            "{} to copy, {} unchanged",
            style.count(plan.sessions_to_copy()),
            style.count(plan.sessions_unchanged)
        ),
    );
    print_field(
        style,
        2,
        "Auxiliary",
        format!(
            "{} to copy, {} unchanged",
            style.count(plan.auxiliary_to_copy()),
            style.count(plan.auxiliary_unchanged)
        ),
    );
    print_field(
        style,
        2,
        "Known paths",
        format!("{} new", style.count(plan.new_known_paths.len())),
    );
    for warning in &plan.warnings {
        print_warning(warning);
    }

    if args.dry_run {
        println!();
        print_section(style, "Status");
        print_field(style, 2, "Overall", style.warn("dry run only"));
        print_line(2, style.muted("No files were written."));
        return Ok(());
    }

    let report = execute_sync(plan)?;
    println!();
    print_sync_result(style, &report);
    println!();
    print_section(style, "Status");
    print_field(style, 2, "Overall", style.ok("synced"));

    Ok(())
}

/// Prints the common project/path header for human workflow commands.
fn print_project_run_header(
    style: HumanStyle,
    title: &str,
    project_name: &str,
    project_root: &std::path::Path,
    archive: Option<&std::path::Path>,
) {
    print_section(style, title);
    print_field(style, 2, "Project", project_name);
    print_field(style, 2, "Project root", style.path(project_root.display()));
    if let Some(archive) = archive {
        print_field(style, 2, "Archive", style.path(archive.display()));
    }
}

/// Prints one executed sync summary block.
fn print_sync_result(style: HumanStyle, report: &SyncReport) {
    print_section(style, "Sync");
    print_field(
        style,
        2,
        "Sessions",
        format!(
            "{} copied, {} unchanged",
            style.count(report.sessions_copied),
            style.count(report.sessions_unchanged)
        ),
    );
    print_field(
        style,
        2,
        "Auxiliary",
        format!(
            "{} copied, {} unchanged",
            style.count(report.auxiliary_copied),
            style.count(report.auxiliary_unchanged)
        ),
    );
    print_field(
        style,
        2,
        "Known paths",
        format!("{} new", style.count(report.new_known_paths.len())),
    );
}

/// Prints one index summary block.
fn print_index_summary(style: HumanStyle, report: &IndexReport) {
    print_section(style, "Indexed Data");
    print_field(style, 2, "Providers", format_sources(&report.providers));
    print_field(
        style,
        2,
        "Index DB",
        style.path(report.index_db_path.display()),
    );
    print_field(
        style,
        2,
        "Sessions discovered",
        style.count(report.sessions_discovered),
    );
    print_field(
        style,
        2,
        "Sessions skipped this run",
        style.count(report.sessions_skipped_this_run),
    );
    print_field(
        style,
        2,
        "Sessions currently indexed",
        style.count(report.sessions_currently_indexed),
    );
    print_field(
        style,
        2,
        "Turns currently indexed",
        style.count(report.turns_currently_indexed),
    );
    let skipped = report.skipped_rollouts.len();
    let skipped = if skipped == 0 {
        style.ok(skipped)
    } else {
        style.warn(skipped)
    };
    print_field(style, 2, "Skipped rollout files", skipped);
}

/// Adds a `darc init` hint when sync or refresh runs outside a configured project.
fn add_init_hint_for_unconfigured_project(error: anyhow::Error) -> anyhow::Error {
    if error.chain().any(|cause| {
        cause.to_string() == "current directory does not match any configured darc project"
    }) {
        anyhow::anyhow!(
            "{error:#}\nrun `darc init` from this project root first (reuse the same `--root` flag if you passed one here)"
        )
    } else {
        error
    }
}

/// Indexes archived sessions for the active project into SQLite.
fn run_index(args: IndexArgs) -> Result<()> {
    let report = index_project_sessions(
        Some(args.root),
        IndexOptions {
            provider_filter: args.provider.into_iter().map(ProviderArg::into).collect(),
        },
    )?;
    let style = HumanStyle::stdout();

    for skipped in &report.skipped_rollouts {
        print_warning(format_skipped_rollout(skipped));
    }

    print_project_run_header(
        style,
        "Index",
        &report.project_name,
        &report.project_root,
        Some(report.sessions_root.as_path()),
    );
    println!();
    print_index_summary(style, &report);
    println!();
    print_section(style, "Status");
    let status = if report.skipped_rollouts.is_empty() {
        style.ok("indexed")
    } else {
        style.warn("indexed with skipped rollouts")
    };
    print_field(style, 2, "Overall", status);

    Ok(())
}

/// Prints the combined sync and index summary for one refreshed project.
fn print_refresh_report(report: &RefreshReport) {
    let style = HumanStyle::stdout();
    print_refresh_report_with_style(style, report);
}

/// Prints the combined sync and index summary using one resolved style context.
fn print_refresh_report_with_style(style: HumanStyle, report: &RefreshReport) {
    for warning in &report.sync.warnings {
        print_project_warning(&report.sync.project_name, warning);
    }
    for skipped in &report.index.skipped_rollouts {
        print_project_warning(&report.sync.project_name, format_skipped_rollout(skipped));
    }

    print_project_run_header(
        style,
        "Refresh",
        &report.sync.project_name,
        &report.sync.project_root,
        Some(report.sync.sessions_root.as_path()),
    );
    println!();
    print_section(style, "Providers");
    match format_refresh_provider_lines(report) {
        RefreshProviderLines::Shared(providers) => print_field(style, 2, "Selected", providers),
        RefreshProviderLines::Split {
            sync_providers,
            index_providers,
        } => {
            print_field(style, 2, "Sync", sync_providers);
            print_field(style, 2, "Index", index_providers);
        }
    }
    println!();
    print_sync_result(style, &report.sync);
    println!();
    print_index_summary(style, &report.index);
    println!();
    print_section(style, "Changes");
    print_field(
        style,
        2,
        "Manifest",
        if report.sync.manifest_written {
            style.ok("updated")
        } else {
            style.muted("unchanged")
        },
    );
    print_field(
        style,
        2,
        "Config",
        if report.sync.config_written {
            style.ok("updated")
        } else {
            style.muted("unchanged")
        },
    );
    println!();
    print_section(style, "Status");
    let status = if report.index.skipped_rollouts.is_empty() {
        style.ok("refreshed")
    } else {
        style.warn("refreshed with skipped rollouts")
    };
    print_field(style, 2, "Overall", status);
}

/// Prints one multi-project refresh report with per-project results and totals.
fn print_refresh_all_report(report: &RefreshAllBestEffortReport) {
    let style = HumanStyle::stdout();
    for (index, project) in report.projects.iter().enumerate() {
        if index > 0 {
            println!();
        }
        print_refresh_all_project_report(style, project);
    }
    println!();
    print_section(style, "Workspace Summary");
    print_field(style, 2, "Succeeded", style.ok(report.refreshed_count()));
    let failed = report.failed_count();
    let failed = if failed == 0 {
        style.ok(failed)
    } else {
        style.error(failed)
    };
    print_field(style, 2, "Failed", failed);
}

/// Prints one project-scoped entry from a multi-project refresh report.
fn print_refresh_all_project_report(style: HumanStyle, project: &RefreshProjectAttempt) {
    match project {
        RefreshProjectAttempt::Refreshed(report) => print_refresh_report_with_style(style, report),
        RefreshProjectAttempt::Failed(failure) => print_refresh_project_failure(style, failure),
    }
}

/// Prints one structured project refresh failure from a best-effort workspace refresh.
fn print_refresh_project_failure(style: HumanStyle, failure: &RefreshProjectFailure) {
    print_project_run_header(
        style,
        "Refresh",
        &failure.project_name,
        &failure.project_root,
        None,
    );
    println!();
    print_section(style, "Status");
    print_field(style, 2, "Overall", style.error("failed"));
    print_field(
        style,
        2,
        "Error",
        style.error(format!("{:#}", failure.error)),
    );
}

/// Runs the hidden Codex rollout schema audit command with dedicated exit codes.
fn run_codex_schema_audit_command(args: CodexSchemaAuditArgs) -> i32 {
    match run_codex_schema_audit_with_progress(
        CodexSchemaAuditOptions {
            cache_dir: args.cache_dir,
        },
        |message| eprintln!("[codex-schema-audit] {message}"),
    ) {
        Ok(report) => {
            println!("{}", format_codex_schema_audit_report(&report));
            codex_schema_audit_exit_code(&report)
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            2
        }
    }
}

/// Runs the hidden Claude rollout schema audit command with dedicated exit codes.
fn run_claude_schema_audit_command(args: ClaudeSchemaAuditArgs) -> i32 {
    match run_claude_schema_audit_with_progress(
        ClaudeSchemaAuditOptions {
            cache_dir: args.cache_dir,
            use_host_auth: args.use_host_auth,
            sample_stride: args.sample_stride,
            from_version: args.from_version,
            survey_mode: args.survey_mode.into(),
        },
        |message| eprintln!("[claude-schema-audit] {message}"),
    ) {
        Ok(report) => {
            println!("{}", format_claude_schema_audit_report(&report));
            claude_schema_audit_exit_code(&report)
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            2
        }
    }
}

impl From<ProviderArg> for SourceKind {
    fn from(value: ProviderArg) -> Self {
        match value {
            ProviderArg::Claude => SourceKind::Claude,
            ProviderArg::Codex => SourceKind::Codex,
        }
    }
}

impl From<ClaudeSurveyModeArg> for ClaudeSchemaSurveyMode {
    fn from(value: ClaudeSurveyModeArg) -> Self {
        match value {
            ClaudeSurveyModeArg::Refine => ClaudeSchemaSurveyMode::Refine,
            ClaudeSurveyModeArg::Coarse => ClaudeSchemaSurveyMode::Coarse,
        }
    }
}

/// Formats a source list for compact CLI output.
fn format_sources(sources: &[SourceKind]) -> String {
    sources
        .iter()
        .map(|source| source.title())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Stores the provider lines rendered for one refresh report.
enum RefreshProviderLines {
    Shared(String),
    Split {
        sync_providers: String,
        index_providers: String,
    },
}

/// Formats the provider lines for one refresh report.
fn format_refresh_provider_lines(report: &RefreshReport) -> RefreshProviderLines {
    let sync_providers = format_sources(&report.sync.sources);
    let index_providers = format_sources(&report.index.providers);
    if report.sync.sources == report.index.providers {
        RefreshProviderLines::Shared(sync_providers)
    } else {
        RefreshProviderLines::Split {
            sync_providers,
            index_providers,
        }
    }
}

/// Formats one skipped rollout warning for `darc index`.
fn format_skipped_rollout(skipped: &SkippedRollout) -> String {
    let mut details = Vec::new();
    if let Some(session_id) = &skipped.logical_session_id {
        details.push(format!("session_id={session_id}"));
    }
    if let Some(cli_version) = &skipped.cli_version {
        details.push(format!("cli_version={cli_version}"));
    }
    if details.is_empty() {
        format!(
            "skipped {} rollout {}: {}",
            skipped.provider.title(),
            skipped.source_path.display(),
            skipped.reason
        )
    } else {
        format!(
            "skipped {} rollout {} ({}): {}",
            skipped.provider.title(),
            skipped.source_path.display(),
            details.join(", "),
            skipped.reason
        )
    }
}

/// Returns the CLI exit code for one Codex schema audit report.
fn codex_schema_audit_exit_code(report: &CodexSchemaAuditReport) -> i32 {
    if report.is_compatible() { 0 } else { 1 }
}

/// Returns the CLI exit code for one Claude schema audit report.
fn claude_schema_audit_exit_code(report: &ClaudeSchemaAuditReport) -> i32 {
    if report.is_compatible() { 0 } else { 1 }
}

/// Formats one Codex schema audit report for the hidden CLI command.
fn format_codex_schema_audit_report(report: &CodexSchemaAuditReport) -> String {
    let mut lines = vec![
        format!(
            "Status: {}",
            if report.is_compatible() {
                "compatible"
            } else {
                "schema drift detected"
            }
        ),
        format!("Release Source: {}", report.release_source),
        format!("Binary Cache: {}", report.binary_cache_dir.display()),
        format!(
            "Latest Stable Codex Release Tag: {}",
            report.latest_stable_release_tag
        ),
        format!(
            "Latest Exact-Covered Darc Version: {}",
            report.latest_exact_covered_version
        ),
        format!("Audited Tag Range: {}", report.audited_tag_range()),
    ];

    match &report.outcome {
        CodexSchemaAuditOutcome::Compatible => {
            lines.push(format!(
                "Compatible across {} audited stable release tag(s).",
                report.audited_tags.len()
            ));
        }
        CodexSchemaAuditOutcome::Drift(drift) => {
            lines.push(format!("First Drift Tag: {}", drift.first_drift_tag));
            lines.push("Schema Differences:".to_owned());
            lines.extend(
                drift
                    .difference_summary
                    .iter()
                    .map(|line| format!("- {line}")),
            );
            lines.push("Likely Darc Files To Update:".to_owned());
            lines.extend(
                drift
                    .likely_files_to_update
                    .iter()
                    .map(|path| format!("- {path}")),
            );
        }
    }

    lines.join("\n")
}

/// Formats one Claude schema audit report for the hidden CLI command.
fn format_claude_schema_audit_report(report: &ClaudeSchemaAuditReport) -> String {
    let mut lines = vec![
        format!(
            "Status: {}",
            if report.is_compatible() {
                "compatible"
            } else {
                "schema drift detected"
            }
        ),
        format!("Release Source: {}", report.release_source),
        format!("Binary Cache: {}", report.binary_cache_dir.display()),
        format!(
            "Latest Published Claude Version: {}",
            report.latest_published_version
        ),
        format!(
            "Latest Exact-Covered Darc Version: {}",
            report.latest_exact_covered_version
        ),
        format!("Audited Version Range: {}", report.audited_version_range()),
        format!(
            "Inspected Versions: {}",
            report.inspected_versions.join(", ")
        ),
        format!("Sampling Stride: {}", report.sample_stride),
        format!(
            "Survey Mode: {}",
            match report.survey_mode {
                ClaudeSchemaSurveyMode::Refine => "refine",
                ClaudeSchemaSurveyMode::Coarse => "coarse",
            }
        ),
        format!(
            "Auth Mode: {}",
            if report.used_host_auth {
                "host (explicit opt-in)"
            } else {
                "isolated (no auth)"
            }
        ),
    ];

    match &report.outcome {
        ClaudeSchemaAuditOutcome::Compatible => {
            if report.sample_stride == 1 {
                lines.push(format!(
                    "Compatible across {} audited Claude version(s).",
                    report.audited_versions.len()
                ));
            } else {
                lines.push(format!(
                    "Compatible across {} Claude version(s) in range with {} directly inspected version(s).",
                    report.audited_versions.len(),
                    report.inspected_versions.len()
                ));
            }
        }
        ClaudeSchemaAuditOutcome::Drift(drift) => {
            lines.push(format!(
                "First Drift Version: {}",
                drift.first_drift_version
            ));
            lines.push("Transcript Differences:".to_owned());
            lines.extend(
                drift
                    .difference_summary
                    .iter()
                    .map(|line| format!("- {line}")),
            );
            lines.push("Likely Darc Files To Update:".to_owned());
            lines.extend(
                drift
                    .likely_files_to_update
                    .iter()
                    .map(|path| format!("- {path}")),
            );
        }
    }

    if let Some(drift) = &report.supplementary_sdk_drift {
        lines.push(format!(
            "Supplementary Agent SDK Drift Version: {}",
            drift.first_drift_version
        ));
        lines.push("Supplementary Agent SDK Differences:".to_owned());
        lines.extend(
            drift
                .difference_summary
                .iter()
                .map(|line| format!("- {line}")),
        );
    }

    if !report.assumed_compatible_intervals.is_empty() {
        lines.push("Assumed Compatible Unsampled Intervals:".to_owned());
        lines.extend(
            report
                .assumed_compatible_intervals
                .iter()
                .map(|line| format!("- {line}")),
        );
    }

    if !report.transcript_drift_windows.is_empty() {
        lines.push("Sampled Transcript Drift Windows:".to_owned());
        lines.extend(report.transcript_drift_windows.iter().map(|window| {
            format!(
                "- {} ..= {} (sampled compatible {}, sampled drift {})",
                window.window_start_version,
                window.window_end_version,
                window.sampled_compatible_version,
                window.sampled_drift_version
            )
        }));
    }

    lines.join("\n")
}
