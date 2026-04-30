#[cfg(test)]
mod tests;

use std::{
    env,
    ffi::OsString,
    io::{self, IsTerminal},
    path::PathBuf,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum, error::ErrorKind};
use darc_core::query::{
    DEFAULT_MATCHED_PATH_LIMIT, DEFAULT_RESOLVE_SESSION_MATCH_LIMIT, DEFAULT_SEARCH_MATCH_LIMIT,
    DEFAULT_TURN_STEP_LIMIT, DEFAULT_WORKSPACE_RECENT_SESSION_LIMIT, FilesQueryRequest,
    QueryProtocolError, ResolveSessionQueryRequest, ResolvedQueryProject, ResolvedSessionMatch,
    SearchEvidenceField, SearchMode, SearchSnippetMatcher, SearchTurnsQueryData,
    SearchTurnsRequest, SessionBundleQueryRequest, SessionBundleView, SessionsQueryRequest,
    SessionsView, TurnDetailOptions, TurnsQueryRequest, TurnsView, query_files_for_project,
    query_project_insight_report_for_project, query_resolve_sessions,
    query_search_turns_for_project, query_session_bundle_for_project,
    query_session_files_for_project, query_sessions_for_project, query_turn_for_project,
    query_turn_insight_report_for_project, query_turns_for_project, query_workspace,
    query_workspace_insight_report, resolve_query_project,
    resolve_query_search_session_id_for_project, resolve_query_session_for_project,
};
use darc_core::{
    IndexOptions, IndexReport, InitDraft, RefreshAllBestEffortReport, RefreshOptions,
    RefreshProjectAttempt, RefreshProjectFailure, RefreshReport, SkippedRollout, SourceKind,
    StatusProject, StatusSource, StatusSyncCheck, StatusSyncPlan, SyncOptions, SyncReport,
    WorkspaceStatusReport, default_root_path, execute_sync, index_project_sessions, link_project,
    prepare_init, prepare_sync, refresh_all_projects_best_effort, refresh_project, remove_project,
    rename_project, status_project, status_workspace, write_init,
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
use serde::Serialize;
use serde_json::{Value as JsonValue, json};

#[derive(Debug, Parser)]
#[command(name = "darc", version, about = "Darc CLI")]
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
        about = "Link one configured project's historical paths into the current project",
        long_about = "Link one configured project's historical paths into the current project.\n\nRun this command from the target project directory.\nThe PROJECT argument is the old or source project name already stored in ~/.darc/config.toml.\n\nExample:\n- You renamed `/path/to/old-project` to `/path/to/new-project`.\n- Darc still has a configured project named `old-project`.\n- Run `cd /path/to/new-project && darc link old-project`.\n\nThis command is non-destructive.\nIt updates config so the current project knows the source project's old local_path and known_paths.\nIt does not run `darc refresh` or remove the source project."
    )]
    Link(LinkArgs),
    #[command(
        about = "Remove one configured project and its archived/indexed data",
        long_about = "Remove one configured project and its archived/indexed data.\n\nThe PROJECT argument is matched against the configured project `name` in ~/.darc/config.toml.\nThe name must identify exactly one configured project.\n\nThis command deletes:\n- the project entry from config.toml\n- the project's archived sessions directory under ~/.darc/projects/...\n- the project's indexed SQLite rows\n\nYou can run this command from any directory."
    )]
    Remove(RemoveArgs),
    #[command(
        name = "rename-from",
        about = "Rebuild one old project's history into the current renamed project",
        long_about = "Rebuild one old project's history into the current renamed project.\n\nUse this when you just renamed a project from one name to another.\nRun the command from the new project directory, and pass the old project name.\n\nExample:\n- Darc config still contains a project named `old-project`.\n- You renamed the checkout to `/path/to/new-project`.\n- Run `cd /path/to/new-project && darc rename-from old-project`.\n\nThis command bootstraps or reuses the current project as the target, links the old project's paths into it, runs `darc refresh`, and removes the old source project after those steps succeed.\n\nIn other words, it is the safe built-in workflow for:\n`darc link <old-project> && darc refresh && darc remove <old-project>`\n\nIf ~/.darc/config.toml does not exist yet, run `darc init` first."
    )]
    RenameFrom(RenameArgs),
    /// Sync matching Claude and Codex sessions into the project archive.
    Sync(SyncArgs),
    /// Index archived sessions from selected providers for the active project into SQLite.
    Index(IndexArgs),
    /// Query darc state through the machine-readable read protocol.
    Query(Box<QueryArgs>),
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
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(long)]
    dry_run: bool,
}

/// Syncs then indexes archived sessions for one or all projects.
#[derive(Debug, Args)]
struct RefreshArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(
        long = "provider",
        value_enum,
        help = "Limit both sync and index to the selected providers"
    )]
    provider: Vec<ProviderArg>,

    #[arg(
        long,
        help = "Refresh every registered project, continue past per-project failures, and summarize the results"
    )]
    all: bool,
}

/// Shows Darc status for the active project or workspace.
#[derive(Debug, Args)]
struct StatusArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(
        long,
        help = "Show status for the shared Darc workspace instead of the active project"
    )]
    workspace: bool,

    #[arg(
        long,
        help = "Run sync planning without writing manifests, config, archives, or SQLite"
    )]
    check: bool,
}

/// Sync matching Claude and Codex sessions into the project archive.
#[derive(Debug, Args)]
struct SyncArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(long)]
    dry_run: bool,

    #[arg(long = "provider", value_enum)]
    provider: Vec<ProviderArg>,
}

/// Link one configured project's historical paths into the active project.
#[derive(Debug, Args)]
struct LinkArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(value_name = "PROJECT")]
    project: String,
}

/// Remove one configured project and its archived/indexed data.
#[derive(Debug, Args)]
struct RemoveArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(value_name = "PROJECT")]
    project: String,
}

/// Rebuild one configured project's history under the active project's id, then remove the old project.
#[derive(Debug, Args)]
struct RenameArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(value_name = "PROJECT")]
    project: String,
}

/// Index archived sessions from selected providers for the active project into SQLite.
#[derive(Debug, Args)]
struct IndexArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(long = "provider", value_enum)]
    provider: Vec<ProviderArg>,
}

/// Queries darc state through the machine-readable read protocol.
#[derive(Debug, Args)]
struct QueryArgs {
    #[arg(
        long,
        value_enum,
        default_value_t = ColorArg::Auto,
        help = "Control ANSI color in query JSON output"
    )]
    color: ColorArg,

    #[command(subcommand)]
    command: QueryCommands,
}

/// Represents the supported machine-readable query commands.
#[derive(Debug, Subcommand)]
enum QueryCommands {
    /// Queries the workspace/sidebar payload for one darc root.
    Workspace(QueryWorkspaceArgs),
    /// Resolves one full session id or UUID prefix into canonical matches.
    ResolveSession(QueryResolveSessionArgs),
    /// Queries the session list for one configured project.
    Sessions(QuerySessionsArgs),
    /// Lists most-touched files or pivots from one file selector.
    #[command(
        about = "List most-touched files or pivot from one file selector",
        long_about = "List most-touched files or pivot from one file selector.\n\nWith no PATH, --path, or --co-touched-with, this ranks files by touches across the project.\nPass PATH or --path to return sessions that touched matching paths.\nPass --co-touched-with to return files touched in the same sessions as the seed path."
    )]
    Files(QueryFilesArgs),
    /// Queries per-file access summaries for one session.
    SessionFiles(QuerySessionFilesArgs),
    /// Queries one composite session bundle for one session.
    SessionBundle(QuerySessionBundleArgs),
    /// Queries the turn list for one session.
    Turns(QueryTurnsArgs),
    /// Queries one turn detail payload.
    Turn(QueryTurnArgs),
    /// Queries one paginated search payload.
    Search(QuerySearchArgs),
    /// Queries one insights payload.
    Insights(QueryInsightsArgs),
}

/// Queries the workspace/sidebar payload for one darc root.
#[derive(Debug, Args)]
struct QueryWorkspaceArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,
}

/// Resolves one full session id or UUID prefix into canonical project/provider/session matches.
#[derive(Debug, Args)]
struct QueryResolveSessionArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(help = "Resolve this full UUID or UUID prefix")]
    input: String,

    #[arg(
        long = "project-id",
        help = "Restrict matches to this configured project id"
    )]
    project_id: Option<String>,

    #[arg(long, value_enum, help = "Restrict matches to this provider")]
    provider: Option<ProviderArg>,

    #[arg(
        long,
        help = "Require exactly one match and return it as one convenience object"
    )]
    pick_one: bool,
}

/// Queries the session list for one configured project.
#[derive(Debug, Args)]
struct QuerySessionsArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(
        long = "project-id",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(long, value_enum, help = "Restrict sessions to this provider")]
    provider: Option<ProviderArg>,

    #[arg(
        long,
        value_enum,
        default_value_t = SessionListViewArg::Compact,
        help = "Return full session prompts and final messages or compact previews"
    )]
    view: SessionListViewArg,

    #[arg(
        long,
        help = "Inclusive latest_turn_at lower bound. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    since: Option<String>,

    #[arg(
        long,
        help = "Exclusive latest_turn_at upper bound. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    until: Option<String>,

    #[arg(
        long = "touched-path",
        help = "Only keep sessions that touched a file path matching this glob"
    )]
    touched_path: Option<String>,

    #[arg(long, default_value_t = 50, help = "Maximum sessions to return")]
    limit: usize,

    #[arg(long, default_value_t = 0, help = "Number of sessions to skip")]
    offset: usize,
}

/// Queries the turn list for one session.
#[derive(Debug, Args)]
struct QueryTurnsArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(
        long = "project-id",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(long, value_enum, help = "Disambiguate a cross-provider session id")]
    provider: Option<ProviderArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Full session id to list turns for; required unless --session-id is set"
    )]
    session_id_arg: Option<String>,

    #[arg(
        long = "session-id",
        value_name = "SESSION_ID",
        help = "Full session id to list turns for; alternative to positional SESSION_ID"
    )]
    session_id: Option<String>,

    #[arg(
        long,
        value_enum,
        default_value_t = TurnListViewArg::Full,
        help = "Return full turn summaries or a compact one-line skim"
    )]
    view: TurnListViewArg,

    #[arg(
        long,
        help = "Inclusive started_at lower bound. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    since: Option<String>,

    #[arg(
        long,
        help = "Exclusive started_at upper bound. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    until: Option<String>,

    #[arg(long, default_value_t = 50, help = "Maximum turns to return")]
    limit: usize,

    #[arg(long, default_value_t = 0, help = "Number of turns to skip")]
    offset: usize,
}

/// Lists most-touched files or pivots from one file selector.
#[derive(Debug, Args)]
struct QueryFilesArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(
        long = "project-id",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(long, value_enum, help = "Restrict file pivots to this provider")]
    provider: Option<ProviderArg>,

    #[arg(
        long,
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
        help = "Return files touched in the same sessions as this seed path instead of most-touched files"
    )]
    co_touched_with: Option<String>,

    #[arg(
        long,
        help = "Inclusive started_at lower bound for file pivots. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    since: Option<String>,

    #[arg(
        long,
        help = "Exclusive started_at upper bound for file pivots. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    until: Option<String>,

    #[arg(long, default_value_t = 50, help = "Maximum rows to return")]
    limit: usize,

    #[arg(long, default_value_t = 0, help = "Number of rows to skip")]
    offset: usize,

    #[arg(
        long = "matched-path-limit",
        default_value_t = DEFAULT_MATCHED_PATH_LIMIT,
        conflicts_with = "include_all_matched_paths",
        help = "Maximum matched_paths entries per path-mode row"
    )]
    matched_path_limit: usize,

    #[arg(
        long = "include-all-matched-paths",
        help = "Return every matched path in path-mode rows"
    )]
    include_all_matched_paths: bool,
}

/// Queries one session-scoped per-file access summary payload.
#[derive(Debug, Args)]
struct QuerySessionFilesArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(
        long = "project-id",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(long, value_enum, help = "Disambiguate a cross-provider session id")]
    provider: Option<ProviderArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Query this session id; required unless --session-id is set"
    )]
    session_id_arg: Option<String>,

    #[arg(
        long = "session-id",
        value_name = "SESSION_ID",
        help = "Query this session id; alternative to positional SESSION_ID"
    )]
    session_id: Option<String>,
}

/// Queries one composite session bundle payload.
#[derive(Debug, Args)]
struct QuerySessionBundleArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(
        long = "project-id",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(long, value_enum, help = "Disambiguate a cross-provider session id")]
    provider: Option<ProviderArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Query this session id; required unless --session-id is set"
    )]
    session_id_arg: Option<String>,

    #[arg(
        long = "session-id",
        value_name = "SESSION_ID",
        help = "Query this session id; alternative to positional SESSION_ID"
    )]
    session_id: Option<String>,

    #[arg(
        long = "session-view",
        value_enum,
        default_value_t = SessionListViewArg::Compact,
        help = "Return full session prompt/final message or compact previews"
    )]
    session_view: SessionListViewArg,

    #[arg(
        long,
        value_enum,
        default_value_t = ViewArg::Narrative,
        help = "Turn detail level. `narrative` omits tool arguments, outputs, and payload blobs"
    )]
    view: ViewArg,

    #[arg(
        long = "turn-limit",
        default_value_t = 50,
        help = "Maximum turn details to return"
    )]
    turn_limit: usize,

    #[arg(
        long = "turn-offset",
        default_value_t = 0,
        help = "Number of turn details to skip"
    )]
    turn_offset: usize,

    #[arg(
        long = "step-limit",
        default_value_t = DEFAULT_TURN_STEP_LIMIT,
        help = "Maximum steps to return per turn detail"
    )]
    step_limit: usize,

    #[arg(
        long = "step-offset",
        default_value_t = 0,
        help = "Number of steps to skip per turn detail"
    )]
    step_offset: usize,
}

/// Queries one turn detail payload.
#[derive(Debug, Args)]
struct QueryTurnArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(
        long = "project-id",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(long, value_enum, help = "Disambiguate a cross-provider session id")]
    provider: Option<ProviderArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Query this session id; required unless --session-id is set"
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
        help = "Query this session id; alternative to positional SESSION_ID"
    )]
    session_id: Option<String>,

    #[arg(
        long = "turn-ordinal",
        value_name = "TURN_ORDINAL",
        help = "Query this turn ordinal; alternative to positional TURN_ORDINAL"
    )]
    turn_ordinal: Option<u64>,

    #[arg(
        long,
        value_enum,
        help = "Step detail level. Defaults to narrative unless --include-raw is set; `narrative` omits tool arguments, outputs, and payload blobs"
    )]
    view: Option<ViewArg>,

    #[arg(
        long,
        help = "Include optional raw/debug fields such as raw_steps_json"
    )]
    include_raw: bool,

    #[arg(
        long,
        help = "Include one derived insights block with metrics plus tool and file analytics"
    )]
    include_insights: bool,

    #[arg(
        long = "step-limit",
        default_value_t = DEFAULT_TURN_STEP_LIMIT,
        help = "Maximum steps to return"
    )]
    step_limit: usize,

    #[arg(
        long = "step-offset",
        default_value_t = 0,
        help = "Number of steps to skip"
    )]
    step_offset: usize,
}

/// Queries one search payload.
#[derive(Debug, Args)]
struct QuerySearchArgs {
    #[command(subcommand)]
    command: QuerySearchCommands,
}

/// Represents the supported machine-readable search query commands.
#[derive(Debug, Subcommand)]
enum QuerySearchCommands {
    /// Queries paginated turn search results for one configured project.
    Turns(QuerySearchTurnsArgs),
}

/// Queries paginated turn search results for one configured project.
#[derive(Debug, Args)]
struct QuerySearchTurnsArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(
        long = "project-id",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(long, value_enum, help = "Restrict search to this provider")]
    provider: Option<ProviderArg>,

    #[arg(long = "session-id", help = "Restrict search to this session id")]
    session_id: Option<String>,

    #[arg(
        long,
        value_enum,
        default_value_t = SearchModeArg::Keyword,
        help = "Search in this mode"
    )]
    mode: SearchModeArg,

    #[arg(
        value_name = "QUERY",
        help = "Search for this text, file glob, or path fragment"
    )]
    query_arg: Option<String>,

    #[arg(
        long,
        allow_hyphen_values = true,
        value_name = "QUERY",
        help = "Search for this text, file glob, or path fragment"
    )]
    query: Option<String>,

    #[arg(
        long,
        help = "Include tool output evidence in literal and regex search"
    )]
    include_tool_output: bool,

    #[arg(
        long = "field",
        value_name = "FIELD",
        value_parser = parse_search_evidence_field,
        help = search_evidence_field_include_help()
    )]
    fields: Vec<SearchEvidenceField>,

    #[arg(
        long = "exclude-field",
        value_name = "FIELD",
        value_parser = parse_search_evidence_field,
        help = search_evidence_field_exclude_help()
    )]
    excluded_fields: Vec<SearchEvidenceField>,

    #[arg(
        long,
        help = "Inclusive started_at lower bound. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    since: Option<String>,

    #[arg(
        long,
        help = "Exclusive started_at upper bound. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    until: Option<String>,

    #[arg(long, default_value_t = 50, help = "Maximum turn hits to return")]
    limit: usize,

    #[arg(long, default_value_t = 0, help = "Number of turn hits to skip")]
    offset: usize,

    #[arg(
        long = "matched-path-limit",
        default_value_t = DEFAULT_MATCHED_PATH_LIMIT,
        conflicts_with = "include_all_matched_paths",
        help = "Maximum matched_paths entries per file-search hit"
    )]
    matched_path_limit: usize,

    #[arg(
        long = "match-limit",
        value_name = "MATCH_LIMIT",
        help = search_match_limit_help()
    )]
    match_limit: Option<usize>,

    #[arg(
        long = "include-all-matched-paths",
        help = "Return every matched path in file-search hits"
    )]
    include_all_matched_paths: bool,
}

/// Queries one workspace or project insights payload.
#[derive(Debug, Args)]
struct QueryInsightsArgs {
    #[command(subcommand)]
    command: QueryInsightsCommands,
}

/// Represents the supported machine-readable insights query commands.
#[derive(Debug, Subcommand)]
enum QueryInsightsCommands {
    /// Queries the workspace insights payload for one rolling day window.
    Workspace(QueryWorkspaceInsightsArgs),
    /// Queries the project insights payload for one configured project.
    Project(QueryProjectInsightsArgs),
    /// Queries the turn insights payload for one session turn.
    Turn(QueryTurnInsightsArgs),
}

/// Queries the workspace insights payload for one rolling day window.
#[derive(Debug, Args)]
struct QueryWorkspaceInsightsArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(
        long = "window",
        default_value = "7d",
        value_parser = parse_window_days,
        help = "Rolling host-local day window in `<days>d` format"
    )]
    window_days: u32,

    #[arg(
        long = "recent-session-limit",
        default_value_t = DEFAULT_WORKSPACE_RECENT_SESSION_LIMIT,
        help = "Maximum recent sessions to return"
    )]
    recent_session_limit: usize,

    #[arg(
        long = "recent-session-offset",
        default_value_t = 0,
        help = "Number of recent sessions to skip"
    )]
    recent_session_offset: usize,
}

/// Queries the project insights payload for one configured project.
#[derive(Debug, Args)]
struct QueryProjectInsightsArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(
        long = "project-id",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(long, value_enum, help = "Restrict project insights to this provider")]
    provider: Option<ProviderArg>,

    #[arg(
        long = "turn-limit",
        alias = "limit",
        default_value_t = 1000,
        help = "Maximum indexed turns to inspect"
    )]
    turn_limit: usize,
}

/// Queries one turn insights payload.
#[derive(Debug, Args)]
struct QueryTurnInsightsArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(
        long = "project-id",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(long, value_enum, help = "Disambiguate a cross-provider session id")]
    provider: Option<ProviderArg>,

    #[arg(
        value_name = "SESSION_ID",
        help = "Query this session id; required unless --session-id is set"
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
        help = "Query this session id; alternative to positional SESSION_ID"
    )]
    session_id: Option<String>,

    #[arg(
        long = "turn-ordinal",
        value_name = "TURN_ORDINAL",
        help = "Query this turn ordinal; alternative to positional TURN_ORDINAL"
    )]
    turn_ordinal: Option<u64>,
}

/// Audit Codex rollout schema compatibility against stable release tags.
#[derive(Debug, Args)]
struct CodexSchemaAuditArgs {
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,
}

/// Audit Claude rollout transcript compatibility against published npm releases.
#[derive(Debug, Args)]
struct ClaudeSchemaAuditArgs {
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,

    #[arg(long, default_value_t = 1, value_name = "N")]
    sample_stride: usize,

    #[arg(long)]
    use_host_auth: bool,

    #[arg(long, value_name = "VERSION")]
    from_version: Option<String>,

    #[arg(long, value_enum, default_value_t = ClaudeSurveyModeArg::Refine)]
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
    match Cli::try_parse_from(args.clone()) {
        Ok(cli) => run_cli(cli),
        Err(error) => clap_error_exit(error, &args),
    }
}

/// Dispatches one already parsed CLI command.
fn run_cli(cli: Cli) -> i32 {
    match cli.command {
        Commands::Init(args) => standard_exit(run_init(args)),
        Commands::Refresh(args) => standard_exit(run_refresh(args)),
        Commands::Status(args) => standard_exit(run_status(args)),
        Commands::Link(args) => standard_exit(run_link(args)),
        Commands::Remove(args) => standard_exit(run_remove(args)),
        Commands::RenameFrom(args) => standard_exit(run_rename_from(args)),
        Commands::Sync(args) => standard_exit(run_sync(args)),
        Commands::Index(args) => standard_exit(run_index(args)),
        Commands::Query(args) => query_exit(run_query(*args)),
        Commands::CodexSchemaAudit(args) => run_codex_schema_audit_command(args),
        Commands::ClaudeSchemaAudit(args) => run_claude_schema_audit_command(args),
    }
}

/// Maps Clap parse errors to the correct command-family output format.
fn clap_error_exit(error: clap::Error, args: &[OsString]) -> i32 {
    if is_query_invocation(args) && !is_clap_display_request(error.kind()) {
        eprintln!("{}", format_query_clap_error(&error));
        return error.exit_code();
    }

    if let Err(print_error) = error.print() {
        eprintln!("error: failed to write CLI error: {print_error}");
        return 1;
    }
    error.exit_code()
}

/// Returns whether the raw CLI arguments target the query protocol surface.
fn is_query_invocation(args: &[OsString]) -> bool {
    args.get(1).and_then(|arg| arg.to_str()) == Some("query")
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

/// Maps query command results to JSON-only machine-readable output.
fn query_exit(result: Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            let message = format_query_error(&error);
            eprintln!("{message}");
            1
        }
    }
}

/// Dispatches the supported machine-readable query commands.
fn run_query(args: QueryArgs) -> Result<()> {
    let output = QueryOutput::new(args.color);
    match args.command {
        QueryCommands::Workspace(args) => run_query_workspace(&output, args),
        QueryCommands::ResolveSession(args) => run_query_resolve_session(&output, args),
        QueryCommands::Sessions(args) => run_query_sessions(&output, args),
        QueryCommands::Files(args) => run_query_files(&output, args),
        QueryCommands::SessionFiles(args) => run_query_session_files(&output, args),
        QueryCommands::SessionBundle(args) => run_query_session_bundle(&output, args),
        QueryCommands::Turns(args) => run_query_turns(&output, args),
        QueryCommands::Turn(args) => run_query_turn(&output, args),
        QueryCommands::Search(args) => run_query_search(&output, args),
        QueryCommands::Insights(args) => run_query_insights(&output, args),
    }
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
        bail!("query files accepts either PATH/--path or --co-touched-with, not both");
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
    let data = query_session_files_for_project(&project, session.provider, &session.session_id)?;
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

/// Dispatches the supported machine-readable search query commands.
fn run_query_search(output: &QueryOutput, args: QuerySearchArgs) -> Result<()> {
    match args.command {
        QuerySearchCommands::Turns(args) => run_query_search_turns(output, args),
    }
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

/// Dispatches the supported machine-readable insights query commands.
fn run_query_insights(output: &QueryOutput, args: QueryInsightsArgs) -> Result<()> {
    match args.command {
        QueryInsightsCommands::Workspace(args) => run_query_workspace_insights(output, args),
        QueryInsightsCommands::Project(args) => run_query_project_insights(output, args),
        QueryInsightsCommands::Turn(args) => run_query_turn_insights(output, args),
    }
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
const ANSI_KEY: &str = "\x1b[1;34m";
const ANSI_STRING: &str = "\x1b[32m";
const ANSI_NUMBER: &str = "\x1b[33m";
const ANSI_BOOLEAN: &str = "\x1b[35m";
const ANSI_NULL: &str = "\x1b[36m";
const ANSI_MATCH: &str = "\x1b[1;95m";
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
            enabled: should_style_human_output(is_terminal, no_color, term),
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

/// Returns whether a human-oriented output stream should include ANSI styling.
fn should_style_human_output(is_terminal: bool, no_color: bool, term: Option<&str>) -> bool {
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
        ColorArg::Auto => stdout_is_terminal && !no_color && term != Some("dumb"),
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

/// Adds ANSI match highlighting to nested search-match snippet strings.
fn color_search_turns_json(json: &str, data: &SearchTurnsQueryData) -> Result<String> {
    let mut colored = color_json(json);
    if !matches!(data.mode, SearchMode::Literal | SearchMode::Regex) {
        return Ok(colored);
    }

    let mut cursor = 0;
    let matcher = SearchSnippetMatcher::new(data.mode, &data.query)?;
    for hit in &data.hits {
        for matched in &hit.matches {
            let Some(range) = matcher.find(&matched.snippet) else {
                continue;
            };
            if range.is_empty() {
                continue;
            }
            let Some((value_start, token_len)) =
                find_colored_snippet_value(&colored, &matched.snippet, cursor)
            else {
                continue;
            };
            let highlighted = color_json_string_with_match(&matched.snippet, range);
            colored.replace_range(value_start..value_start + token_len, &highlighted);
            cursor = value_start + highlighted.len();
        }
    }
    Ok(colored)
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
    let payload = QueryErrorEnvelope {
        schema: "darc.error.v1",
        generated_at: current_utc_timestamp(),
        darc_version: env!("CARGO_PKG_VERSION"),
        error: QueryErrorData {
            message: error.to_string(),
            code: structured.map(QueryProtocolError::code),
            details: structured.map(QueryProtocolError::details),
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
        (Some(_), Some(_)) => {
            bail!("pass {value_label} either as {positional_name} or {flag_name}, not both")
        }
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
        anyhow!("query command requires {value_label} as {positional_name} or {flag_name}")
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
        (Some(_), Some(_), Some(_), _) | (Some(_), Some(_), None, Some(_)) => bail!(
            "pass turn identity either as SESSION_ID TURN_ORDINAL or with --session-id/--turn-ordinal, not both"
        ),
        (Some(_), None, None, None) => {
            bail!("query command requires turn ordinal as TURN_ORDINAL or --turn-ordinal")
        }
        (None, Some(_), None, None) => {
            bail!("query command requires session id as SESSION_ID or --session-id")
        }
        (None, None, None, None) => bail!(
            "query command requires session id and turn ordinal as SESSION_ID TURN_ORDINAL or --session-id/--turn-ordinal"
        ),
        _ => bail!("unexpected extra positional turn identity arguments"),
    }
}

/// Parses one turn ordinal positional value.
fn parse_turn_ordinal_arg(value: &str) -> Result<u64> {
    value
        .parse()
        .with_context(|| format!("invalid turn ordinal `{value}`"))
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
    format!(
        "Restrict literal and regex search to this evidence field. Accepted fields: {}",
        supported_search_evidence_fields()
    )
}

/// Returns help text for exact-search field exclusion.
fn search_evidence_field_exclude_help() -> String {
    format!(
        "Exclude this evidence field from literal and regex search. Accepted fields: {}",
        supported_search_evidence_fields()
    )
}

/// Returns help text for the literal/regex per-hit match preview cap.
fn search_match_limit_help() -> String {
    format!(
        "Maximum nested matches per literal/regex turn hit [default: {DEFAULT_SEARCH_MATCH_LIMIT}]"
    )
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

/// Stores one compact row for session-scoped `darc query turns --view oneline`.
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

/// Stores the `--pick-one` success payload for `darc query resolve-session`.
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
    let report = link_project(Some(args.root), &args.project)?;
    let style = HumanStyle::stdout();

    print_section(style, "Link");
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
    println!();
    print_section(style, "Status");
    if report.config_written {
        print_field(style, 2, "Config", style.ok("updated"));
    } else {
        print_field(style, 2, "Config", style.ok("already covered linked paths"));
    }

    Ok(())
}

/// Removes one configured project and its archived/indexed data.
fn run_remove(args: RemoveArgs) -> Result<()> {
    let report = remove_project(Some(args.root), &args.project)?;
    let style = HumanStyle::stdout();

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
    let report = rename_project(Some(args.root), &args.project)?;
    let style = HumanStyle::stdout();

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

/// Runs the daily refresh workflow for one or all projects.
fn run_refresh(args: RefreshArgs) -> Result<()> {
    let options = RefreshOptions {
        provider_filter: args.provider.into_iter().map(ProviderArg::into).collect(),
    };

    if args.all {
        let report = refresh_all_projects_best_effort(Some(args.root), options)?;
        print_refresh_all_report(&report);
        return refresh_all_exit_status(&report);
    }

    let report = refresh_project(Some(args.root), options)
        .map_err(add_init_hint_for_unconfigured_project)?;
    print_refresh_report(&report);
    Ok(())
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
        print_workspace_status(&report);
        return status_check_exit(report.has_failed_check(), "workspace status check failed");
    }

    let report = status_project(Some(args.root), args.check)
        .map_err(add_init_hint_for_unconfigured_project)?;
    print_project_status(&report);
    status_check_exit(report.has_failed_check(), "status check failed")
}

/// Converts an optional status sync-check failure into the final CLI exit result.
fn status_check_exit(has_failed_check: bool, message: &'static str) -> Result<()> {
    if has_failed_check {
        bail!("{message}");
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
    print_section(style, "Index");
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
