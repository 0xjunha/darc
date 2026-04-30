use std::{
    path::PathBuf,
    time::{Duration, Instant, UNIX_EPOCH},
};

use anyhow::anyhow;
use clap::{ColorChoice, CommandFactory, Parser};
use darc_core::{
    IndexReport, RefreshAllBestEffortReport, RefreshProjectAttempt, RefreshProjectFailure,
    RefreshReport, SourceKind, SyncReport,
};
use darc_rollout_audit::claude::{
    ClaudeSchemaAuditReport, ClaudeSchemaDrift, ClaudeSchemaDriftWindow, ClaudeSchemaSurveyMode,
    ClaudeSdkSchemaDrift,
};
use darc_rollout_audit::codex::{CodexSchemaAuditReport, CodexSchemaDrift};
use darc_rollout_audit::{claude::ClaudeSchemaAuditOutcome, codex::CodexSchemaAuditOutcome};
use serde_json::Value;

use super::{
    Cli, ColorArg, Commands, HELP_STYLES, QueryCommands, QueryInsightsCommands,
    claude_schema_audit_exit_code, codex_schema_audit_exit_code, format_claude_schema_audit_report,
    format_codex_schema_audit_report, format_query_clap_error, format_query_error,
    parse_window_days, resolve_query_time_bound_at, should_auto_color_output, should_color_output,
};

fn compatible_report() -> CodexSchemaAuditReport {
    CodexSchemaAuditReport {
        release_source: "GitHub Releases (openai/codex)".to_owned(),
        binary_cache_dir: "/tmp/darc-cache".into(),
        latest_stable_release_tag: "rust-v0.118.0".to_owned(),
        latest_exact_covered_version: "0.118.0".to_owned(),
        audited_tags: vec!["rust-v0.118.0".to_owned()],
        outcome: CodexSchemaAuditOutcome::Compatible,
    }
}

/// Extracts query arguments from a parsed CLI for focused parser assertions.
fn query_args(cli: Cli) -> super::QueryArgs {
    match cli.command {
        Commands::Query(args) => *args,
        command => panic!("expected query command, got {command:?}"),
    }
}

/// Renders long help for one nested command path.
fn help_for_command_path(path: &[&str]) -> String {
    let mut command = Cli::command();
    let mut current = &mut command;
    for name in path {
        current = current
            .find_subcommand_mut(name)
            .unwrap_or_else(|| panic!("subcommand `{name}` should be present"));
    }
    current.render_long_help().to_string()
}

fn compatible_claude_report() -> ClaudeSchemaAuditReport {
    ClaudeSchemaAuditReport {
        release_source: "npm registry (@anthropic-ai/claude-code)".to_owned(),
        binary_cache_dir: "/tmp/darc-claude-cache".into(),
        latest_published_version: "2.1.92".to_owned(),
        latest_exact_covered_version: "2.1.87".to_owned(),
        audited_versions: vec!["2.1.92".to_owned(), "2.1.87".to_owned()],
        inspected_versions: vec!["2.1.92".to_owned(), "2.1.87".to_owned()],
        assumed_compatible_intervals: Vec::new(),
        sample_stride: 1,
        used_host_auth: false,
        survey_mode: ClaudeSchemaSurveyMode::Refine,
        transcript_drift_windows: Vec::new(),
        outcome: ClaudeSchemaAuditOutcome::Compatible,
        supplementary_sdk_drift: Some(ClaudeSdkSchemaDrift {
            first_drift_version: "2.1.92".to_owned(),
            difference_summary: vec![
                "$/agent_sdk_version: changed from \"0.2.87\" to \"0.2.92\"".to_owned(),
            ],
        }),
    }
}

fn sample_refresh_report(project_name: &str) -> RefreshReport {
    let project_root = PathBuf::from(format!("/tmp/{project_name}"));
    let sessions_root = project_root.join("sessions");
    RefreshReport {
        sync: SyncReport {
            project_name: project_name.to_owned(),
            project_root: project_root.clone(),
            sessions_root: sessions_root.clone(),
            sources: vec![SourceKind::Codex],
            sessions_copied: 1,
            sessions_unchanged: 0,
            auxiliary_copied: 0,
            auxiliary_unchanged: 0,
            new_known_paths: Vec::new(),
            warnings: Vec::new(),
            manifest_written: false,
            config_written: false,
        },
        index: IndexReport {
            project_name: project_name.to_owned(),
            project_root: project_root.clone(),
            sessions_root,
            index_db_path: project_root.join("index.sqlite"),
            providers: vec![SourceKind::Codex],
            sessions_discovered: 1,
            sessions_skipped_this_run: 0,
            sessions_currently_indexed: 1,
            turns_currently_indexed: 2,
            skipped_rollouts: Vec::new(),
        },
    }
}

#[test]
fn parses_hidden_codex_schema_audit_command() {
    let cli = Cli::try_parse_from(["darc", "codex-schema-audit"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::CodexSchemaAudit(super::CodexSchemaAuditArgs { .. })
    ));
}

#[test]
fn top_level_help_points_to_common_workflows() {
    let help = Cli::command().render_long_help().to_string();

    assert!(help.contains("Archive, index, and query coding-agent sessions"));
    assert!(help.contains("Common workflows:"));
    assert!(help.contains("darc status"));
    assert!(help.contains("darc query search turns \"panic\" --limit 5"));
    assert!(help.contains("darc help <command>"));
}

#[test]
fn help_uses_terminal_color_auto() {
    let mut command = Cli::command();
    command.build();

    assert_eq!(command.get_color(), ColorChoice::Auto);
    assert_eq!(
        *command.get_styles().get_header(),
        *HELP_STYLES.get_header()
    );

    let query = command
        .find_subcommand("query")
        .expect("query subcommand should be present");
    assert_eq!(*query.get_styles().get_header(), *HELP_STYLES.get_header());
}

#[test]
fn rendered_help_is_plain_but_carries_ansi_styles() {
    let styled = Cli::command().render_long_help();
    let plain = styled.to_string();
    let ansi = styled.ansi().to_string();

    assert!(!plain.contains("\x1b["));
    assert!(ansi.contains("\x1b["));
    assert_eq!(strip_ansi_text(&ansi), plain);
}

#[test]
fn parses_hidden_claude_schema_audit_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "claude-schema-audit",
        "--sample-stride",
        "10",
        "--use-host-auth",
        "--from-version",
        "2.1.84",
        "--survey-mode",
        "coarse",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::ClaudeSchemaAudit(super::ClaudeSchemaAuditArgs {
            sample_stride,
            use_host_auth,
            from_version,
            survey_mode,
            ..
        }) if sample_stride == 10
            && use_host_auth
            && from_version.as_deref() == Some("2.1.84")
            && matches!(survey_mode, super::ClaudeSurveyModeArg::Coarse)
    ));
}

#[test]
fn refresh_command_accepts_provider_filters_and_all() {
    let cli = Cli::try_parse_from(["darc", "refresh", "--provider", "claude", "--all"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::Refresh(super::RefreshArgs { provider, all, .. })
            if provider.len() == 1 && all
    ));
}

#[test]
fn refresh_command_accepts_watch_options() {
    let cli = Cli::try_parse_from([
        "darc",
        "refresh",
        "--watch",
        "--all",
        "--debounce",
        "30s",
        "--min-interval",
        "60s",
        "--reconcile-interval",
        "10m",
        "--poll",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Refresh(super::RefreshArgs {
            watch: true,
            all: true,
            debounce,
            min_interval,
            reconcile_interval,
            poll: true,
            ..
        }) if debounce.as_deref() == Some("30s")
            && min_interval.as_deref() == Some("60s")
            && reconcile_interval.as_deref() == Some("10m")
    ));
}

#[test]
fn parses_service_lifecycle_command() {
    let cli = Cli::try_parse_from(["darc", "service", "enable"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::Service(super::ServiceArgs {
            command: super::ServiceCommands::Enable,
            ..
        })
    ));
}

#[test]
fn parse_duration_accepts_supported_units() {
    assert_eq!(
        super::parse_duration("500ms").unwrap(),
        Duration::from_millis(500)
    );
    assert_eq!(
        super::parse_duration("30s").unwrap(),
        Duration::from_secs(30)
    );
    assert_eq!(
        super::parse_duration("5m").unwrap(),
        Duration::from_secs(300)
    );
    assert_eq!(
        super::parse_duration("1h").unwrap(),
        Duration::from_secs(3_600)
    );
}

#[test]
fn parse_duration_rejects_missing_unit() {
    let error = super::parse_duration("30").unwrap_err();

    assert!(format!("{error:#}").contains("duration must use a unit"));
}

#[test]
fn reconcile_refresh_runs_when_interval_elapses() {
    let settings = sample_watch_settings(
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(600),
    );
    let now = Instant::now();

    assert!(super::should_run_reconcile_refresh(
        Some(now - Duration::from_secs(600)),
        now,
        &settings
    ));
}

#[test]
fn watched_refresh_still_respects_debounce() {
    let settings = sample_watch_settings(
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(600),
    );
    let now = Instant::now();

    assert!(!super::should_run_watched_refresh(
        Some(now - Duration::from_secs(29)),
        Some(now - Duration::from_secs(600)),
        now,
        &settings
    ));
}

#[test]
fn status_command_accepts_workspace_and_check_flags() {
    let cli = Cli::try_parse_from(["darc", "status", "--workspace", "--check"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::Status(super::StatusArgs {
            workspace: true,
            check: true,
            ..
        })
    ));
}

fn sample_watch_settings(
    debounce: Duration,
    min_interval: Duration,
    reconcile_interval: Duration,
) -> super::WatchSettings {
    super::WatchSettings {
        debounce,
        min_interval,
        reconcile_interval,
        provider_filter: Vec::new(),
        poll: false,
        watch_paths: Vec::new(),
    }
}

#[test]
fn human_command_help_groups_options() {
    let status_help = help_for_command_path(&["status"]);
    assert!(status_help.contains("Workspace:"));
    assert!(status_help.contains("Scope:"));
    assert!(status_help.contains("Mode:"));

    let sync_help = help_for_command_path(&["sync"]);
    assert!(sync_help.contains("Workspace:"));
    assert!(sync_help.contains("Mode:"));
    assert!(sync_help.contains("Selection:"));
    assert!(sync_help.contains("Preview pending copies without writing files"));
    assert!(sync_help.contains("darc sync --dry-run"));
}

#[test]
fn refresh_all_exit_status_errors_when_any_project_failed() {
    let report = RefreshAllBestEffortReport {
        projects: vec![
            RefreshProjectAttempt::Refreshed(Box::new(sample_refresh_report("repo-a"))),
            RefreshProjectAttempt::Failed(RefreshProjectFailure {
                project_name: "repo-b".into(),
                project_root: PathBuf::from("/tmp/repo-b"),
                error: anyhow!("failed to refresh project `repo-b`: boom"),
            }),
        ],
    };

    let error = super::refresh_all_exit_status(&report).unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "1 project(s) failed during workspace refresh"
    );
}

#[test]
fn index_command_accepts_provider_filters() {
    let cli = Cli::try_parse_from(["darc", "index", "--provider", "claude"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::Index(super::IndexArgs { provider, .. }) if provider.len() == 1
    ));
}

#[test]
fn sync_command_accepts_provider_filters() {
    let cli = Cli::try_parse_from(["darc", "sync", "--provider", "claude"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::Sync(super::SyncArgs { provider, .. }) if provider.len() == 1
    ));
}

#[test]
fn parses_link_command() {
    let cli = Cli::try_parse_from(["darc", "link", "memstack"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::Link(super::LinkArgs { project, .. }) if project == "memstack"
    ));
}

#[test]
fn parses_remove_command() {
    let cli = Cli::try_parse_from(["darc", "remove", "memstack"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::Remove(super::RemoveArgs { project, .. }) if project == "memstack"
    ));
}

#[test]
fn parses_rename_command() {
    let cli = Cli::try_parse_from(["darc", "rename-from", "memstack"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::RenameFrom(super::RenameArgs { project, .. }) if project == "memstack"
    ));
}

#[test]
fn parses_query_workspace_command() {
    let cli = Cli::try_parse_from(["darc", "query", "workspace"]).unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            color,
            command: QueryCommands::Workspace(super::QueryWorkspaceArgs { .. }),
            ..
        } if matches!(color, ColorArg::Auto)
    ));
}

#[test]
fn parses_query_color_argument() {
    let cli = Cli::try_parse_from(["darc", "query", "--color", "always", "workspace"]).unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            color,
            command: QueryCommands::Workspace(super::QueryWorkspaceArgs { .. }),
            ..
        } if matches!(color, ColorArg::Always)
    ));
}

#[test]
fn parses_query_resolve_session_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "resolve-session",
        "11111111",
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--pick-one",
    ])
    .unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            command: QueryCommands::ResolveSession(super::QueryResolveSessionArgs {
                input,
                project_id,
                provider,
                pick_one,
                ..
            }),
            ..
        } if input == "11111111"
            && project_id.as_deref() == Some("repo-abc123")
            && matches!(provider, Some(super::ProviderArg::Codex))
            && pick_one
    ));
}

#[test]
fn query_workspace_rejects_removed_json_flag() {
    let error = Cli::try_parse_from(["darc", "query", "workspace", "--json"]).unwrap_err();

    assert!(error.to_string().contains("unexpected argument '--json'"));
}

#[test]
fn parses_query_turn_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "turn",
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "session-1",
        "2",
        "--view",
        "narrative",
        "--include-raw",
        "--include-insights",
    ])
    .unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            command: QueryCommands::Turn(super::QueryTurnArgs {
                project_id,
                session_id_arg,
                turn_ordinal_arg,
                session_id,
                turn_ordinal,
                view,
                include_raw,
                include_insights,
                step_limit,
                step_offset,
                ..
            }),
            ..
        } if project_id.as_deref() == Some("repo-abc123")
            && session_id_arg.as_deref() == Some("session-1")
            && turn_ordinal_arg.as_deref() == Some("2")
            && session_id.is_none()
            && turn_ordinal.is_none()
            && matches!(view, Some(super::ViewArg::Narrative))
            && include_raw
            && include_insights
            && step_limit == darc_core::query::DEFAULT_TURN_STEP_LIMIT
            && step_offset == 0
    ));
}

#[test]
fn parses_query_sessions_with_time_bounds() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "sessions",
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--since",
        "5d",
        "--until",
        "2026-04-07T00:00:00Z",
    ])
    .unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            command: QueryCommands::Sessions(super::QuerySessionsArgs {
                project_id,
                provider,
                since,
                until,
                touched_path,
                ..
            }),
            ..
        } if project_id.as_deref() == Some("repo-abc123")
            && matches!(provider, Some(super::ProviderArg::Codex))
            && since.as_deref() == Some("5d")
            && until.as_deref() == Some("2026-04-07T00:00:00Z")
            && touched_path.is_none()
    ));
}

#[test]
fn parses_query_sessions_without_project_id() {
    let cli = Cli::try_parse_from(["darc", "query", "sessions"]).unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            command: QueryCommands::Sessions(super::QuerySessionsArgs {
                project_id,
                ..
            }),
            ..
        } if project_id.is_none()
    ));
}

#[test]
fn parses_query_sessions_touched_path_filter() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "sessions",
        "--project-id",
        "repo-abc123",
        "--touched-path",
        "src/components/**",
        "--limit",
        "25",
        "--offset",
        "50",
    ])
    .unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            command: QueryCommands::Sessions(super::QuerySessionsArgs {
                project_id,
                touched_path,
                limit,
                offset,
                ..
            }),
            ..
        } if project_id.as_deref() == Some("repo-abc123")
            && touched_path.as_deref() == Some("src/components/**")
            && limit == 25
            && offset == 50
    ));
}

#[test]
fn parses_query_files_path_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "files",
        "--project-id",
        "repo-abc123",
        "--provider",
        "claude",
        "src/components/**/*.rs",
        "--since",
        "30d",
        "--until",
        "2026-04-07T00:00:00Z",
    ])
    .unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            command: QueryCommands::Files(super::QueryFilesArgs {
                project_id,
                provider,
                path,
                path_arg,
                co_touched_with,
                since,
                until,
                limit,
                offset,
                ..
            }),
            ..
        } if project_id.as_deref() == Some("repo-abc123")
            && matches!(provider, Some(super::ProviderArg::Claude))
            && path.is_none()
            && path_arg.as_deref() == Some("src/components/**/*.rs")
            && co_touched_with.is_none()
            && since.as_deref() == Some("30d")
            && until.as_deref() == Some("2026-04-07T00:00:00Z")
            && limit == 50
            && offset == 0
    ));
}

#[test]
fn parses_query_files_co_touched_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "files",
        "--project-id",
        "repo-abc123",
        "--co-touched-with",
        "src/components/planner.rs",
        "--since",
        "7d",
        "--until",
        "2026-04-08T00:00:00Z",
        "--limit",
        "10",
        "--offset",
        "5",
    ])
    .unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            command: QueryCommands::Files(super::QueryFilesArgs {
                project_id,
                path,
                co_touched_with,
                since,
                until,
                limit,
                offset,
                ..
            }),
            ..
        } if project_id.as_deref() == Some("repo-abc123")
            && path.is_none()
            && co_touched_with.as_deref() == Some("src/components/planner.rs")
            && since.as_deref() == Some("7d")
            && until.as_deref() == Some("2026-04-08T00:00:00Z")
            && limit == 10
            && offset == 5
    ));
}

#[test]
fn parses_query_session_files_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "session-files",
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "session-1",
    ])
    .unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            command: QueryCommands::SessionFiles(super::QuerySessionFilesArgs {
                project_id,
                provider,
                session_id_arg,
                session_id,
                ..
            }),
            ..
        } if project_id.as_deref() == Some("repo-abc123")
            && matches!(provider, Some(super::ProviderArg::Codex))
            && session_id_arg.as_deref() == Some("session-1")
            && session_id.is_none()
    ));
}

#[test]
fn parses_query_session_bundle_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "session-bundle",
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "session-1",
        "--view",
        "narrative",
    ])
    .unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            command: QueryCommands::SessionBundle(super::QuerySessionBundleArgs {
                project_id,
                provider,
                session_id_arg,
                session_id,
                session_view,
                view,
                turn_limit,
                turn_offset,
                step_limit,
                step_offset,
                ..
            }),
            ..
        } if project_id.as_deref() == Some("repo-abc123")
            && matches!(provider, Some(super::ProviderArg::Codex))
            && session_id_arg.as_deref() == Some("session-1")
            && session_id.is_none()
            && matches!(session_view, super::SessionListViewArg::Compact)
            && matches!(view, super::ViewArg::Narrative)
            && turn_limit == 50
            && turn_offset == 0
            && step_limit == darc_core::query::DEFAULT_TURN_STEP_LIMIT
            && step_offset == 0
    ));
}

#[test]
fn query_session_bundle_help_mentions_prompt_and_final_message_projection() {
    let help = help_for_command_path(&["query", "session-bundle"]);

    assert!(help.contains("--session-view"));
    assert!(help.contains("session prompt/final message"));
    assert!(help.contains("compact previews"));
    assert!(help.contains("Identity:"));
    assert!(help.contains("Output:"));
    assert!(help.contains("Result Size:"));
    assert!(help.contains("darc query session-bundle <SESSION_ID>"));
}

#[test]
fn parses_query_turns_session_scope_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "turns",
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "session-1",
    ])
    .unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            command: QueryCommands::Turns(super::QueryTurnsArgs {
                project_id,
                provider,
                session_id_arg,
                session_id,
                view,
                limit,
                offset,
                ..
            }),
            ..
        } if project_id.as_deref() == Some("repo-abc123")
            && matches!(provider, Some(super::ProviderArg::Codex))
            && session_id_arg.as_deref() == Some("session-1")
            && session_id.is_none()
            && matches!(view, super::TurnListViewArg::Full)
            && limit == 50
            && offset == 0
    ));
}

#[test]
fn parses_query_search_turns_literal_with_filters() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "search",
        "turns",
        "--project-id",
        "repo-abc123",
        "--mode",
        "literal",
        "--query",
        "--output-last-message",
        "--since",
        "5d",
        "--until",
        "2026-04-07T00:00:00Z",
        "--include-tool-output",
        "--field",
        "user-message",
        "--exclude-field",
        "tool_arguments",
        "--match-limit",
        "3",
    ])
    .unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            command: QueryCommands::Search(super::QuerySearchArgs {
                command: super::QuerySearchCommands::Turns(super::QuerySearchTurnsArgs {
                project_id,
                mode,
                query,
                query_arg,
                since,
                until,
                include_tool_output,
                fields,
                excluded_fields,
                match_limit,
                ..
                }),
            }),
            ..
        } if project_id.as_deref() == Some("repo-abc123")
            && matches!(mode, super::SearchModeArg::Literal)
            && query.as_deref() == Some("--output-last-message")
            && query_arg.is_none()
            && since.as_deref() == Some("5d")
            && until.as_deref() == Some("2026-04-07T00:00:00Z")
            && include_tool_output
            && fields == [super::SearchEvidenceField::UserMessage]
            && excluded_fields == [super::SearchEvidenceField::ToolArguments]
            && match_limit == Some(3)
    ));
}

#[test]
fn parses_query_search_turns_default_keyword_positional_query() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "search",
        "turns",
        "--project-id",
        "repo-abc123",
        "panic unwrap",
    ])
    .unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            command: QueryCommands::Search(super::QuerySearchArgs {
                command: super::QuerySearchCommands::Turns(super::QuerySearchTurnsArgs {
                    project_id,
                    mode,
                    query_arg,
                    query,
                    ..
                }),
            }),
            ..
        } if project_id.as_deref() == Some("repo-abc123")
            && matches!(mode, super::SearchModeArg::Keyword)
            && query_arg.as_deref() == Some("panic unwrap")
            && query.is_none()
    ));
}

#[test]
fn query_help_mentions_machine_protocol() {
    let help = help_for_command_path(&["query"]);

    assert!(help.contains("machine-readable"));
    assert!(help.contains("--color"));
    assert!(help.contains("Output:"));
    assert!(help.contains("darc query sessions --limit 5"));
}

#[test]
fn query_color_policy_respects_terminal_environment() {
    assert!(should_color_output(ColorArg::Auto, true, false, None));
    assert!(should_color_output(
        ColorArg::Auto,
        true,
        false,
        Some("xterm-256color"),
    ));
    assert!(!should_color_output(ColorArg::Auto, false, false, None));
    assert!(!should_color_output(ColorArg::Auto, true, true, None));
    assert!(!should_color_output(
        ColorArg::Auto,
        true,
        false,
        Some("dumb"),
    ));
    assert!(should_color_output(
        ColorArg::Always,
        false,
        true,
        Some("dumb"),
    ));
    assert!(!should_color_output(
        ColorArg::Never,
        true,
        false,
        Some("xterm-256color"),
    ));
}

#[test]
fn auto_color_policy_respects_terminal_environment() {
    assert!(should_auto_color_output(true, false, None));
    assert!(should_auto_color_output(
        true,
        false,
        Some("xterm-256color"),
    ));
    assert!(!should_auto_color_output(false, false, None));
    assert!(!should_auto_color_output(true, true, None));
    assert!(!should_auto_color_output(true, false, Some("dumb")));
}

#[test]
fn query_json_coloring_strips_to_original_json() {
    let json = "{\n  \"schema\": \"darc.query.workspace.v1\",\n  \"data\": {\n    \"count\": 1,\n    \"enabled\": true,\n    \"missing\": null,\n    \"escaped\": \"quote \\\" ok\"\n  }\n}";
    let colored = super::color_json(json);

    assert!(colored.contains("\x1b["));
    assert_eq!(strip_ansi_text(&colored), json);
}

/// Strips ANSI control sequences from rendered text for unit assertions.
fn strip_ansi_text(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for ch in chars.by_ref() {
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
        } else {
            output.push(ch);
        }
    }
    output
}

#[test]
fn query_workspace_help_hides_json_flag() {
    let mut command = Cli::command();
    let query = command
        .find_subcommand_mut("query")
        .expect("query subcommand should be present");
    let help = query
        .find_subcommand_mut("workspace")
        .expect("workspace query subcommand should be present")
        .render_long_help()
        .to_string();

    assert!(!help.contains("--json"));
}

#[test]
fn query_sessions_help_mentions_examples_for_time_bounds() {
    let mut command = Cli::command();
    let query = command
        .find_subcommand_mut("query")
        .expect("query subcommand should be present");
    let help = query
        .find_subcommand_mut("sessions")
        .expect("sessions query subcommand should be present")
        .render_long_help()
        .to_string();

    assert!(help.contains("--since"));
    assert!(help.contains("--until"));
    assert!(help.contains("--touched-path"));
    assert!(help.contains("5d"));
    assert!(help.contains("2026-04-07T00:00:00Z"));
    assert!(help.contains("current directory"));
}

#[test]
fn query_turn_help_mentions_narrative_view_behavior() {
    let mut command = Cli::command();
    let query = command
        .find_subcommand_mut("query")
        .expect("query subcommand should be present");
    let help = query
        .find_subcommand_mut("turn")
        .expect("turn query subcommand should be present")
        .render_long_help()
        .to_string();

    assert!(help.contains("--view"));
    assert!(help.contains("narrative"));
    assert!(help.contains("tool arguments"));
    assert!(help.contains("[SESSION_ID] [TURN_ORDINAL]"));
    assert!(!help.contains("[TURN_ORDINAL]..."));
    assert!(help.contains("required unless --session-id is set"));
    assert!(help.contains("required unless --turn-ordinal is set"));
}

#[test]
fn query_turns_help_omits_removed_grep_surface() {
    let mut command = Cli::command();
    let query = command
        .find_subcommand_mut("query")
        .expect("query subcommand should be present");
    let help = query
        .find_subcommand_mut("turns")
        .expect("turns query subcommand should be present")
        .render_long_help()
        .to_string();

    assert!(!help.contains("--grep"));
    assert!(help.contains("--view"));
    assert!(help.contains("oneline"));
    assert!(help.contains("--since"));
    assert!(help.contains("--until"));
    assert!(help.contains("required unless --session-id is set"));
}

#[test]
fn query_search_turns_help_mentions_tool_output_opt_in() {
    let help = help_for_command_path(&["query", "search", "turns"]);

    assert!(help.contains("--include-tool-output"));
    assert!(help.contains("--field"));
    assert!(help.contains("--exclude-field"));
    assert!(help.contains("--match-limit <MATCH_LIMIT>"));
    assert!(help.contains("Maximum nested matches per literal/regex turn hit [default: 20]"));
    assert!(help.contains("literal and regex"));
    assert!(help.contains("Accepted fields:"));
    assert!(help.contains("messages: user-message, final-answer"));
    assert!(help.contains("path-fragment"));
    assert!(help.contains("Workspace:"));
    assert!(help.contains("Scope:"));
    assert!(help.contains("Search:"));
    assert!(help.contains("Evidence:"));
    assert!(help.contains("Time Filters:"));
    assert!(help.contains("Result Size:"));
    assert!(help.contains("darc query search turns \"panic\" --limit 5"));
}

#[test]
fn query_files_help_mentions_path_and_co_touch_modes() {
    let help = help_for_command_path(&["query", "files"]);

    assert!(help.contains("--path"));
    assert!(help.contains("--co-touched-with"));
    assert!(help.contains("--limit"));
    assert!(help.contains("most-touched files"));
    assert!(help.contains("Selection:"));
    assert!(help.contains("Time Filters:"));
    assert!(help.contains("Result Size:"));
}

#[test]
fn parses_query_workspace_insights_command() {
    let cli =
        Cli::try_parse_from(["darc", "query", "insights", "workspace", "--window", "14d"]).unwrap();
    assert!(matches!(
        query_args(cli),
        super::QueryArgs {
            command: QueryCommands::Insights(super::QueryInsightsArgs {
                command: QueryInsightsCommands::Workspace(super::QueryWorkspaceInsightsArgs {
                    window_days,
                    recent_session_limit,
                    recent_session_offset,
                    ..
                }),
            }),
            ..
        } if window_days == 14
            && recent_session_limit == darc_core::query::DEFAULT_WORKSPACE_RECENT_SESSION_LIMIT
            && recent_session_offset == 0
    ));
}

#[test]
fn parses_window_days() {
    assert_eq!(parse_window_days("7d").unwrap(), 7);
    assert!(parse_window_days("0d").is_err());
    assert!(parse_window_days("weekly").is_err());
}

#[test]
fn resolves_relative_query_time_bounds() {
    let now = UNIX_EPOCH + Duration::from_secs(1_744_022_096);
    assert_eq!(
        resolve_query_time_bound_at("5d", now).unwrap(),
        "2025-04-02T10:34:56Z"
    );
}

#[test]
fn rejects_invalid_query_time_bounds() {
    let now = UNIX_EPOCH + Duration::from_secs(1_744_022_096);
    assert!(resolve_query_time_bound_at("weekly", now).is_err());
    assert!(resolve_query_time_bound_at("", now).is_err());
    assert!(resolve_query_time_bound_at("2026-99-99T00:00:00Z", now).is_err());
}

#[test]
fn formats_query_errors_as_json() {
    let payload = format_query_error(&anyhow!("boom"));
    let value: Value = serde_json::from_str(&payload).unwrap();

    assert_eq!(value["schema"], "darc.error.v1");
    assert_eq!(value["error"]["message"], "boom");
    assert!(value["error"]["code"].is_null());
    assert!(value["error"]["details"].is_null());
    assert!(value["generated_at"].as_str().unwrap().ends_with('Z'));
}

#[test]
fn formats_structured_query_errors_with_code_and_details() {
    let payload = format_query_error(
        &darc_core::query::QueryProtocolError::unknown_data_session("11111111", true).into(),
    );
    let value: Value = serde_json::from_str(&payload).unwrap();

    assert_eq!(value["schema"], "darc.error.v1");
    assert_eq!(value["error"]["code"], "unknown_session");
    assert_eq!(value["error"]["details"]["session"], "11111111");
    assert_eq!(value["error"]["details"]["looks_like_prefix"], true);
}

#[test]
fn formats_query_clap_errors_as_json() {
    let error = Cli::try_parse_from(["darc", "query", "workspace", "--json"]).unwrap_err();
    let payload = format_query_clap_error(&error);
    let value: Value = serde_json::from_str(&payload).unwrap();

    assert_eq!(value["schema"], "darc.error.v1");
    assert_eq!(value["error"]["code"], "invalid_arguments");
    assert_eq!(value["error"]["details"]["clap_kind"], "UnknownArgument");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("unexpected argument '--json'"))
    );
}

#[test]
fn codex_schema_audit_help_mentions_github_tokens() {
    let mut command = Cli::command();
    let help = command
        .find_subcommand_mut("codex-schema-audit")
        .expect("hidden subcommand should still be present")
        .render_long_help()
        .to_string();

    assert!(help.contains("GH_TOKEN"));
    assert!(help.contains("GITHUB_TOKEN"));
    assert!(help.contains("Personal access tokens are accepted."));
}

#[test]
fn claude_schema_audit_help_mentions_explicit_host_auth() {
    let mut command = Cli::command();
    let help = command
        .find_subcommand_mut("claude-schema-audit")
        .expect("hidden subcommand should still be present")
        .render_long_help()
        .to_string();

    assert!(help.contains("--use-host-auth"));
    assert!(help.contains("does not provide an OS-level sandbox"));
}

#[test]
fn formats_compatible_codex_schema_audit_report() {
    let report = compatible_report();
    let output = format_codex_schema_audit_report(&report);

    assert_eq!(codex_schema_audit_exit_code(&report), 0);
    assert!(output.contains("Status: compatible"));
    assert!(output.contains("Release Source: GitHub Releases (openai/codex)"));
    assert!(output.contains("Compatible across 1 audited stable release tag(s)."));
}

#[test]
fn formats_drift_codex_schema_audit_report() {
    let report = CodexSchemaAuditReport {
        outcome: CodexSchemaAuditOutcome::Drift(CodexSchemaDrift {
            first_drift_tag: "rust-v0.119.0".to_owned(),
            difference_summary: vec!["$/required: array length changed from 2 to 3".to_owned()],
            likely_files_to_update: vec![
                "crates/core/src/rollout/codex/version.rs".to_owned(),
                "crates/core/src/rollout/codex/mod.rs".to_owned(),
            ],
        }),
        ..compatible_report()
    };
    let output = format_codex_schema_audit_report(&report);

    assert_eq!(codex_schema_audit_exit_code(&report), 1);
    assert!(output.contains("Status: schema drift detected"));
    assert!(output.contains("First Drift Tag: rust-v0.119.0"));
    assert!(output.contains("Likely Darc Files To Update:"));
}

#[test]
fn formats_compatible_claude_schema_audit_report() {
    let report = compatible_claude_report();
    let output = format_claude_schema_audit_report(&report);

    assert_eq!(claude_schema_audit_exit_code(&report), 0);
    assert!(output.contains("Status: compatible"));
    assert!(output.contains("Latest Published Claude Version: 2.1.92"));
    assert!(output.contains("Compatible across 2 audited Claude version(s)."));
    assert!(output.contains("Supplementary Agent SDK Drift Version: 2.1.92"));
    assert!(output.contains("Sampling Stride: 1"));
    assert!(output.contains("Survey Mode: refine"));
    assert!(output.contains("Auth Mode: isolated (no auth)"));
}

#[test]
fn formats_drift_claude_schema_audit_report() {
    let report = ClaudeSchemaAuditReport {
        survey_mode: ClaudeSchemaSurveyMode::Coarse,
        transcript_drift_windows: vec![ClaudeSchemaDriftWindow {
            window_start_version: "2.1.88".to_owned(),
            window_end_version: "2.1.90".to_owned(),
            sampled_compatible_version: "2.1.87".to_owned(),
            sampled_drift_version: "2.1.90".to_owned(),
            difference_summary: vec!["$/line_types: array length changed from 4 to 5".to_owned()],
        }],
        outcome: ClaudeSchemaAuditOutcome::Drift(ClaudeSchemaDrift {
            first_drift_version: "2.1.90".to_owned(),
            difference_summary: vec![
                "$/line_types[2]: changed from \"system\" to \"mystery-event\"".to_owned(),
            ],
            likely_files_to_update: vec![
                "crates/core/src/rollout/claude/version.rs".to_owned(),
                "crates/core/src/rollout/claude/mod.rs".to_owned(),
            ],
        }),
        supplementary_sdk_drift: None,
        ..compatible_claude_report()
    };
    let output = format_claude_schema_audit_report(&report);

    assert_eq!(claude_schema_audit_exit_code(&report), 1);
    assert!(output.contains("Status: schema drift detected"));
    assert!(output.contains("First Drift Version: 2.1.90"));
    assert!(output.contains("Likely Darc Files To Update:"));
    assert!(output.contains("Survey Mode: coarse"));
    assert!(output.contains("Sampled Transcript Drift Windows:"));
}
