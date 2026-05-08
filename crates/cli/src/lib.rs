mod agent_help;
mod args;
mod output;
mod project;
mod query_commands;
mod refresh;
mod schema_audit;
mod service;
mod status;
mod sync_index;
#[cfg(test)]
mod tests;
mod upgrade;

use std::{
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

use agent_help::*;
use anyhow::{Context, Result, anyhow, bail};
use args::*;
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
use output::*;
use project::*;
use query_commands::*;
use refresh::*;
use schema_audit::*;
use serde::Serialize;
use serde_json::{Value as JsonValue, json};
use service::*;
use status::*;
use sync_index::*;
use upgrade::*;

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
    let upgrade_nudge = UpgradeNudgeContext::start(&cli.command);
    let exit_code = match cli.command {
        Commands::Init(args) => standard_exit(run_init(args)),
        Commands::Refresh(args) => standard_exit(run_refresh(args)),
        Commands::Status(args) if args.json => json_exit(run_status(args)),
        Commands::Status(args) => standard_exit(run_status(args)),
        Commands::AgentHelp(args) => standard_exit(run_agent_help(args)),
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
    };
    if let Some(nudge) = upgrade_nudge {
        nudge.refresh_after_command(exit_code);
    }
    exit_code
}

/// Maps Clap parse errors to the correct command-family output format.
fn clap_error_exit(error: clap::Error, args: &[OsString]) -> i32 {
    if is_json_read_invocation(args) && !is_clap_display_request(error.kind()) {
        eprintln!("{}", format_json_clap_error(&error, args));
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
