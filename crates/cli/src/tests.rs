use std::{
    path::PathBuf,
    time::{Duration, UNIX_EPOCH},
};

use anyhow::anyhow;
use clap::{CommandFactory, Parser};
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
    Cli, Commands, QueryCommands, QueryInsightsCommands, claude_schema_audit_exit_code,
    codex_schema_audit_exit_code, format_claude_schema_audit_report,
    format_codex_schema_audit_report, format_query_clap_error, format_query_error,
    parse_window_days, resolve_query_time_bound_at,
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
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Workspace(super::QueryWorkspaceArgs { .. }),
        })
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
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::ResolveSession(super::QueryResolveSessionArgs {
                input,
                project_id,
                provider,
                pick_one,
                ..
            }),
        }) if input == "11111111"
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
        cli.command,
        Commands::Query(super::QueryArgs {
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
        }) if project_id.as_deref() == Some("repo-abc123")
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
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Sessions(super::QuerySessionsArgs {
                project_id,
                provider,
                since,
                until,
                touched_path,
                ..
            }),
        }) if project_id.as_deref() == Some("repo-abc123")
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
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Sessions(super::QuerySessionsArgs {
                project_id,
                ..
            }),
        }) if project_id.is_none()
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
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Sessions(super::QuerySessionsArgs {
                project_id,
                touched_path,
                limit,
                offset,
                ..
            }),
        }) if project_id.as_deref() == Some("repo-abc123")
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
        cli.command,
        Commands::Query(super::QueryArgs {
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
        }) if project_id.as_deref() == Some("repo-abc123")
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
        cli.command,
        Commands::Query(super::QueryArgs {
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
        }) if project_id.as_deref() == Some("repo-abc123")
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
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::SessionFiles(super::QuerySessionFilesArgs {
                project_id,
                provider,
                session_id_arg,
                session_id,
                ..
            }),
        }) if project_id.as_deref() == Some("repo-abc123")
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
        cli.command,
        Commands::Query(super::QueryArgs {
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
        }) if project_id.as_deref() == Some("repo-abc123")
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
        cli.command,
        Commands::Query(super::QueryArgs {
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
        }) if project_id.as_deref() == Some("repo-abc123")
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
        cli.command,
        Commands::Query(super::QueryArgs {
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
        }) if project_id.as_deref() == Some("repo-abc123")
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
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Search(super::QuerySearchArgs {
                command: super::QuerySearchCommands::Turns(super::QuerySearchTurnsArgs {
                    project_id,
                    mode,
                    query_arg,
                    query,
                    ..
                }),
            }),
        }) if project_id.as_deref() == Some("repo-abc123")
            && matches!(mode, super::SearchModeArg::Keyword)
            && query_arg.as_deref() == Some("panic unwrap")
            && query.is_none()
    ));
}

#[test]
fn query_help_mentions_machine_protocol() {
    let mut command = Cli::command();
    let help = command
        .find_subcommand_mut("query")
        .expect("query subcommand should be present")
        .render_long_help()
        .to_string();

    assert!(help.contains("machine-readable"));
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
    let mut command = Cli::command();
    let query = command
        .find_subcommand_mut("query")
        .expect("query subcommand should be present");
    let search = query
        .find_subcommand_mut("search")
        .expect("search query subcommand should be present");
    let help = search
        .find_subcommand_mut("turns")
        .expect("turn search subcommand should be present")
        .render_long_help()
        .to_string();

    assert!(help.contains("--include-tool-output"));
    assert!(help.contains("--field"));
    assert!(help.contains("--exclude-field"));
    assert!(help.contains("--match-limit <MATCH_LIMIT>"));
    assert!(help.contains("Maximum nested matches per literal/regex turn hit [default: 20]"));
    assert!(help.contains("literal and regex"));
    assert!(help.contains("Accepted fields: user-message, final-answer"));
    assert!(help.contains("path-fragment"));
}

#[test]
fn query_files_help_mentions_path_and_co_touch_modes() {
    let mut command = Cli::command();
    let query = command
        .find_subcommand_mut("query")
        .expect("query subcommand should be present");
    let help = query
        .find_subcommand_mut("files")
        .expect("files query subcommand should be present")
        .render_long_help()
        .to_string();

    assert!(help.contains("--path"));
    assert!(help.contains("--co-touched-with"));
    assert!(help.contains("--limit"));
    assert!(help.contains("most-touched files"));
}

#[test]
fn parses_query_workspace_insights_command() {
    let cli =
        Cli::try_parse_from(["darc", "query", "insights", "workspace", "--window", "14d"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Insights(super::QueryInsightsArgs {
                command: QueryInsightsCommands::Workspace(super::QueryWorkspaceInsightsArgs {
                    window_days,
                    recent_session_limit,
                    recent_session_offset,
                    ..
                }),
            }),
        }) if window_days == 14
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
