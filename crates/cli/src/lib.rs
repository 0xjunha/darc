#[cfg(test)]
mod tests;

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use darc_core::query::{
    query_project_insight_report, query_sessions, query_turn, query_turn_insight_report,
    query_turns, query_workspace, query_workspace_insight_report,
};
use darc_core::{
    IndexOptions, InitDraft, RefreshOptions, RefreshReport, SkippedRollout, SourceKind,
    SyncOptions, default_root_path, execute_sync, index_project_sessions, link_project,
    prepare_init, prepare_sync, refresh_all_projects, refresh_project, remove_project,
    rename_project, write_init,
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
        long_about = "Sync then index archived sessions for the active project.\n\nThis is the daily happy path after `darc init`.\nBy default it refreshes the project resolved from the current directory.\nUse `--provider` to limit both sync and index to selected providers.\nUse `--all` to refresh every registered project in the shared darc workspace."
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
        help = "Refresh every registered project instead of only the active one"
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
    /// Queries the session list for one configured project.
    Sessions(QuerySessionsArgs),
    /// Queries the turn list for one provider session.
    Turns(QueryTurnsArgs),
    /// Queries one full turn detail payload.
    Turn(QueryTurnArgs),
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

/// Queries the session list for one configured project.
#[derive(Debug, Args)]
struct QuerySessionsArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(long = "project-id", help = "Query this configured project id")]
    project_id: String,

    #[arg(
        long,
        required = true,
        help = "Required. Emit the stable machine-readable JSON envelope on stdout"
    )]
    json: bool,
}

/// Queries the turn list for one provider session.
#[derive(Debug, Args)]
struct QueryTurnsArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(long = "project-id", help = "Query this configured project id")]
    project_id: String,

    #[arg(long, value_enum, help = "Query this provider")]
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

/// Queries one full turn detail payload.
#[derive(Debug, Args)]
struct QueryTurnArgs {
    #[arg(long, default_value_os_t = default_root_path(), help = "Read from this darc root")]
    root: PathBuf,

    #[arg(long = "project-id", help = "Query this configured project id")]
    project_id: String,

    #[arg(long, value_enum, help = "Query this provider")]
    provider: ProviderArg,

    #[arg(long = "session-id", help = "Query this session id")]
    session_id: String,

    #[arg(long = "turn-ordinal", help = "Query this turn ordinal")]
    turn_ordinal: u64,

    #[arg(
        long,
        help = "Include optional raw/debug fields such as raw_steps_json"
    )]
    include_raw: bool,

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

    #[arg(long = "project-id", help = "Query this configured project id")]
    project_id: String,

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

    #[arg(long = "project-id", help = "Query this configured project id")]
    project_id: String,

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
        QueryCommands::Sessions(args) => run_query_sessions(args),
        QueryCommands::Turns(args) => run_query_turns(args),
        QueryCommands::Turn(args) => run_query_turn(args),
        QueryCommands::Insights(args) => run_query_insights(args),
    }
}

/// Queries the workspace/sidebar payload for one darc root.
fn run_query_workspace(args: QueryWorkspaceArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    print_query_json("darc.query.workspace.v1", &query_workspace(Some(args.root)))
}

/// Queries the session list for one configured project.
fn run_query_sessions(args: QuerySessionsArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    let data = query_sessions(Some(args.root), &args.project_id)?;
    print_query_json("darc.query.sessions.v1", &data)
}

/// Queries the turn list for one provider session.
fn run_query_turns(args: QueryTurnsArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    let data = query_turns(
        Some(args.root),
        &args.project_id,
        provider_arg_to_source_kind(args.provider),
        &args.session_id,
    )?;
    print_query_json("darc.query.turns.v1", &data)
}

/// Queries one full turn detail payload.
fn run_query_turn(args: QueryTurnArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    let data = query_turn(
        Some(args.root),
        &args.project_id,
        provider_arg_to_source_kind(args.provider),
        &args.session_id,
        args.turn_ordinal,
        args.include_raw,
    )?;
    print_query_json("darc.query.turn.v1", &data)
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
    print_query_json("darc.query.insights.workspace.v1", &data)
}

/// Queries the project insights payload for one configured project.
fn run_query_project_insights(args: QueryProjectInsightsArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    let data = query_project_insight_report(Some(args.root), &args.project_id, args.limit)?;
    print_query_json("darc.query.insights.project.v1", &data)
}

/// Queries the turn insights payload for one provider session turn.
fn run_query_turn_insights(args: QueryTurnInsightsArgs) -> Result<()> {
    ensure_json_requested(args.json)?;
    let data = query_turn_insight_report(
        Some(args.root),
        &args.project_id,
        provider_arg_to_source_kind(args.provider),
        &args.session_id,
        args.turn_ordinal,
    )?;
    print_query_json("darc.query.insights.turn.v1", &data)
}

/// Writes one machine-readable JSON envelope to stdout.
fn print_query_json<T: Serialize>(schema: &'static str, data: &T) -> Result<()> {
    let payload = QueryEnvelope {
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

/// Returns one machine-readable JSON error envelope string.
fn format_query_error(error: &anyhow::Error) -> String {
    let causes = error
        .chain()
        .skip(1)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let payload = QueryErrorEnvelope {
        schema: "darc.error.v1",
        generated_at: current_utc_timestamp(),
        darc_version: env!("CARGO_PKG_VERSION"),
        error: QueryErrorData {
            message: error.to_string(),
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

/// Converts one parsed provider argument back into the shared source kind.
fn provider_arg_to_source_kind(provider: ProviderArg) -> SourceKind {
    match provider {
        ProviderArg::Claude => SourceKind::Claude,
        ProviderArg::Codex => SourceKind::Codex,
    }
}

/// Stores one machine-readable query success envelope.
#[derive(Debug, Serialize)]
struct QueryEnvelope<'a, T> {
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
    causes: Vec<String>,
}

/// Returns the current UTC timestamp formatted for query protocol envelopes.
fn current_utc_timestamp() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_seconds = duration.as_secs();
    let days = i64::try_from(total_seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_of_day = total_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Converts one Unix-day count into a UTC civil date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
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
        let report = refresh_all_projects(Some(args.root), options)?;
        for (index, project) in report.projects.iter().enumerate() {
            if index > 0 {
                println!();
            }
            print_refresh_report(project);
        }
        println!("\nRefreshed {} project(s).", report.projects.len());
        return Ok(());
    }

    let report = refresh_project(Some(args.root), options)?;
    print_refresh_report(&report);
    Ok(())
}

/// Prepares and optionally executes the project-scoped sync workflow.
fn run_sync(args: SyncArgs) -> Result<()> {
    let plan = prepare_sync(
        Some(args.root),
        SyncOptions {
            provider_filter: args.provider.into_iter().map(ProviderArg::into).collect(),
        },
    )?;

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
