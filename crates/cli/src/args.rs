use std::path::PathBuf;

use clap::{Args, ColorChoice, Parser, Subcommand};
use darc_core::{
    default_root_path,
    query::{
        DEFAULT_MATCHED_PATH_LIMIT, DEFAULT_QUERY_PAGE_LIMIT, DEFAULT_SESSION_BUNDLE_TURN_LIMIT,
        DEFAULT_TURN_STEP_LIMIT, DEFAULT_WORKSPACE_RECENT_SESSION_LIMIT, SearchEvidenceField,
    },
};

use crate::query_commands::{
    default_query_page_limit_help, parse_search_evidence_field, parse_window_days,
    search_evidence_field_exclude_help, search_evidence_field_include_help,
    search_match_limit_help,
};

mod help;
mod value;

pub(crate) use help::*;
pub(crate) use value::*;

#[derive(Debug, Parser)]
#[command(
    name = "darc",
    version,
    about = "Archive, index, and query coding-agent sessions. Agents can run `darc agent-help` for usage guidance.",
    color = ColorChoice::Auto,
    styles = HELP_STYLES,
    after_help = root_after_help()
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Commands,
}

/// Supported CLI subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    /// Detect local sources and create the shared darc config.
    Init(InitArgs),
    #[command(
        about = "Sync then index archived sessions for the active project",
        long_about = "Sync then index archived sessions for the active project.\n\nThis is the daily happy path after `darc init`.\nBy default it refreshes the project resolved from the current directory.\nUse `--auto` to enable automatic background refresh and start it now.\nUse `--provider` to limit both sync and index to selected providers.\nUse `--all` to refresh every registered project in the shared darc workspace.\nWhen `--all` is set, darc continues past per-project failures, prints a workspace summary, and exits non-zero if any project failed."
    )]
    Refresh(RefreshArgs),
    #[command(
        about = "Show Darc status for the active project or workspace",
        long_about = "Show Darc status for the active project or workspace.\n\nBy default this resolves the project from the current directory and prints root, config, source, archive, index, and sync-manifest status.\nUse `--workspace` to summarize every configured project in the shared Darc workspace.\nUse `--check` to run sync planning without writing manifests, config, archives, or SQLite."
    )]
    Status(StatusArgs),
    #[command(
        name = "agent-help",
        about = "Show agent-friendly Darc usage guidance",
        long_about = "Show agent-friendly Darc usage guidance.\n\nUse this from AGENTS.md, CLAUDE.md, or an agent session when prior coding-agent history might affect the current task. The guide explains safe read commands, evidence handles, file pivots, output limits, and mutating boundaries.\n\nPass --agents-md-line to print the one-line AGENTS.md trigger that points agents back to this command."
    )]
    AgentHelp(AgentHelpArgs),
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
        about = "Manage Git-backed Darc share remotes",
        long_about = "Manage Git-backed Darc share remotes stored in Darc config.toml.\n\nDarc share remotes point to Git repositories or project origins that store encrypted shared index artifacts on darc/* branches."
    )]
    Remote(RemoteArgs),
    #[command(
        about = "Manage encrypted shared index selection and recipients",
        long_about = "Manage encrypted shared index selection and recipients for the active project.\n\nUse `darc share include` or `darc share exclude` to choose which local sessions are exported. Use recipient subcommands to manage age public-key recipients in Darc config.toml."
    )]
    Share(ShareArgs),
    #[command(
        about = "Export selected shared sessions and push a darc/* branch",
        long_about = "Export selected shared sessions for the active project, encrypt the redacted index data for configured recipients, commit it into the local share cache, and push it to the Git branch darc/<branch>."
    )]
    Push(ShareBranchArgs),
    #[command(
        about = "Fetch a darc/* shared index branch",
        long_about = "Fetch the Git branch darc/<branch> into Darc's local share cache without importing it."
    )]
    Fetch(ShareBranchArgs),
    #[command(
        about = "Import a fetched darc/* shared index branch",
        long_about = "Import already fetched encrypted shared index artifacts from Darc's local share cache into the local SQLite index."
    )]
    Merge(ShareBranchArgs),
    #[command(
        about = "Fetch and import a darc/* shared index branch",
        long_about = "Fetch darc/<branch> from Git and import its encrypted shared index artifacts into the local SQLite index."
    )]
    Pull(ShareBranchArgs),
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
        about = "Index archived sessions into SQLite",
        long_about = "Index archived sessions into the shared Darc SQLite database.\n\nWithout `--rebuild`, this indexes the active project. Run it after `darc sync` when you want to refresh searchable/queryable state without copying new archive files.\n\nUse `--rebuild` only when Darc reports that the SQLite index cannot be opened or migrated. Rebuild deletes the shared local index cache, then recreates it from every configured project's archived sessions.",
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
pub(crate) struct InitArgs {
    #[arg(
        long,
        help_heading = "Mode",
        help = "Show what would be written without changing files"
    )]
    pub(crate) dry_run: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Create or update config under this Darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Syncs then indexes archived sessions for one or all projects.
#[derive(Debug, Args)]
pub(crate) struct RefreshArgs {
    #[arg(
        long = "provider",
        value_enum,
        help_heading = "Selection",
        help = "Limit both sync and index to the selected providers"
    )]
    pub(crate) provider: Vec<ProviderArg>,

    #[arg(
        long,
        help_heading = "Scope",
        help = "Refresh every registered project, continue past per-project failures, and summarize the results"
    )]
    pub(crate) all: bool,

    #[arg(
        long,
        conflicts_with_all = [
            "provider",
            "all",
            "watch",
            "debounce",
            "min_interval",
            "reconcile_interval",
            "poll"
        ],
        help_heading = "Mode",
        help = "Enable automatic background refresh for all projects and start it now"
    )]
    pub(crate) auto: bool,

    #[arg(
        long,
        help_heading = "Mode",
        help = "Keep refreshing when Claude or Codex session files change"
    )]
    pub(crate) watch: bool,

    #[arg(
        long,
        value_name = "DURATION",
        requires = "watch",
        help_heading = "Mode",
        help = "Quiet period before a watched refresh, such as 30s or 2m"
    )]
    pub(crate) debounce: Option<String>,

    #[arg(
        long = "min-interval",
        value_name = "DURATION",
        requires = "watch",
        help_heading = "Mode",
        help = "Minimum time between watched refresh runs, such as 60s or 5m"
    )]
    pub(crate) min_interval: Option<String>,

    #[arg(
        long = "reconcile-interval",
        value_name = "DURATION",
        requires = "watch",
        help_heading = "Mode",
        help = "Periodic safety refresh interval for watch mode, such as 10m"
    )]
    pub(crate) reconcile_interval: Option<String>,

    #[arg(
        long,
        requires = "watch",
        help_heading = "Mode",
        help = "Use periodic polling instead of native filesystem events"
    )]
    pub(crate) poll: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    pub(crate) root: PathBuf,
}

/// Manages the background Darc refresh service.
#[derive(Debug, Args)]
pub(crate) struct ServiceArgs {
    #[arg(
        long,
        default_value_os_t = default_root_path(),
        global = true,
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    pub(crate) root: PathBuf,

    #[command(subcommand)]
    pub(crate) command: ServiceCommands,
}

/// Represents the supported service lifecycle commands.
#[derive(Debug, Subcommand)]
pub(crate) enum ServiceCommands {
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
pub(crate) struct UpgradeArgs {
    #[arg(
        long,
        help_heading = "Mode",
        help = "Only check whether a newer Darc release is available"
    )]
    pub(crate) check: bool,

    #[arg(
        long,
        requires = "check",
        help_heading = "Output",
        help = "Write the upgrade check result as a machine-readable JSON envelope"
    )]
    pub(crate) json: bool,

    #[arg(
        long,
        global = true,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root for cached upgrade nudges"
    )]
    pub(crate) root: PathBuf,

    #[command(subcommand)]
    pub(crate) command: Option<UpgradeCommands>,
}

/// Represents supported upgrade-state commands.
#[derive(Debug, Subcommand)]
pub(crate) enum UpgradeCommands {
    /// Dismiss one cached upgrade nudge.
    Dismiss(UpgradeDismissArgs),
}

/// Dismisses one cached Darc upgrade version.
#[derive(Debug, Args)]
pub(crate) struct UpgradeDismissArgs {
    #[arg(value_name = "VERSION", help = "Darc version to dismiss")]
    pub(crate) version: Option<String>,
}

/// Shows Darc status for the active project or workspace.
#[derive(Debug, Args)]
pub(crate) struct StatusArgs {
    #[arg(
        long,
        help_heading = "Scope",
        help = "Show status for the shared Darc workspace instead of the active project"
    )]
    pub(crate) workspace: bool,

    #[arg(
        long,
        help_heading = "Mode",
        help = "Run sync planning without writing manifests, config, archives, or SQLite"
    )]
    pub(crate) check: bool,

    #[arg(
        long,
        help_heading = "Output",
        help = "Write status as a machine-readable JSON envelope"
    )]
    pub(crate) json: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    pub(crate) root: PathBuf,
}

/// Shows agent-facing usage guidance without reading or writing Darc state.
#[derive(Debug, Args)]
pub(crate) struct AgentHelpArgs {
    #[arg(
        long = "agents-md-line",
        help_heading = "Output",
        help = "Print only the marker-wrapped one-line AGENTS.md guidance"
    )]
    pub(crate) agents_md_line: bool,
}

/// Sync matching Claude and Codex sessions into the project archive.
#[derive(Debug, Args)]
pub(crate) struct SyncArgs {
    #[arg(
        long,
        help_heading = "Mode",
        help = "Preview pending copies without writing files"
    )]
    pub(crate) dry_run: bool,

    #[arg(
        long = "provider",
        value_enum,
        help_heading = "Selection",
        help = "Limit sync to the selected providers"
    )]
    pub(crate) provider: Vec<ProviderArg>,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    pub(crate) root: PathBuf,
}

/// Link one configured project's historical paths into the active project.
#[derive(Debug, Args)]
pub(crate) struct LinkArgs {
    #[arg(
        long,
        help_heading = "Mode",
        help = "Preview link changes without writing config"
    )]
    pub(crate) dry_run: bool,

    #[arg(
        value_name = "PROJECT",
        help = "Configured source project name to link into the current project"
    )]
    pub(crate) project: String,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    pub(crate) root: PathBuf,
}

/// Remove one configured project and its archived/indexed data.
#[derive(Debug, Args)]
pub(crate) struct RemoveArgs {
    #[arg(
        long,
        help_heading = "Mode",
        help = "Preview removal without writing config, archive, or index changes"
    )]
    pub(crate) dry_run: bool,

    #[arg(value_name = "PROJECT", help = "Configured project name to remove")]
    pub(crate) project: String,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    pub(crate) root: PathBuf,
}

/// Rebuild one configured project's history under the active project's id, then remove the old project.
#[derive(Debug, Args)]
pub(crate) struct RenameArgs {
    #[arg(
        long,
        help_heading = "Mode",
        help = "Preview rename workflow without writing config, archive, or index changes"
    )]
    pub(crate) dry_run: bool,

    #[arg(
        value_name = "PROJECT",
        help = "Old configured project name to rebuild into the current project"
    )]
    pub(crate) project: String,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    pub(crate) root: PathBuf,
}

/// Index archived sessions from selected providers for the active project into SQLite.
#[derive(Debug, Args)]
pub(crate) struct IndexArgs {
    #[arg(
        long = "provider",
        value_enum,
        help_heading = "Selection",
        help = "Limit non-rebuild indexing to the selected providers"
    )]
    pub(crate) provider: Vec<ProviderArg>,

    #[arg(
        long,
        help_heading = "Mode",
        help = "Delete the shared SQLite index and rebuild it from every configured project's archived sessions"
    )]
    pub(crate) rebuild: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    pub(crate) root: PathBuf,
}

/// Lists indexed Darc resources through the canonical JSON read surface.
#[derive(Debug, Args)]
pub(crate) struct ListArgs {
    #[arg(
        long,
        default_value_os_t = default_root_path(),
        global = true,
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,

    #[arg(
        long,
        value_enum,
        default_value_t = ColorArg::Auto,
        global = true,
        help_heading = "Presentation",
        help = "Control ANSI color in JSON output"
    )]
    pub(crate) color: ColorArg,

    #[command(subcommand)]
    pub(crate) command: ListCommands,
}

/// Represents the supported canonical list commands.
#[derive(Debug, Subcommand)]
pub(crate) enum ListCommands {
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
pub(crate) struct ShowArgs {
    #[arg(
        long,
        default_value_os_t = default_root_path(),
        global = true,
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,

    #[arg(
        long,
        value_enum,
        default_value_t = ColorArg::Auto,
        global = true,
        help_heading = "Presentation",
        help = "Control ANSI color in JSON output"
    )]
    pub(crate) color: ColorArg,

    #[command(subcommand)]
    pub(crate) command: ShowCommands,
}

/// Represents the supported canonical show commands.
#[derive(Debug, Subcommand)]
pub(crate) enum ShowCommands {
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
pub(crate) struct SearchArgs {
    #[arg(
        long,
        value_enum,
        default_value_t = SearchModeArg::Keyword,
        help_heading = "Search",
        help = "Choose how QUERY is interpreted"
    )]
    pub(crate) mode: SearchModeArg,

    #[arg(
        value_name = "QUERY",
        help = "Search query text or path pattern",
        long_help = search_query_help()
    )]
    pub(crate) query_arg: Option<String>,

    #[arg(
        long,
        allow_hyphen_values = true,
        value_name = "QUERY",
        help_heading = "Search",
        help = "Pass QUERY by flag; use this when the value starts with '-'"
    )]
    pub(crate) query: Option<String>,

    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Search this configured project id. Defaults to the project resolved from the current directory"
    )]
    pub(crate) project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict search to this provider"
    )]
    pub(crate) provider: Option<ProviderArg>,

    #[arg(
        long = "session-id",
        alias = "session",
        help_heading = "Scope",
        help = "Restrict search to this session id or unambiguous UUID prefix"
    )]
    pub(crate) session_id: Option<String>,

    #[arg(
        long,
        conflicts_with = "scope",
        help_heading = "Scope",
        help = "Search only imported shared sessions"
    )]
    pub(crate) shared: bool,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Choose local, shared, or all sessions"
    )]
    pub(crate) scope: Option<SessionScopeArg>,

    #[arg(
        long,
        help_heading = "Scope",
        help = "Restrict shared-session results to this author user id, email, or display name"
    )]
    pub(crate) author: Option<String>,

    #[arg(
        long,
        help_heading = "Evidence",
        help = "Include tool output evidence in literal and regex search"
    )]
    pub(crate) include_tool_output: bool,

    #[arg(
        long = "field",
        value_name = "FIELD",
        value_parser = parse_search_evidence_field,
        help_heading = "Evidence",
        help = search_evidence_field_include_help()
    )]
    pub(crate) fields: Vec<SearchEvidenceField>,

    #[arg(
        long = "exclude-field",
        value_name = "FIELD",
        value_parser = parse_search_evidence_field,
        help_heading = "Evidence",
        help = search_evidence_field_exclude_help()
    )]
    pub(crate) excluded_fields: Vec<SearchEvidenceField>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Inclusive started_at lower bound. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    pub(crate) since: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Exclusive started_at upper bound. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    pub(crate) until: Option<String>,

    #[arg(
        long,
        default_value_t = DEFAULT_QUERY_PAGE_LIMIT,
        help_heading = "Result Size",
        help = "Maximum turn hits to return"
    )]
    pub(crate) limit: usize,

    #[arg(
        long,
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of turn hits to skip"
    )]
    pub(crate) offset: usize,

    #[arg(
        long = "matched-path-limit",
        default_value_t = DEFAULT_MATCHED_PATH_LIMIT,
        conflicts_with = "include_all_matched_paths",
        help_heading = "Result Size",
        help = "Maximum matched_paths entries per file-search hit"
    )]
    pub(crate) matched_path_limit: usize,

    #[arg(
        long = "match-limit",
        value_name = "MATCH_LIMIT",
        help_heading = "Result Size",
        help = search_match_limit_help()
    )]
    pub(crate) match_limit: Option<usize>,

    #[arg(
        long = "include-all-matched-paths",
        help_heading = "Result Size",
        help = "Return every matched path in file-search hits"
    )]
    pub(crate) include_all_matched_paths: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,

    #[arg(
        long,
        value_enum,
        default_value_t = ColorArg::Auto,
        help_heading = "Presentation",
        help = "Control ANSI color in JSON output"
    )]
    pub(crate) color: ColorArg,
}

/// Shows indexed stats through the canonical JSON read surface.
#[derive(Debug, Args)]
pub(crate) struct StatsArgs {
    #[arg(
        long,
        default_value_os_t = default_root_path(),
        global = true,
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,

    #[arg(
        long,
        value_enum,
        default_value_t = ColorArg::Auto,
        global = true,
        help_heading = "Presentation",
        help = "Control ANSI color in JSON output"
    )]
    pub(crate) color: ColorArg,

    #[command(subcommand)]
    pub(crate) command: StatsCommands,
}

/// Represents the supported canonical stats commands.
#[derive(Debug, Subcommand)]
pub(crate) enum StatsCommands {
    /// Show workspace stats for one rolling day window.
    Workspace(QueryWorkspaceInsightsArgs),
    /// Show project stats for one configured project.
    Project(QueryProjectInsightsArgs),
    /// Show one turn's derived stats.
    Turn(QueryTurnInsightsArgs),
}

/// Resolves Darc identifiers through the canonical JSON read surface.
#[derive(Debug, Args)]
pub(crate) struct ResolveArgs {
    #[arg(
        long,
        default_value_os_t = default_root_path(),
        global = true,
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,

    #[arg(
        long,
        value_enum,
        default_value_t = ColorArg::Auto,
        global = true,
        help_heading = "Presentation",
        help = "Control ANSI color in JSON output"
    )]
    pub(crate) color: ColorArg,

    #[command(subcommand)]
    pub(crate) command: ResolveCommands,
}

/// Represents the supported canonical resolver commands.
#[derive(Debug, Subcommand)]
pub(crate) enum ResolveCommands {
    /// Resolve a full session id or UUID prefix into canonical matches.
    Session(QueryResolveSessionArgs),
}

/// Manages configured projects through the canonical project namespace.
#[derive(Debug, Args)]
pub(crate) struct ProjectArgs {
    #[command(subcommand)]
    pub(crate) command: ProjectCommands,
}

/// Represents the supported project-management commands.
#[derive(Debug, Subcommand)]
pub(crate) enum ProjectCommands {
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

/// Manages Git-backed Darc share remotes.
#[derive(Debug, Args)]
pub(crate) struct RemoteArgs {
    #[arg(
        long,
        default_value_os_t = default_root_path(),
        global = true,
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    pub(crate) root: PathBuf,

    #[command(subcommand)]
    pub(crate) command: RemoteCommands,
}

/// Represents supported Darc share remote commands.
#[derive(Debug, Subcommand)]
pub(crate) enum RemoteCommands {
    /// Add or update one named Darc share remote.
    Add(RemoteAddArgs),
    /// List configured Darc share remotes.
    List,
}

/// Adds or updates one named Darc share remote.
#[derive(Debug, Args)]
pub(crate) struct RemoteAddArgs {
    #[arg(help = "Remote name, for example team")]
    pub(crate) name: String,
    #[arg(help = "Git remote URL that stores encrypted shared indexes")]
    pub(crate) url: String,
}

/// Manages encrypted shared index selection and recipients.
#[derive(Debug, Args)]
pub(crate) struct ShareArgs {
    #[arg(
        long,
        default_value_os_t = default_root_path(),
        global = true,
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    pub(crate) root: PathBuf,

    #[command(subcommand)]
    pub(crate) command: ShareCommands,
}

/// Represents supported Darc share management commands.
#[derive(Debug, Subcommand)]
pub(crate) enum ShareCommands {
    /// Show share selection status for the active project.
    Status,
    /// Show or create the local age share key.
    Key,
    /// Show the local share author identity.
    Identity,
    /// Set the active project's default share policy.
    Policy(SharePolicyArgs),
    /// Include one session or every local session in sharing.
    Include(ShareSessionSelectionArgs),
    /// Exclude one session or every local session from sharing.
    Exclude(ShareSessionSelectionArgs),
    /// Manage configured age recipients.
    Recipient(ShareRecipientArgs),
}

/// Sets the active project's default share policy.
#[derive(Debug, Args)]
pub(crate) struct SharePolicyArgs {
    #[arg(value_enum, help = "Project sharing policy")]
    pub(crate) policy: SharePolicyArg,
}

/// Selects one session or all sessions for sharing changes.
#[derive(Debug, Args)]
pub(crate) struct ShareSessionSelectionArgs {
    #[arg(
        help = "Session id or unambiguous UUID prefix",
        required_unless_present = "all"
    )]
    pub(crate) session_id: Option<String>,

    #[arg(
        long,
        help_heading = "Selection",
        conflicts_with = "session_id",
        help = "Apply to every local session in the active project"
    )]
    pub(crate) all: bool,

    #[arg(
        long,
        value_enum,
        requires = "session_id",
        help_heading = "Selection",
        help = "Disambiguate a session id or UUID prefix by provider"
    )]
    pub(crate) provider: Option<ProviderArg>,
}

/// Manages configured age recipients.
#[derive(Debug, Args)]
pub(crate) struct ShareRecipientArgs {
    #[command(subcommand)]
    pub(crate) command: ShareRecipientCommands,
}

/// Represents supported age recipient commands.
#[derive(Debug, Subcommand)]
pub(crate) enum ShareRecipientCommands {
    /// Add one age public-key recipient.
    Add(ShareRecipientValueArgs),
    /// Remove one age public-key recipient.
    Remove(ShareRecipientValueArgs),
    /// List configured age public-key recipients.
    List,
}

/// Stores one age recipient argument.
#[derive(Debug, Args)]
pub(crate) struct ShareRecipientValueArgs {
    #[arg(help = "age X25519 public recipient, for example age1...")]
    pub(crate) recipient: String,
}

/// Operates on one Darc shared index branch.
#[derive(Debug, Args)]
pub(crate) struct ShareBranchArgs {
    #[arg(help = "Darc share branch shorthand. `team` maps to git branch `darc/team`")]
    pub(crate) branch: String,

    #[arg(
        long,
        help_heading = "Remote",
        help = "Use a configured Darc share remote instead of the project origin"
    )]
    pub(crate) remote: Option<String>,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Use this Darc root instead of the default"
    )]
    pub(crate) root: PathBuf,
}

/// Queries the workspace/sidebar payload for one darc root.
#[derive(Debug, Args)]
pub(crate) struct QueryWorkspaceArgs {
    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Resolves one full session id or UUID prefix into canonical project/provider/session matches.
#[derive(Debug, Args)]
pub(crate) struct QueryResolveSessionArgs {
    #[arg(help = "Resolve this full UUID or UUID prefix")]
    pub(crate) input: String,

    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Restrict matches to this configured project id"
    )]
    pub(crate) project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict matches to this provider"
    )]
    pub(crate) provider: Option<ProviderArg>,

    #[arg(
        long,
        help_heading = "Output",
        help = "Require exactly one match and return it as one convenience object"
    )]
    pub(crate) pick_one: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Queries the session list for one configured project.
#[derive(Debug, Args)]
pub(crate) struct QuerySessionsArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    pub(crate) project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict sessions to this provider"
    )]
    pub(crate) provider: Option<ProviderArg>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Inclusive latest_turn_at lower bound. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    pub(crate) since: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Exclusive latest_turn_at upper bound. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    pub(crate) until: Option<String>,

    #[arg(
        long = "touched-path",
        help_heading = "Selection",
        help = "Only keep sessions that touched a file path matching this glob"
    )]
    pub(crate) touched_path: Option<String>,

    #[arg(
        long,
        conflicts_with = "scope",
        help_heading = "Scope",
        help = "List only imported shared sessions"
    )]
    pub(crate) shared: bool,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Choose local, shared, or all sessions"
    )]
    pub(crate) scope: Option<SessionScopeArg>,

    #[arg(
        long,
        help_heading = "Scope",
        help = "Restrict shared-session results to this author user id, email, or display name"
    )]
    pub(crate) author: Option<String>,

    #[arg(
        long,
        default_value_t = DEFAULT_QUERY_PAGE_LIMIT,
        help_heading = "Result Size",
        help = "Maximum sessions to return"
    )]
    pub(crate) limit: usize,

    #[arg(
        long,
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of sessions to skip"
    )]
    pub(crate) offset: usize,

    #[arg(
        long,
        value_enum,
        default_value_t = SessionListViewArg::Compact,
        help_heading = "Output",
        help = "Return full session prompts and final messages or compact previews"
    )]
    pub(crate) view: SessionListViewArg,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Lists sessions for one configured project through the canonical read surface.
#[derive(Debug, Args)]
pub(crate) struct ListSessionsArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "List sessions for this configured project id. Defaults to the project resolved from the current directory"
    )]
    pub(crate) project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict sessions to this provider"
    )]
    pub(crate) provider: Option<ProviderArg>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Inclusive latest_turn_at lower bound. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    pub(crate) since: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Exclusive latest_turn_at upper bound. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    pub(crate) until: Option<String>,

    #[arg(
        long = "touching",
        alias = "touched-path",
        help_heading = "Selection",
        help = "Only keep sessions that touched a file path matching this glob"
    )]
    pub(crate) touching: Option<String>,

    #[arg(
        long,
        conflicts_with = "scope",
        help_heading = "Scope",
        help = "List only imported shared sessions"
    )]
    pub(crate) shared: bool,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Choose local, shared, or all sessions"
    )]
    pub(crate) scope: Option<SessionScopeArg>,

    #[arg(
        long,
        help_heading = "Scope",
        help = "Restrict shared-session results to this author user id, email, or display name"
    )]
    pub(crate) author: Option<String>,

    #[arg(
        long,
        default_value_t = DEFAULT_QUERY_PAGE_LIMIT,
        help_heading = "Result Size",
        help = "Maximum sessions to return"
    )]
    pub(crate) limit: usize,

    #[arg(
        long,
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of sessions to skip"
    )]
    pub(crate) offset: usize,

    #[arg(
        long,
        value_enum,
        default_value_t = SessionListViewArg::Compact,
        help_heading = "Output",
        help = "Return full session prompts and final messages or compact previews"
    )]
    pub(crate) view: SessionListViewArg,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Queries the turn list for one session.
#[derive(Debug, Args)]
pub(crate) struct QueryTurnsArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    pub(crate) project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Disambiguate a session id or UUID prefix by provider"
    )]
    pub(crate) provider: Option<ProviderArg>,

    #[arg(
        long,
        conflicts_with = "scope",
        help_heading = "Scope",
        help = "Query an imported shared session"
    )]
    pub(crate) shared: bool,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Choose local, shared, or all sessions"
    )]
    pub(crate) scope: Option<SessionScopeArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Session id or unambiguous UUID prefix to list turns for; required unless --session-id is set"
    )]
    pub(crate) session_id_arg: Option<String>,

    #[arg(
        long = "session-id",
        value_name = "SESSION_ID",
        help_heading = "Identity",
        help = "Session id or unambiguous UUID prefix to list turns for; alternative to positional SESSION_ID"
    )]
    pub(crate) session_id: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Inclusive started_at lower bound. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    pub(crate) since: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Exclusive started_at upper bound. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    pub(crate) until: Option<String>,

    #[arg(
        long,
        default_value_t = DEFAULT_QUERY_PAGE_LIMIT,
        help_heading = "Result Size",
        help = "Maximum turns to return"
    )]
    pub(crate) limit: usize,

    #[arg(
        long,
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of turns to skip"
    )]
    pub(crate) offset: usize,

    #[arg(
        long,
        value_enum,
        default_value_t = TurnListViewArg::Full,
        help_heading = "Output",
        help = "Return full turn summaries or a compact one-line skim"
    )]
    pub(crate) view: TurnListViewArg,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Lists most-touched files or pivots from one file selector.
#[derive(Debug, Args)]
pub(crate) struct QueryFilesArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    pub(crate) project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict file pivots to this provider"
    )]
    pub(crate) provider: Option<ProviderArg>,

    #[arg(
        long,
        help_heading = "Selection",
        help = "Return sessions that touched file paths matching this glob instead of most-touched files"
    )]
    pub(crate) path: Option<String>,

    #[arg(
        value_name = "PATH",
        help = "Return sessions that touched this path or glob instead of most-touched files"
    )]
    pub(crate) path_arg: Option<String>,

    #[arg(
        long = "co-touched-with",
        help_heading = "Selection",
        help = "Return files touched in the same sessions as this seed path instead of most-touched files"
    )]
    pub(crate) co_touched_with: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Inclusive started_at lower bound for file pivots. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    pub(crate) since: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Exclusive started_at upper bound for file pivots. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    pub(crate) until: Option<String>,

    #[arg(
        long,
        default_value_t = DEFAULT_QUERY_PAGE_LIMIT,
        help_heading = "Result Size",
        help = "Maximum rows to return"
    )]
    pub(crate) limit: usize,

    #[arg(
        long,
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of rows to skip"
    )]
    pub(crate) offset: usize,

    #[arg(
        long = "matched-path-limit",
        default_value_t = DEFAULT_MATCHED_PATH_LIMIT,
        conflicts_with = "include_all_matched_paths",
        help_heading = "Result Size",
        help = "Maximum matched_paths entries per path-mode row"
    )]
    pub(crate) matched_path_limit: usize,

    #[arg(
        long = "include-all-matched-paths",
        help_heading = "Result Size",
        help = "Return every matched path in path-mode rows"
    )]
    pub(crate) include_all_matched_paths: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Lists files for one project or session through the canonical read surface.
#[derive(Debug, Args)]
pub(crate) struct ListFilesArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "List files for this configured project id. Defaults to the project resolved from the current directory"
    )]
    pub(crate) project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict file queries to this provider"
    )]
    pub(crate) provider: Option<ProviderArg>,

    #[arg(
        long,
        help_heading = "Selection",
        help = "Return sessions that touched file paths matching this glob instead of most-touched files"
    )]
    pub(crate) path: Option<String>,

    #[arg(
        value_name = "PATH",
        help = "Return sessions that touched this path or glob instead of most-touched files"
    )]
    pub(crate) path_arg: Option<String>,

    #[arg(
        long,
        value_name = "SESSION_ID",
        help_heading = "Selection",
        help = "Return a paginated per-session file summary for this session id or unambiguous UUID prefix"
    )]
    pub(crate) session: Option<String>,

    #[arg(
        long,
        conflicts_with = "scope",
        help_heading = "Scope",
        help = "Allow --session to resolve an imported shared session"
    )]
    pub(crate) shared: bool,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Choose local, shared, or all sessions for --session resolution"
    )]
    pub(crate) scope: Option<SessionScopeArg>,

    #[arg(
        long = "co-touched-with",
        help_heading = "Selection",
        help = "Return files touched in the same sessions as this seed path instead of most-touched files"
    )]
    pub(crate) co_touched_with: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Inclusive started_at lower bound for top/path/co-touch modes. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    pub(crate) since: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Exclusive started_at upper bound for top/path/co-touch modes. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    pub(crate) until: Option<String>,

    #[arg(
        long,
        help_heading = "Result Size",
        help = default_query_page_limit_help(
            "Maximum rows to return in top/path/co-touch/session modes"
        )
    )]
    pub(crate) limit: Option<usize>,

    #[arg(
        long,
        help_heading = "Result Size",
        help = "Number of rows to skip in top/path/co-touch/session modes [default: 0]"
    )]
    pub(crate) offset: Option<usize>,

    #[arg(
        long = "matched-path-limit",
        conflicts_with = "include_all_matched_paths",
        help_heading = "Result Size",
        help = "Maximum matched_paths entries per path-mode row [default: 20]"
    )]
    pub(crate) matched_path_limit: Option<usize>,

    #[arg(
        long = "include-all-matched-paths",
        help_heading = "Result Size",
        help = "Return every matched path in path-mode rows"
    )]
    pub(crate) include_all_matched_paths: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Queries one session-scoped per-file access summary payload.
#[derive(Debug, Args)]
pub(crate) struct QuerySessionFilesArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    pub(crate) project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Disambiguate a session id or UUID prefix by provider"
    )]
    pub(crate) provider: Option<ProviderArg>,

    #[arg(
        long,
        conflicts_with = "scope",
        help_heading = "Scope",
        help = "Query an imported shared session"
    )]
    pub(crate) shared: bool,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Choose local, shared, or all sessions"
    )]
    pub(crate) scope: Option<SessionScopeArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Query this session id or unambiguous UUID prefix; required unless --session-id is set"
    )]
    pub(crate) session_id_arg: Option<String>,

    #[arg(
        long = "session-id",
        value_name = "SESSION_ID",
        help_heading = "Identity",
        help = "Query this session id or unambiguous UUID prefix; alternative to positional SESSION_ID"
    )]
    pub(crate) session_id: Option<String>,

    #[arg(
        long,
        default_value_t = DEFAULT_QUERY_PAGE_LIMIT,
        help_heading = "Result Size",
        help = "Maximum file rows to return"
    )]
    pub(crate) limit: usize,

    #[arg(
        long,
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of file rows to skip"
    )]
    pub(crate) offset: usize,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Queries one composite session bundle payload.
#[derive(Debug, Args)]
pub(crate) struct QuerySessionBundleArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    pub(crate) project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Disambiguate a session id or UUID prefix by provider"
    )]
    pub(crate) provider: Option<ProviderArg>,

    #[arg(
        long,
        conflicts_with = "scope",
        help_heading = "Scope",
        help = "Query an imported shared session"
    )]
    pub(crate) shared: bool,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Choose local, shared, or all sessions"
    )]
    pub(crate) scope: Option<SessionScopeArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Query this session id or unambiguous UUID prefix; required unless --session-id is set"
    )]
    pub(crate) session_id_arg: Option<String>,

    #[arg(
        long = "session-id",
        value_name = "SESSION_ID",
        help_heading = "Identity",
        help = "Query this session id or unambiguous UUID prefix; alternative to positional SESSION_ID"
    )]
    pub(crate) session_id: Option<String>,

    #[arg(
        long = "session-view",
        value_enum,
        default_value_t = SessionListViewArg::Compact,
        help_heading = "Output",
        help = "Return full session prompt/final message or compact previews"
    )]
    pub(crate) session_view: SessionListViewArg,

    #[arg(
        long,
        value_enum,
        default_value_t = ViewArg::Narrative,
        help_heading = "Output",
        help = "Turn detail level. `narrative` omits tool arguments, outputs, and payload blobs"
    )]
    pub(crate) view: ViewArg,

    #[arg(
        long = "turn-limit",
        default_value_t = DEFAULT_SESSION_BUNDLE_TURN_LIMIT,
        help_heading = "Result Size",
        help = "Maximum turn details to return"
    )]
    pub(crate) turn_limit: usize,

    #[arg(
        long = "turn-offset",
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of turn details to skip"
    )]
    pub(crate) turn_offset: usize,

    #[arg(
        long = "step-limit",
        default_value_t = DEFAULT_TURN_STEP_LIMIT,
        help_heading = "Result Size",
        help = "Maximum steps to return per turn detail"
    )]
    pub(crate) step_limit: usize,

    #[arg(
        long = "step-offset",
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of steps to skip per turn detail"
    )]
    pub(crate) step_offset: usize,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Queries one turn detail payload.
#[derive(Debug, Args)]
pub(crate) struct QueryTurnArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    pub(crate) project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Disambiguate a session id or UUID prefix by provider"
    )]
    pub(crate) provider: Option<ProviderArg>,

    #[arg(
        long,
        conflicts_with = "scope",
        help_heading = "Scope",
        help = "Query an imported shared session"
    )]
    pub(crate) shared: bool,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Choose local, shared, or all sessions"
    )]
    pub(crate) scope: Option<SessionScopeArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Query this session id or unambiguous UUID prefix; required unless --session-id is set"
    )]
    pub(crate) session_id_arg: Option<String>,

    #[arg(
        value_name = "TURN_ORDINAL",
        help = "Query this turn ordinal; required unless --turn-ordinal is set"
    )]
    pub(crate) turn_ordinal_arg: Option<String>,

    #[arg(
        long = "session-id",
        value_name = "SESSION_ID",
        help_heading = "Identity",
        help = "Query this session id or unambiguous UUID prefix; alternative to positional SESSION_ID"
    )]
    pub(crate) session_id: Option<String>,

    #[arg(
        long = "turn-ordinal",
        value_name = "TURN_ORDINAL",
        help_heading = "Identity",
        help = "Query this turn ordinal; alternative to positional TURN_ORDINAL"
    )]
    pub(crate) turn_ordinal: Option<u64>,

    #[arg(
        long,
        value_enum,
        help_heading = "Output",
        help = "Step detail level. Defaults to narrative unless --include-raw is set; `narrative` omits tool arguments, outputs, and payload blobs"
    )]
    pub(crate) view: Option<ViewArg>,

    #[arg(
        long,
        help_heading = "Output",
        help = "Include optional raw/debug fields such as raw_steps_json"
    )]
    pub(crate) include_raw: bool,

    #[arg(
        long,
        help_heading = "Output",
        help = "Include one derived insights block with metrics plus tool and file analytics"
    )]
    pub(crate) include_insights: bool,

    #[arg(
        long = "step-limit",
        default_value_t = DEFAULT_TURN_STEP_LIMIT,
        help_heading = "Result Size",
        help = "Maximum steps to return"
    )]
    pub(crate) step_limit: usize,

    #[arg(
        long = "step-offset",
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of steps to skip"
    )]
    pub(crate) step_offset: usize,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Queries paginated turn search results for one configured project.
#[derive(Debug, Args)]
pub(crate) struct QuerySearchTurnsArgs {
    #[arg(
        long,
        value_enum,
        default_value_t = SearchModeArg::Keyword,
        help_heading = "Search",
        help = "Choose how QUERY is interpreted"
    )]
    pub(crate) mode: SearchModeArg,

    #[arg(
        value_name = "QUERY",
        help = "Search query text or path pattern",
        long_help = search_query_help()
    )]
    pub(crate) query_arg: Option<String>,

    #[arg(
        long,
        allow_hyphen_values = true,
        value_name = "QUERY",
        help_heading = "Search",
        help = "Pass QUERY by flag; use this when the value starts with '-'"
    )]
    pub(crate) query: Option<String>,

    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    pub(crate) project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict search to this provider"
    )]
    pub(crate) provider: Option<ProviderArg>,

    #[arg(
        long = "session-id",
        help_heading = "Scope",
        help = "Restrict search to this session id or unambiguous UUID prefix"
    )]
    pub(crate) session_id: Option<String>,

    #[arg(
        long,
        conflicts_with = "scope",
        help_heading = "Scope",
        help = "Search only imported shared sessions"
    )]
    pub(crate) shared: bool,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Choose local, shared, or all sessions"
    )]
    pub(crate) scope: Option<SessionScopeArg>,

    #[arg(
        long,
        help_heading = "Scope",
        help = "Restrict shared-session results to this author user id, email, or display name"
    )]
    pub(crate) author: Option<String>,

    #[arg(
        long,
        help_heading = "Evidence",
        help = "Include tool output evidence in literal and regex search"
    )]
    pub(crate) include_tool_output: bool,

    #[arg(
        long = "field",
        value_name = "FIELD",
        value_parser = parse_search_evidence_field,
        help_heading = "Evidence",
        help = search_evidence_field_include_help()
    )]
    pub(crate) fields: Vec<SearchEvidenceField>,

    #[arg(
        long = "exclude-field",
        value_name = "FIELD",
        value_parser = parse_search_evidence_field,
        help_heading = "Evidence",
        help = search_evidence_field_exclude_help()
    )]
    pub(crate) excluded_fields: Vec<SearchEvidenceField>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Inclusive started_at lower bound. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    pub(crate) since: Option<String>,

    #[arg(
        long,
        help_heading = "Time Filters",
        help = "Exclusive started_at upper bound. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    pub(crate) until: Option<String>,

    #[arg(
        long,
        default_value_t = DEFAULT_QUERY_PAGE_LIMIT,
        help_heading = "Result Size",
        help = "Maximum turn hits to return"
    )]
    pub(crate) limit: usize,

    #[arg(
        long,
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of turn hits to skip"
    )]
    pub(crate) offset: usize,

    #[arg(
        long = "matched-path-limit",
        default_value_t = DEFAULT_MATCHED_PATH_LIMIT,
        conflicts_with = "include_all_matched_paths",
        help_heading = "Result Size",
        help = "Maximum matched_paths entries per file-search hit"
    )]
    pub(crate) matched_path_limit: usize,

    #[arg(
        long = "match-limit",
        value_name = "MATCH_LIMIT",
        help_heading = "Result Size",
        help = search_match_limit_help()
    )]
    pub(crate) match_limit: Option<usize>,

    #[arg(
        long = "include-all-matched-paths",
        help_heading = "Result Size",
        help = "Return every matched path in file-search hits"
    )]
    pub(crate) include_all_matched_paths: bool,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Queries the workspace insights payload for one rolling day window.
#[derive(Debug, Args)]
pub(crate) struct QueryWorkspaceInsightsArgs {
    #[arg(
        long = "window",
        default_value = "7d",
        value_parser = parse_window_days,
        help_heading = "Time Window",
        help = "Rolling host-local day window in `<days>d` format"
    )]
    pub(crate) window_days: u32,

    #[arg(
        long = "recent-session-limit",
        default_value_t = DEFAULT_WORKSPACE_RECENT_SESSION_LIMIT,
        help_heading = "Result Size",
        help = "Maximum recent sessions to return"
    )]
    pub(crate) recent_session_limit: usize,

    #[arg(
        long = "recent-session-offset",
        default_value_t = 0,
        help_heading = "Result Size",
        help = "Number of recent sessions to skip"
    )]
    pub(crate) recent_session_offset: usize,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Queries the project insights payload for one configured project.
#[derive(Debug, Args)]
pub(crate) struct QueryProjectInsightsArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    pub(crate) project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Restrict project insights to this provider"
    )]
    pub(crate) provider: Option<ProviderArg>,

    #[arg(
        long = "turn-limit",
        alias = "limit",
        default_value_t = 1000,
        help_heading = "Result Size",
        help = "Maximum indexed turns to inspect"
    )]
    pub(crate) turn_limit: usize,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Queries one turn insights payload.
#[derive(Debug, Args)]
pub(crate) struct QueryTurnInsightsArgs {
    #[arg(
        long = "project-id",
        help_heading = "Scope",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    pub(crate) project_id: Option<String>,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Disambiguate a session id or UUID prefix by provider"
    )]
    pub(crate) provider: Option<ProviderArg>,

    #[arg(
        long,
        conflicts_with = "scope",
        help_heading = "Scope",
        help = "Query an imported shared session"
    )]
    pub(crate) shared: bool,

    #[arg(
        long,
        value_enum,
        help_heading = "Scope",
        help = "Choose local, shared, or all sessions"
    )]
    pub(crate) scope: Option<SessionScopeArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Query this session id or unambiguous UUID prefix; required unless --session-id is set"
    )]
    pub(crate) session_id_arg: Option<String>,

    #[arg(
        value_name = "TURN_ORDINAL",
        help = "Query this turn ordinal; required unless --turn-ordinal is set"
    )]
    pub(crate) turn_ordinal_arg: Option<String>,

    #[arg(
        long = "session-id",
        value_name = "SESSION_ID",
        help_heading = "Identity",
        help = "Query this session id or unambiguous UUID prefix; alternative to positional SESSION_ID"
    )]
    pub(crate) session_id: Option<String>,

    #[arg(
        long = "turn-ordinal",
        value_name = "TURN_ORDINAL",
        help_heading = "Identity",
        help = "Query this turn ordinal; alternative to positional TURN_ORDINAL"
    )]
    pub(crate) turn_ordinal: Option<u64>,

    #[arg(
        long,
        default_value_os_t = default_root_path(),
        help_heading = "Workspace",
        help = "Read from this darc root"
    )]
    pub(crate) root: PathBuf,
}

/// Audit Codex rollout schema compatibility against stable release tags.
#[derive(Debug, Args)]
pub(crate) struct CodexSchemaAuditArgs {
    #[arg(long, value_name = "DIR", help_heading = "Cache")]
    pub(crate) cache_dir: Option<PathBuf>,
}

/// Audit Claude rollout transcript compatibility against published npm releases.
#[derive(Debug, Args)]
pub(crate) struct ClaudeSchemaAuditArgs {
    #[arg(long, value_name = "DIR", help_heading = "Cache")]
    pub(crate) cache_dir: Option<PathBuf>,

    #[arg(long, default_value_t = 1, value_name = "N", help_heading = "Sampling")]
    pub(crate) sample_stride: usize,

    #[arg(long, help_heading = "Runtime")]
    pub(crate) use_host_auth: bool,

    #[arg(long, value_name = "VERSION", help_heading = "Scope")]
    pub(crate) from_version: Option<String>,

    #[arg(
        long,
        value_enum,
        default_value_t = ClaudeSurveyModeArg::Refine,
        help_heading = "Mode"
    )]
    pub(crate) survey_mode: ClaudeSurveyModeArg,
}
