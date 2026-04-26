#[cfg(test)]
mod tests;

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use darc_core::query::{
    DEFAULT_RESOLVE_SESSION_MATCH_LIMIT, FilesQueryRequest, QueryProtocolError,
    ResolveSessionQueryRequest, ResolvedQueryProject, ResolvedSessionMatch, SearchMode,
    SearchTurnsRequest, SessionBundleView, TurnDetailOptions, TurnMatchesQueryRequest,
    TurnSearchRole, TurnsQueryRequest, TurnsView, query_files_for_project,
    query_project_insight_report_for_project, query_resolve_sessions,
    query_search_turns_for_project, query_session_bundle_for_project,
    query_session_files_for_project, query_sessions_for_project, query_turn_for_project,
    query_turn_insight_report_for_project, query_turn_matches_for_project, query_turns_for_project,
    query_workspace, query_workspace_insight_report, resolve_query_project,
    resolve_query_session_id_for_project,
};
use darc_core::{
    IndexOptions, InitDraft, RefreshAllBestEffortReport, RefreshOptions, RefreshProjectAttempt,
    RefreshProjectFailure, RefreshReport, SkippedRollout, SourceKind, SyncOptions,
    default_root_path, execute_sync, index_project_sessions, link_project, prepare_init,
    prepare_sync, refresh_all_projects_best_effort, refresh_project, remove_project,
    rename_project, write_init,
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
use serde_json::Value as JsonValue;

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
    Query(QueryArgs),
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
    /// Queries file pivots for one configured project.
    Files(QueryFilesArgs),
    /// Queries per-file access summaries for one provider session.
    SessionFiles(QuerySessionFilesArgs),
    /// Queries one composite session bundle for one provider session.
    SessionBundle(QuerySessionBundleArgs),
    /// Queries the turn list for one provider session or grep request.
    Turns(QueryTurnsArgs),
    /// Queries one full turn detail payload.
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

    #[arg(
        long,
        required = true,
        help = "Required. Emit the stable machine-readable JSON envelope on stdout"
    )]
    json: bool,
}

/// Resolves one full session id or UUID prefix into canonical provider/session matches.
#[derive(Debug, Args)]
struct QueryResolveSessionArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(help = "Resolve this full UUID or UUID prefix")]
    input: String,

    #[arg(long, value_enum, help = "Restrict matches to this provider")]
    provider: Option<ProviderArg>,

    #[arg(
        long,
        help = "Require exactly one match and return it as one convenience object"
    )]
    pick_one: bool,

    #[arg(
        long,
        required = true,
        help = "Required. Emit the stable machine-readable JSON envelope on stdout"
    )]
    json: bool,
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

    #[arg(
        long,
        required = true,
        help = "Required. Emit the stable machine-readable JSON envelope on stdout"
    )]
    json: bool,
}

/// Queries the turn list for one provider session or grep request.
#[derive(Debug, Args)]
struct QueryTurnsArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(
        long = "project-id",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(long, value_enum, help = "Restrict turns to this provider")]
    provider: Option<ProviderArg>,

    #[arg(long = "session-id", help = "Restrict turns to this session id")]
    session_id: Option<String>,

    #[arg(long, help = "Search turn text for this free-form query")]
    grep: Option<String>,

    #[arg(
        long,
        value_enum,
        default_value_t = TurnSearchRoleArg::Both,
        help = "Restrict grep matches to user prompts, assistant text, or both"
    )]
    role: TurnSearchRoleArg,

    #[arg(
        long,
        default_value_t = 0,
        help = "Include this many surrounding turns before and after each grep match"
    )]
    context: usize,

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

    #[arg(
        long,
        value_enum,
        default_value_t = TurnListViewArg::Full,
        help = "Return full turn summaries or a compact one-line skim"
    )]
    view: TurnListViewArg,

    #[arg(
        long = "touched-path",
        help = "Only keep grep matches from turns that touched a file path matching this glob"
    )]
    touched_path: Option<String>,

    #[arg(
        long,
        required = true,
        help = "Required. Emit the stable machine-readable JSON envelope on stdout"
    )]
    json: bool,
}

/// Queries file pivots for one configured project.
#[derive(Debug, Args)]
struct QueryFilesArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(
        long = "project-id",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(
        long,
        help = "Return sessions that touched file paths matching this glob"
    )]
    path: Option<String>,

    #[arg(
        long = "co-touched-with",
        help = "Return files that were touched in the same sessions as this seed path"
    )]
    co_touched_with: Option<String>,

    #[arg(
        long,
        help = "Inclusive started_at lower bound for --path mode. Example: `5d` or `2026-04-07T00:00:00Z`"
    )]
    since: Option<String>,

    #[arg(
        long,
        help = "Exclusive started_at upper bound for --path mode. Example: `1d` or `2026-04-08T00:00:00Z`"
    )]
    until: Option<String>,

    #[arg(
        long,
        help = "Maximum co-touched files to return for --co-touched-with mode"
    )]
    limit: Option<usize>,

    #[arg(
        long,
        required = true,
        help = "Required. Emit the stable machine-readable JSON envelope on stdout"
    )]
    json: bool,
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

    #[arg(long, value_enum, help = "Query this provider session")]
    provider: ProviderArg,

    #[arg(long = "session-id", help = "Query this session id")]
    session_id: String,

    #[arg(
        long,
        required = true,
        help = "Required. Emit the stable machine-readable JSON envelope on stdout"
    )]
    json: bool,
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

    #[arg(long, value_enum, help = "Query this provider session")]
    provider: ProviderArg,

    #[arg(long = "session-id", help = "Query this session id")]
    session_id: String,

    #[arg(
        long,
        value_enum,
        default_value_t = ViewArg::Full,
        help = "Turn detail level. `narrative` omits tool arguments, outputs, and payload blobs"
    )]
    view: ViewArg,

    #[arg(
        long,
        required = true,
        help = "Required. Emit the stable machine-readable JSON envelope on stdout"
    )]
    json: bool,
}

/// Queries one full turn detail payload.
#[derive(Debug, Args)]
struct QueryTurnArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(
        long = "project-id",
        help = "Query this configured project id. Defaults to the project resolved from the current directory"
    )]
    project_id: Option<String>,

    #[arg(long, value_enum, help = "Query this provider")]
    provider: ProviderArg,

    #[arg(long = "session-id", help = "Query this session id")]
    session_id: String,

    #[arg(long = "turn-ordinal", help = "Query this turn ordinal")]
    turn_ordinal: u64,

    #[arg(
        long,
        value_enum,
        default_value_t = ViewArg::Full,
        help = "Step detail level. `narrative` omits tool arguments, outputs, and payload blobs"
    )]
    view: ViewArg,

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
        long,
        required = true,
        help = "Required. Emit the stable machine-readable JSON envelope on stdout"
    )]
    json: bool,
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

    #[arg(long, value_enum, help = "Search in this mode")]
    mode: SearchModeArg,

    #[arg(long, help = "Search for this text or path fragment")]
    query: String,

    #[arg(long, value_enum, help = "Restrict search to this provider")]
    provider: Option<ProviderArg>,

    #[arg(long = "session-id", help = "Restrict search to this session id")]
    session_id: Option<String>,

    #[arg(long, default_value_t = 50, help = "Maximum turn hits to return")]
    limit: usize,

    #[arg(long, default_value_t = 0, help = "Number of turn hits to skip")]
    offset: usize,

    #[arg(
        long,
        required = true,
        help = "Required. Emit the stable machine-readable JSON envelope on stdout"
    )]
    json: bool,
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
    /// Queries the turn insights payload for one provider session turn.
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
        long,
        required = true,
        help = "Required. Emit the stable machine-readable JSON envelope on stdout"
    )]
    json: bool,
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

    #[arg(
        long,
        default_value_t = 1000,
        help = "Maximum indexed turns to inspect"
    )]
    limit: usize,

    #[arg(
        long,
        required = true,
        help = "Required. Emit the stable machine-readable JSON envelope on stdout"
    )]
    json: bool,
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

    #[arg(long, value_enum, help = "Query this provider")]
    provider: ProviderArg,

    #[arg(long = "session-id", help = "Query this session id")]
    session_id: String,

    #[arg(long = "turn-ordinal", help = "Query this turn ordinal")]
    turn_ordinal: u64,

    #[arg(
        long,
        required = true,
        help = "Required. Emit the stable machine-readable JSON envelope on stdout"
    )]
    json: bool,
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
    FileName,
    FilePath,
}

/// Represents the supported role filters for grep-style turn queries.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum TurnSearchRoleArg {
    User,
    Assistant,
    Both,
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
    let cli = Cli::parse();
    match cli.command {
        Commands::Init(args) => standard_exit(run_init(args)),
        Commands::Refresh(args) => standard_exit(run_refresh(args)),
        Commands::Link(args) => standard_exit(run_link(args)),
        Commands::Remove(args) => standard_exit(run_remove(args)),
        Commands::RenameFrom(args) => standard_exit(run_rename_from(args)),
        Commands::Sync(args) => standard_exit(run_sync(args)),
        Commands::Index(args) => standard_exit(run_index(args)),
        Commands::Query(args) => query_exit(run_query(args)),
        Commands::CodexSchemaAudit(args) => run_codex_schema_audit_command(args),
        Commands::ClaudeSchemaAudit(args) => run_claude_schema_audit_command(args),
    }
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
    match args.command {
        QueryCommands::Workspace(args) => run_query_workspace(args),
        QueryCommands::ResolveSession(args) => run_query_resolve_session(args),
        QueryCommands::Sessions(args) => run_query_sessions(args),
        QueryCommands::Files(args) => run_query_files(args),
        QueryCommands::SessionFiles(args) => run_query_session_files(args),
        QueryCommands::SessionBundle(args) => run_query_session_bundle(args),
        QueryCommands::Turns(args) => run_query_turns(args),
        QueryCommands::Turn(args) => run_query_turn(args),
        QueryCommands::Search(args) => run_query_search(args),
        QueryCommands::Insights(args) => run_query_insights(args),
    }
}

/// Queries the workspace/sidebar payload for one darc root.
fn run_query_workspace(args: QueryWorkspaceArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    print_json_envelope("darc.query.workspace.v1", &query_workspace(Some(args.root)))
}

/// Resolves one full session id or UUID prefix into canonical matches.
fn run_query_resolve_session(args: QueryResolveSessionArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    let data = query_resolve_sessions(
        Some(args.root),
        ResolveSessionQueryRequest {
            query: &args.input,
            provider: args.provider.map(provider_arg_to_source_kind),
            limit: DEFAULT_RESOLVE_SESSION_MATCH_LIMIT,
        },
    )?;
    if !args.pick_one {
        if data.matches.is_empty() && is_full_uuid_text(&data.query) {
            return Err(QueryProtocolError::unknown_resolve_session(&data.query, false).into());
        }
        return print_json_envelope("darc.query.resolve_session.v1", &data);
    }

    match data.matches.as_slice() {
        [] => Err(QueryProtocolError::unknown_resolve_session(
            &data.query,
            !is_full_uuid_text(&data.query),
        )
        .into()),
        [resolved] => print_json_envelope(
            "darc.query.resolve_session.v1",
            &ResolveSessionPickOneQueryData::new(&data.query, resolved.clone()),
        ),
        _ => Err(
            QueryProtocolError::ambiguous_session(&data.query, data.matches, data.truncated).into(),
        ),
    }
}

/// Queries the session list for one configured project.
fn run_query_sessions(args: QuerySessionsArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
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
        since.as_deref(),
        until.as_deref(),
        args.touched_path.as_deref(),
    )?;
    print_json_envelope("darc.query.sessions.v1", &data)
}

/// Queries file pivots for one configured project.
fn run_query_files(args: QueryFilesArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
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
            path: args.path.as_deref(),
            co_touched_with: args.co_touched_with.as_deref(),
            since: since.as_deref(),
            until: until.as_deref(),
            limit: args.limit,
        },
    )?;
    print_json_envelope("darc.query.files.v1", &data)
}

/// Queries one session-scoped per-file access summary payload.
fn run_query_session_files(args: QuerySessionFilesArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let session_id = resolve_query_session_id_for_project(
        &project,
        Some(provider_arg_to_source_kind(args.provider)),
        &args.session_id,
    )?;
    let data = query_session_files_for_project(
        &project,
        provider_arg_to_source_kind(args.provider),
        &session_id,
    )?;
    print_json_envelope("darc.query.session_files.v1", &data)
}

/// Queries one composite session bundle payload.
fn run_query_session_bundle(args: QuerySessionBundleArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let session_id = resolve_query_session_id_for_project(
        &project,
        Some(provider_arg_to_source_kind(args.provider)),
        &args.session_id,
    )?;
    let data = query_session_bundle_for_project(
        &project,
        provider_arg_to_source_kind(args.provider),
        &session_id,
        view_arg_to_session_bundle_view(args.view),
    )?;
    print_json_envelope("darc.query.session_bundle.v1", &data)
}

/// Queries the turn list for one provider session or grep request.
fn run_query_turns(args: QueryTurnsArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
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
    let resolved_session_id = args
        .session_id
        .as_deref()
        .map(|session_id| {
            resolve_query_session_id_for_project(
                &project,
                args.provider.map(provider_arg_to_source_kind),
                session_id,
            )
        })
        .transpose()?;
    if let Some(grep) = args.grep.as_deref() {
        let data = query_turn_matches_for_project(
            &project,
            TurnMatchesQueryRequest {
                project_id: "",
                project_root: None,
                provider: args.provider.map(provider_arg_to_source_kind),
                session_id: resolved_session_id.as_deref(),
                grep,
                role: turn_search_role_arg_to_role(args.role),
                context: args.context,
                since: since.as_deref(),
                until: until.as_deref(),
                touched_path: args.touched_path.as_deref(),
                view: turn_list_view_arg_to_view(args.view),
            },
        )?;
        return print_turn_matches_query_envelope(&data);
    }

    if args.role != TurnSearchRoleArg::Both {
        bail!("--role requires --grep");
    }
    if args.context != 0 {
        bail!("--context requires --grep");
    }
    if args.touched_path.is_some() {
        bail!("--touched-path requires --grep");
    }

    let provider = args
        .provider
        .context("query turns without --grep requires --provider")?;
    let session_id = resolved_session_id
        .as_deref()
        .context("query turns without --grep requires --session-id")?;
    let data = query_turns_for_project(
        &project,
        TurnsQueryRequest {
            project_id: "",
            provider: provider_arg_to_source_kind(provider),
            session_id,
            since: since.as_deref(),
            until: until.as_deref(),
            view: turn_list_view_arg_to_view(args.view),
        },
    )?;
    print_turns_query_envelope(&data)
}

/// Queries one full turn detail payload.
fn run_query_turn(args: QueryTurnArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let session_id = resolve_query_session_id_for_project(
        &project,
        Some(provider_arg_to_source_kind(args.provider)),
        &args.session_id,
    )?;
    let data = query_turn_for_project(
        &project,
        provider_arg_to_source_kind(args.provider),
        &session_id,
        args.turn_ordinal,
        TurnDetailOptions {
            include_raw: args.include_raw,
            include_insights: args.include_insights,
            narrative: matches!(args.view, ViewArg::Narrative),
        },
    )?;
    print_json_envelope("darc.query.turn.v1", &data)
}

/// Dispatches the supported machine-readable search query commands.
fn run_query_search(args: QuerySearchArgs) -> Result<()> {
    match args.command {
        QuerySearchCommands::Turns(args) => run_query_search_turns(args),
    }
}

/// Queries one paginated turn-search payload.
fn run_query_search_turns(args: QuerySearchTurnsArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let session_id = args
        .session_id
        .as_deref()
        .map(|session_id| {
            resolve_query_session_id_for_project(
                &project,
                args.provider.map(provider_arg_to_source_kind),
                session_id,
            )
        })
        .transpose()?;
    let data = query_search_turns_for_project(
        &project,
        SearchTurnsRequest {
            project_id: "",
            mode: search_mode_arg_to_search_mode(args.mode),
            query: &args.query,
            provider: args.provider.map(provider_arg_to_source_kind),
            session_id: session_id.as_deref(),
            limit: args.limit,
            offset: args.offset,
        },
    )?;
    print_json_envelope("darc.query.search.turns.v1", &data)
}

/// Dispatches the supported machine-readable insights query commands.
fn run_query_insights(args: QueryInsightsArgs) -> Result<()> {
    match args.command {
        QueryInsightsCommands::Workspace(args) => run_query_workspace_insights(args),
        QueryInsightsCommands::Project(args) => run_query_project_insights(args),
        QueryInsightsCommands::Turn(args) => run_query_turn_insights(args),
    }
}

/// Queries the workspace insights payload for one rolling host-local day window.
fn run_query_workspace_insights(args: QueryWorkspaceInsightsArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    let data = query_workspace_insight_report(Some(args.root), args.window_days)?;
    print_json_envelope("darc.query.insights.workspace.v1", &data)
}

/// Queries the project insights payload for one configured project.
fn run_query_project_insights(args: QueryProjectInsightsArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let data = query_project_insight_report_for_project(&project, args.limit)?;
    print_json_envelope("darc.query.insights.project.v1", &data)
}

/// Queries the turn insights payload for one provider session turn.
fn run_query_turn_insights(args: QueryTurnInsightsArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let session_id = resolve_query_session_id_for_project(
        &project,
        Some(provider_arg_to_source_kind(args.provider)),
        &args.session_id,
    )?;
    let data = query_turn_insight_report_for_project(
        &project,
        provider_arg_to_source_kind(args.provider),
        &session_id,
        args.turn_ordinal,
    )?;
    print_json_envelope("darc.query.insights.turn.v1", &data)
}

/// Writes one machine-readable JSON envelope to stdout.
fn print_json_envelope<T: Serialize>(schema: &'static str, data: &T) -> Result<()> {
    let payload = JsonEnvelope {
        schema,
        generated_at: current_utc_timestamp(),
        darc_version: env!("CARGO_PKG_VERSION"),
        data,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&payload)
            .context("failed to serialize query response JSON")?
    );
    Ok(())
}

/// Writes one `darc.query.turns.v1` envelope, compacting rows when `view` is `oneline`.
fn print_turns_query_envelope(data: &darc_core::query::TurnsQueryData) -> Result<()> {
    match data.view {
        TurnsView::Full => print_json_envelope("darc.query.turns.v1", data),
        TurnsView::Oneline => print_json_envelope(
            "darc.query.turns.v1",
            &TurnsOnelineQueryData::from_turns_query(data),
        ),
    }
}

/// Writes one `darc.query.turn_matches.v1` envelope, compacting rows when `view` is `oneline`.
fn print_turn_matches_query_envelope(data: &darc_core::query::TurnMatchesQueryData) -> Result<()> {
    match data.view {
        TurnsView::Full => print_json_envelope("darc.query.turn_matches.v1", data),
        TurnsView::Oneline => print_json_envelope(
            "darc.query.turn_matches.v1",
            &TurnMatchesOnelineQueryData::from_turn_matches_query(data),
        ),
    }
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

/// Returns an error unless the query command explicitly requested JSON output.
fn ensure_json_requested(json: bool) -> Result<()> {
    if json {
        return Ok(());
    }
    bail!("query commands currently require --json")
}

/// Resolves one project-scoped query target from an explicit id or the active project.
fn resolve_database_query_project_target(
    root: &std::path::Path,
    project_id: Option<&str>,
) -> Result<ResolvedQueryProject> {
    resolve_query_project(Some(root.to_path_buf()), project_id)
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
        SearchModeArg::FileName => SearchMode::FileName,
        SearchModeArg::FilePath => SearchMode::FilePath,
    }
}

/// Converts one parsed grep-role argument into the shared turn-search role.
fn turn_search_role_arg_to_role(role: TurnSearchRoleArg) -> TurnSearchRole {
    match role {
        TurnSearchRoleArg::User => TurnSearchRole::User,
        TurnSearchRoleArg::Assistant => TurnSearchRole::Assistant,
        TurnSearchRoleArg::Both => TurnSearchRole::Both,
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
    role: TurnSearchRole,
    user_preview: String,
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
            turns: data
                .turns
                .iter()
                .map(|turn| TurnsOnelineTurnRow {
                    turn_ordinal: turn.turn_ordinal,
                    role: turn.oneline_role,
                    user_preview: turn.oneline_user_preview.clone(),
                    step_count: turn.step_count,
                    tool_call_count: turn.tool_call_count,
                })
                .collect(),
        }
    }
}

/// Stores one compact row for grep-scoped `darc query turns --grep --view oneline`.
#[derive(Debug, Clone, Serialize)]
struct TurnMatchesOnelineTurnRow {
    provider: SourceKind,
    session_id: String,
    turn_ordinal: u64,
    role: TurnSearchRole,
    user_preview: String,
    step_count: u64,
    tool_call_count: u64,
    match_kind: Option<darc_core::query::TurnMatchKind>,
    match_snippet: Option<String>,
}

/// Stores one compact top-level payload for grep-scoped turn skims.
#[derive(Debug, Clone, Serialize)]
struct TurnMatchesOnelineQueryData {
    project_id: String,
    provider: Option<SourceKind>,
    session_id: Option<String>,
    grep: String,
    role: TurnSearchRole,
    context: u64,
    since: Option<String>,
    until: Option<String>,
    touched_path: Option<String>,
    view: TurnsView,
    turns: Vec<TurnMatchesOnelineTurnRow>,
}

impl TurnMatchesOnelineQueryData {
    /// Builds one compact grep-turn payload from the full shared query result.
    fn from_turn_matches_query(data: &darc_core::query::TurnMatchesQueryData) -> Self {
        Self {
            project_id: data.project_id.clone(),
            provider: data.provider,
            session_id: data.session_id.clone(),
            grep: data.grep.clone(),
            role: data.role,
            context: data.context,
            since: data.since.clone(),
            until: data.until.clone(),
            touched_path: data.touched_path.clone(),
            view: data.view,
            turns: data
                .turns
                .iter()
                .map(|turn| TurnMatchesOnelineTurnRow {
                    provider: turn.provider,
                    session_id: turn.session_id.clone(),
                    turn_ordinal: turn.turn_ordinal,
                    role: turn.oneline_role,
                    user_preview: turn.oneline_user_preview.clone(),
                    step_count: turn.step_count,
                    tool_call_count: turn.tool_call_count,
                    match_kind: turn.match_kind,
                    match_snippet: turn.match_snippet.clone(),
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

    println!("{draft}");
    if args.dry_run {
        println!("\n{}", format_init_status(&draft, true));
        println!();
        println!("{}", draft.config_toml()?);
    } else {
        println!("\n{}", format_init_status(&draft, false));
    }

    Ok(())
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

    println!("Project: {}", report.target_project_name);
    println!("Linked From: {}", report.source_project_name);
    println!("Project Root: {}", report.target_project_root.display());
    println!(
        "Known paths: {} total, {} added",
        report.total_known_paths,
        report.new_known_paths.len()
    );
    if report.config_written {
        println!("Updated config.");
    } else {
        println!("Config already covered all linked paths.");
    }

    Ok(())
}

/// Removes one configured project and its archived/indexed data.
fn run_remove(args: RemoveArgs) -> Result<()> {
    let report = remove_project(Some(args.root), &args.project)?;

    println!("Project: {}", report.project_name);
    println!("Project ID: {}", report.project_id);
    println!("Archive: {}", report.sessions_root.display());
    println!(
        "Indexed sessions removed: {}",
        report.indexed_sessions_removed
    );
    println!("Indexed turns removed: {}", report.indexed_turns_removed);
    if report.archive_deleted {
        println!("Deleted archive.");
    } else {
        println!("Archive did not exist.");
    }
    if report.config_written {
        println!("Updated config.");
    }

    Ok(())
}

/// Rebuilds one configured project's history under the active project's id.
fn run_rename_from(args: RenameArgs) -> Result<()> {
    let report = rename_project(Some(args.root), &args.project)?;

    println!("Project: {}", report.link.target_project_name);
    println!("Renamed From: {}", report.link.source_project_name);
    println!(
        "Known paths: {} total, {} added",
        report.link.total_known_paths,
        report.link.new_known_paths.len()
    );
    println!(
        "Synced {} session files and {} auxiliary files.",
        report.sync.sessions_copied, report.sync.auxiliary_copied
    );
    println!(
        "Indexed {} discovered sessions into {} indexed sessions and {} turns.",
        report.index.sessions_discovered,
        report.index.sessions_currently_indexed,
        report.index.turns_currently_indexed
    );
    println!(
        "Removed old project archive and {} indexed sessions.",
        report.remove.indexed_sessions_removed
    );

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

/// Prepares and optionally executes the project-scoped sync workflow.
fn run_sync(args: SyncArgs) -> Result<()> {
    let plan = prepare_sync(
        Some(args.root),
        SyncOptions {
            provider_filter: args.provider.into_iter().map(ProviderArg::into).collect(),
        },
    )
    .map_err(add_init_hint_for_unconfigured_project)?;

    println!("Project: {}", plan.project_name);
    println!("Project Root: {}", plan.project_root.display());
    println!("Archive: {}", plan.sessions_root.display());
    println!("Providers: {}", format_sources(&plan.sources));
    println!(
        "Sessions: {} to copy, {} unchanged",
        plan.sessions_to_copy(),
        plan.sessions_unchanged
    );
    println!(
        "Auxiliary: {} to copy, {} unchanged",
        plan.auxiliary_to_copy(),
        plan.auxiliary_unchanged
    );
    if !plan.new_known_paths.is_empty() {
        println!("Known paths: {} new", plan.new_known_paths.len());
    }
    for warning in &plan.warnings {
        eprintln!("warning: {warning}");
    }

    if args.dry_run {
        println!("\nDry run only. No files were written.");
        return Ok(());
    }

    let report = execute_sync(plan)?;
    println!(
        "\nSynced {} session files and {} auxiliary files.",
        report.sessions_copied, report.auxiliary_copied
    );
    if report.manifest_written {
        println!("Updated manifest.");
    }
    if report.config_written {
        println!("Updated config.");
    }

    Ok(())
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

    for skipped in &report.skipped_rollouts {
        eprintln!("warning: {}", format_skipped_rollout(skipped));
    }

    println!("Project: {}", report.project_name);
    println!("Project Root: {}", report.project_root.display());
    println!("Archive: {}", report.sessions_root.display());
    println!("Providers: {}", format_sources(&report.providers));
    println!("Index DB: {}", report.index_db_path.display());
    println!("Sessions discovered: {}", report.sessions_discovered);
    println!(
        "Sessions skipped this run: {}",
        report.sessions_skipped_this_run
    );
    println!(
        "Sessions currently indexed: {}",
        report.sessions_currently_indexed
    );
    println!(
        "Turns currently indexed: {}",
        report.turns_currently_indexed
    );
    println!("Skipped rollout files: {}", report.skipped_rollouts.len());

    Ok(())
}

/// Prints the combined sync and index summary for one refreshed project.
fn print_refresh_report(report: &RefreshReport) {
    for warning in &report.sync.warnings {
        eprintln!("warning [{}]: {warning}", report.sync.project_name);
    }
    for skipped in &report.index.skipped_rollouts {
        eprintln!(
            "warning [{}]: {}",
            report.sync.project_name,
            format_skipped_rollout(skipped)
        );
    }

    println!("Project: {}", report.sync.project_name);
    println!("Project Root: {}", report.sync.project_root.display());
    println!("Archive: {}", report.sync.sessions_root.display());
    match format_refresh_provider_lines(report) {
        RefreshProviderLines::Shared(providers) => println!("Providers: {providers}"),
        RefreshProviderLines::Split {
            sync_providers,
            index_providers,
        } => {
            println!("Sync Providers: {sync_providers}");
            println!("Index Providers: {index_providers}");
        }
    }
    println!("Index DB: {}", report.index.index_db_path.display());
    println!(
        "Sync Sessions: {} copied, {} unchanged",
        report.sync.sessions_copied, report.sync.sessions_unchanged
    );
    println!(
        "Sync Auxiliary: {} copied, {} unchanged",
        report.sync.auxiliary_copied, report.sync.auxiliary_unchanged
    );
    println!(
        "Index Sessions Discovered: {}",
        report.index.sessions_discovered
    );
    println!(
        "Index Sessions Skipped This Run: {}",
        report.index.sessions_skipped_this_run
    );
    println!(
        "Index Sessions Currently Indexed: {}",
        report.index.sessions_currently_indexed
    );
    println!(
        "Index Turns Currently Indexed: {}",
        report.index.turns_currently_indexed
    );
    println!(
        "Skipped rollout files: {}",
        report.index.skipped_rollouts.len()
    );
    if report.sync.manifest_written {
        println!("Updated manifest.");
    }
    if report.sync.config_written {
        println!("Updated config.");
    }
}

/// Prints one multi-project refresh report with per-project results and totals.
fn print_refresh_all_report(report: &RefreshAllBestEffortReport) {
    for (index, project) in report.projects.iter().enumerate() {
        if index > 0 {
            println!();
        }
        print_refresh_all_project_report(project);
    }
    println!(
        "\nRefresh summary: {} succeeded, {} failed.",
        report.refreshed_count(),
        report.failed_count()
    );
}

/// Prints one project-scoped entry from a multi-project refresh report.
fn print_refresh_all_project_report(project: &RefreshProjectAttempt) {
    match project {
        RefreshProjectAttempt::Refreshed(report) => print_refresh_report(report),
        RefreshProjectAttempt::Failed(failure) => print_refresh_project_failure(failure),
    }
}

/// Prints one structured project refresh failure from a best-effort workspace refresh.
fn print_refresh_project_failure(failure: &RefreshProjectFailure) {
    println!("Project: {}", failure.project_name);
    println!("Project Root: {}", failure.project_root.display());
    println!("Status: failed");
    println!("Error: {:#}", failure.error);
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
