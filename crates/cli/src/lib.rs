mod args;
mod output;
mod query_commands;
mod refresh;
mod schema_audit;
mod service;
mod status;
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
use query_commands::*;
use refresh::*;
use schema_audit::*;
use serde::Serialize;
use serde_json::{Value as JsonValue, json};
use service::*;
use status::*;
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

/// Prints the selected agent-facing Darc usage surface.
fn run_agent_help(args: AgentHelpArgs) -> Result<()> {
    if args.agents_md_line {
        println!("{}", render_agents_md_line());
    } else {
        print!("{}", render_agent_help_guide());
    }
    Ok(())
}

/// Renders the marker-wrapped one-line AGENTS.md guidance.
fn render_agents_md_line() -> String {
    format!(
        "{AGENTS_MD_GUIDANCE_START_MARKER} {AGENTS_MD_GUIDANCE_TEXT} {AGENTS_MD_GUIDANCE_END_MARKER}"
    )
}

/// Renders concise operating guidance for agents using Darc.
fn render_agent_help_guide() -> &'static str {
    r#"# Darc Agent Help

Darc is a local archive and query layer for coding-agent sessions. Use it to recover prior-session evidence, not as a replacement for reading the current repository, docs, or tests.

## When to Use Darc

Use Darc as a primary evidence source for:
- prior decisions or rationale
- regressions and repeated failures
- PR handoffs or unfinished work
- ambiguous user references to earlier work
- file, module, or command history across past sessions

Use Darc only as orientation for current-code audits, refactor planning, or implementation work. In those cases, let Darc point you toward likely context, then verify everything against current source and tests.

Skip Darc for fresh, self-contained tasks where current files, tests, docs, or the user prompt are enough.

## Safe First Commands

- `darc status --json`: preflight active-project resolution and index freshness.
- `darc search <query> --limit 5`: find matching turns by keyword.
- `darc search --mode literal --query <text> --limit 5`: find exact text without regex escaping.
- `darc list sessions --limit 5`: browse recent indexed sessions.
- `darc list files --limit 10`: rank files touched in prior sessions.
- `darc list sessions --touching <glob> --limit 5`: find sessions that touched a file or path pattern.
- `darc search --mode file-path <glob> --limit 5`: find turns associated with touched file paths.

## Task Recipes

Decision or rationale:
- `darc search --mode literal --query "<decision phrase>" --limit 5`
- `darc show turn <SESSION_ID> <TURN_ORDINAL> --step-limit 10`

Regression or repeated failure:
- `darc search --mode literal --query "<error text>" --limit 5`
- `darc list sessions --touching <path> --limit 5`

File or module history:
- `darc list sessions --touching <path> --limit 5`
- `darc list files --co-touched-with <path> --limit 10`

Hotspot orientation:
- `darc list files --limit 25`
- Treat historical churn as a map, not a verdict.

## Evidence Ladder

1. Start with bounded `search`, `list`, or `stats` reads.
2. Keep the returned `session_id` and `turn_ordinal` handles.
3. Skim one candidate session with `darc list turns <SESSION_ID> --view oneline --limit 20`.
4. Inspect exact evidence with `darc show turn <SESSION_ID> <TURN_ORDINAL> --step-limit 10`.
5. Use `darc show session <SESSION_ID> --turn-limit 5 --step-limit 10` only after narrowing.

## Reporting Darc Evidence

When Darc materially shapes your answer, briefly separate prior-session evidence from current verification:

- Darc showed: the relevant prior decision, session evidence, or file/history pattern.
- Current source/tests confirmed: what is true in the repository now.

When exact prior evidence matters, include the `session_id` and `turn_ordinal` so another agent can inspect the same turn. Do not list Darc commands unless they help reproduce the evidence.

## Output Discipline

- Prefer small `--limit`, `--turn-limit`, and `--step-limit` values first.
- Prefer `darc show turn` for exact evidence and `darc show session` for bounded broader context.
- Use `--color never` when piping JSON to `jq` or another parser.
- Do not treat high historical churn as proof of bad code.

## Mutating Boundaries

Read surfaces such as `status --json`, `list`, `show`, `search`, `stats`, and `resolve` are safe for investigation.

`darc refresh`, `darc sync`, `darc index`, `darc init`, `darc project ...`, and service/upgrade commands can write local Darc state or config. Run them only when freshness or setup is part of the task.
"#
}

/// Dispatches the supported project-management commands.
fn run_project(args: ProjectArgs) -> Result<()> {
    match args.command {
        ProjectCommands::Link(args) => run_link(args),
        ProjectCommands::Remove(args) => run_remove(args),
        ProjectCommands::RenameFrom(args) => run_rename_from(args),
    }
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
