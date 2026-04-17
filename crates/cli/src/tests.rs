use std::time::{Duration, UNIX_EPOCH};

use anyhow::anyhow;
use clap::{CommandFactory, Parser};
use darc_rollout_audit::claude::{
    ClaudeSchemaAuditReport, ClaudeSchemaDrift, ClaudeSchemaDriftWindow, ClaudeSchemaSurveyMode,
    ClaudeSdkSchemaDrift,
};
use darc_rollout_audit::codex::{CodexSchemaAuditReport, CodexSchemaDrift};
use darc_rollout_audit::{claude::ClaudeSchemaAuditOutcome, codex::CodexSchemaAuditOutcome};
use serde_json::Value;

use super::{
    Cli, Commands, QueryCommands, QueryInsightsCommands, QueryWikiCommands, WikiCommands,
    WikiDigestCommands, claude_schema_audit_exit_code, codex_schema_audit_exit_code,
    format_claude_schema_audit_report, format_codex_schema_audit_report, format_query_error,
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
    let cli = Cli::try_parse_from(["darc", "query", "workspace", "--json"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Workspace(super::QueryWorkspaceArgs { json, .. }),
        }) if json
    ));
}

#[test]
fn parses_query_resolve_session_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "resolve-session",
        "11111111",
        "--provider",
        "codex",
        "--pick-one",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::ResolveSession(super::QueryResolveSessionArgs {
                input,
                provider,
                pick_one,
                json,
                ..
            }),
        }) if input == "11111111"
            && matches!(provider, Some(super::ProviderArg::Codex))
            && pick_one
            && json
    ));
}

#[test]
fn query_workspace_requires_json_flag() {
    let error = Cli::try_parse_from(["darc", "query", "workspace"]).unwrap_err();

    assert!(error.to_string().contains("--json"));
}

#[test]
fn parses_query_wiki_registry_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "wiki",
        "registry",
        "--project-id",
        "repo-abc123",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Wiki(super::QueryWikiArgs {
                command: QueryWikiCommands::Registry(super::QueryWikiRegistryArgs {
                    project_id,
                    json,
                    ..
                }),
            }),
        }) if project_id == "repo-abc123" && json
    ));
}

#[test]
fn query_wiki_registry_requires_json_flag() {
    let error = Cli::try_parse_from([
        "darc",
        "query",
        "wiki",
        "registry",
        "--project-id",
        "repo-abc123",
    ])
    .unwrap_err();

    assert!(error.to_string().contains("--json"));
}

#[test]
fn parses_query_wiki_entries_with_filters() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "wiki",
        "entries",
        "--project-id",
        "repo-abc123",
        "--category",
        "product",
        "--domain",
        "query",
        "--status",
        "active",
        "--grep",
        "staged init",
        "--evidence-ref",
        "codex:session-1#4",
        "--evidence-ref",
        "claude:session-2#1",
        "--covers-session",
        "codex:session-1",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Wiki(super::QueryWikiArgs {
                command: QueryWikiCommands::Entries(super::QueryWikiEntriesArgs {
                    project_id,
                    category,
                    domain,
                    status,
                    grep,
                    evidence_ref,
                    covers_session,
                    json,
                    ..
                }),
            }),
        }) if project_id == "repo-abc123"
            && category.as_deref() == Some("product")
            && domain.as_deref() == Some("query")
            && matches!(status, Some(super::WikiEntryStatusArg::Active))
            && grep.as_deref() == Some("staged init")
            && evidence_ref == vec!["codex:session-1#4".to_owned(), "claude:session-2#1".to_owned()]
            && covers_session == vec!["codex:session-1".to_owned()]
            && json
    ));
}

#[test]
fn parses_query_wiki_entry_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "wiki",
        "entry",
        "--project-id",
        "repo-abc123",
        "--entry-id",
        "cw_01entry",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Wiki(super::QueryWikiArgs {
                command: QueryWikiCommands::Entry(super::QueryWikiEntryArgs {
                    project_id,
                    entry_id,
                    json,
                    ..
                }),
            }),
        }) if project_id == "repo-abc123" && entry_id == "cw_01entry" && json
    ));
}

#[test]
fn parses_query_wiki_digest_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "wiki",
        "digest",
        "--project-id",
        "repo-abc123",
        "--digest-id",
        "dg_01digest",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Wiki(super::QueryWikiArgs {
                command: QueryWikiCommands::Digest(super::QueryWikiDigestArgs {
                    project_id,
                    digest_id,
                    json,
                    ..
                }),
            }),
        }) if project_id == "repo-abc123" && digest_id == "dg_01digest" && json
    ));
}

#[test]
fn parses_query_wiki_digests_with_time_bounds() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "wiki",
        "digests",
        "--project-id",
        "repo-abc123",
        "--since",
        "30d",
        "--until",
        "2026-04-07T00:00:00Z",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Wiki(super::QueryWikiArgs {
                command: QueryWikiCommands::Digests(super::QueryWikiDigestsArgs {
                    project_id,
                    since,
                    until,
                    json,
                    ..
                }),
            }),
        }) if project_id == "repo-abc123"
            && since.as_deref() == Some("30d")
            && until.as_deref() == Some("2026-04-07T00:00:00Z")
            && json
    ));
}

#[test]
fn parses_query_wiki_runs_with_status_and_limit() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "wiki",
        "runs",
        "--project-id",
        "repo-abc123",
        "--status",
        "running",
        "--since",
        "7d",
        "--until",
        "2026-04-07T00:00:00Z",
        "--limit",
        "5",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Wiki(super::QueryWikiArgs {
                command: QueryWikiCommands::Runs(super::QueryWikiRunsArgs {
                    project_id,
                    status,
                    since,
                    until,
                    limit,
                    json,
                    ..
                }),
            }),
        }) if project_id == "repo-abc123"
            && matches!(status, Some(super::WikiRunStatusArg::Running))
            && since.as_deref() == Some("7d")
            && until.as_deref() == Some("2026-04-07T00:00:00Z")
            && limit == Some(5)
            && json
    ));
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
        "--session-id",
        "session-1",
        "--turn-ordinal",
        "2",
        "--view",
        "narrative",
        "--include-raw",
        "--include-insights",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Turn(super::QueryTurnArgs {
                project_id,
                session_id,
                turn_ordinal,
                view,
                include_raw,
                include_insights,
                json,
                ..
            }),
        }) if project_id == "repo-abc123"
            && session_id == "session-1"
            && turn_ordinal == 2
            && matches!(view, super::ViewArg::Narrative)
            && include_raw
            && include_insights
            && json
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
        "--since",
        "5d",
        "--until",
        "2026-04-07T00:00:00Z",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Sessions(super::QuerySessionsArgs {
                project_id,
                since,
                until,
                touched_path,
                json,
                ..
            }),
        }) if project_id == "repo-abc123"
            && since.as_deref() == Some("5d")
            && until.as_deref() == Some("2026-04-07T00:00:00Z")
            && touched_path.is_none()
            && json
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
        "crates/wiki/**",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Sessions(super::QuerySessionsArgs {
                project_id,
                touched_path,
                json,
                ..
            }),
        }) if project_id == "repo-abc123"
            && touched_path.as_deref() == Some("crates/wiki/**")
            && json
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
        "--path",
        "crates/wiki/**/*.rs",
        "--since",
        "30d",
        "--until",
        "2026-04-07T00:00:00Z",
        "--json",
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
                json,
                ..
            }),
        }) if project_id == "repo-abc123"
            && path.as_deref() == Some("crates/wiki/**/*.rs")
            && co_touched_with.is_none()
            && since.as_deref() == Some("30d")
            && until.as_deref() == Some("2026-04-07T00:00:00Z")
            && limit.is_none()
            && json
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
        "crates/wiki/src/proposal.rs",
        "--limit",
        "10",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Files(super::QueryFilesArgs {
                project_id,
                path,
                co_touched_with,
                limit,
                json,
                ..
            }),
        }) if project_id == "repo-abc123"
            && path.is_none()
            && co_touched_with.as_deref() == Some("crates/wiki/src/proposal.rs")
            && limit == Some(10)
            && json
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
        "--session-id",
        "session-1",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::SessionFiles(super::QuerySessionFilesArgs {
                project_id,
                provider,
                session_id,
                json,
                ..
            }),
        }) if project_id == "repo-abc123"
            && matches!(provider, super::ProviderArg::Codex)
            && session_id == "session-1"
            && json
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
        "--session-id",
        "session-1",
        "--view",
        "narrative",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::SessionBundle(super::QuerySessionBundleArgs {
                project_id,
                provider,
                session_id,
                view,
                json,
                ..
            }),
        }) if project_id == "repo-abc123"
            && matches!(provider, super::ProviderArg::Codex)
            && session_id == "session-1"
            && matches!(view, super::ViewArg::Narrative)
            && json
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
        "--session-id",
        "session-1",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Turns(super::QueryTurnsArgs {
                project_id,
                provider,
                session_id,
                grep,
                role,
                context,
                view,
                json,
                ..
            }),
        }) if project_id == "repo-abc123"
            && matches!(provider, Some(super::ProviderArg::Codex))
            && session_id.as_deref() == Some("session-1")
            && grep.is_none()
            && matches!(role, super::TurnSearchRoleArg::Both)
            && context == 0
            && matches!(view, super::TurnListViewArg::Full)
            && json
    ));
}

#[test]
fn parses_query_turns_grep_with_context_and_filters() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "turns",
        "--project-id",
        "repo-abc123",
        "--grep",
        "staged init",
        "--role",
        "user",
        "--context",
        "1",
        "--since",
        "5d",
        "--until",
        "2026-04-07T00:00:00Z",
        "--view",
        "oneline",
        "--touched-path",
        "crates/index/**",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Turns(super::QueryTurnsArgs {
                project_id,
                grep,
                role,
                context,
                since,
                until,
                view,
                touched_path,
                json,
                ..
            }),
        }) if project_id == "repo-abc123"
            && grep.as_deref() == Some("staged init")
            && matches!(role, super::TurnSearchRoleArg::User)
            && context == 1
            && since.as_deref() == Some("5d")
            && until.as_deref() == Some("2026-04-07T00:00:00Z")
            && matches!(view, super::TurnListViewArg::Oneline)
            && touched_path.as_deref() == Some("crates/index/**")
            && json
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
fn query_workspace_help_mentions_json_flag() {
    let mut command = Cli::command();
    let query = command
        .find_subcommand_mut("query")
        .expect("query subcommand should be present");
    let help = query
        .find_subcommand_mut("workspace")
        .expect("workspace query subcommand should be present")
        .render_long_help()
        .to_string();

    assert!(help.contains("--json"));
}

#[test]
fn query_wiki_help_mentions_registry_subcommand() {
    let mut command = Cli::command();
    let query = command
        .find_subcommand_mut("query")
        .expect("query subcommand should be present");
    let help = query
        .find_subcommand_mut("wiki")
        .expect("wiki query subcommand should be present")
        .render_long_help()
        .to_string();

    assert!(help.contains("registry"));
    assert!(help.contains("entry"));
    assert!(help.contains("entries"));
    assert!(help.contains("digest"));
    assert!(help.contains("digests"));
    assert!(help.contains("runs"));
}

#[test]
fn query_wiki_entries_help_mentions_q4_filters() {
    let mut command = Cli::command();
    let query = command
        .find_subcommand_mut("query")
        .expect("query subcommand should be present");
    let help = query
        .find_subcommand_mut("wiki")
        .expect("wiki query subcommand should be present")
        .find_subcommand_mut("entries")
        .expect("wiki entries query subcommand should be present")
        .render_long_help()
        .to_string();

    assert!(help.contains("--grep"));
    assert!(help.contains("--evidence-ref"));
    assert!(help.contains("--covers-session"));
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
}

#[test]
fn query_turns_help_mentions_grep_context_and_touched_path() {
    let mut command = Cli::command();
    let query = command
        .find_subcommand_mut("query")
        .expect("query subcommand should be present");
    let help = query
        .find_subcommand_mut("turns")
        .expect("turns query subcommand should be present")
        .render_long_help()
        .to_string();

    assert!(help.contains("--grep"));
    assert!(help.contains("--role"));
    assert!(help.contains("--context"));
    assert!(help.contains("--view"));
    assert!(help.contains("oneline"));
    assert!(help.contains("--since"));
    assert!(help.contains("--until"));
    assert!(help.contains("--touched-path"));
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
}

#[test]
fn parses_query_workspace_insights_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "query",
        "insights",
        "workspace",
        "--window",
        "14d",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Query(super::QueryArgs {
            command: QueryCommands::Insights(super::QueryInsightsArgs {
                command: QueryInsightsCommands::Workspace(super::QueryWorkspaceInsightsArgs {
                    window_days,
                    json,
                    ..
                }),
            }),
        }) if window_days == 14 && json
    ));
}

#[test]
fn parses_wiki_digest_start_skeleton_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "wiki",
        "digest",
        "start",
        "--project-id",
        "repo-abc123",
        "--session-ref",
        "codex:session-1",
        "--agent",
        "codex",
        "--runtime",
        "external-cli",
        "--model",
        "gpt-5.4",
        "--auth-profile",
        "openai/default",
        "--target-category",
        "architecture",
        "--target-domain",
        "storage",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Wiki(super::WikiArgs {
            command: WikiCommands::Digest(super::WikiDigestArgs {
                command: WikiDigestCommands::Start(super::WikiDigestStartArgs {
                    project_id,
                    session_ref,
                    auth_profile,
                    target_category,
                    target_domain,
                    json,
                    ..
                }),
            }),
        }) if project_id == "repo-abc123"
            && session_ref == vec!["codex:session-1".to_owned()]
            && auth_profile.as_deref() == Some("openai/default")
            && target_category == vec!["architecture".to_owned()]
            && target_domain == vec!["storage".to_owned()]
            && json
    ));
}

#[test]
fn parses_wiki_digest_cancel_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "wiki",
        "digest",
        "cancel",
        "--project-id",
        "repo-abc123",
        "--run-id",
        "cwrun_01abcd",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Wiki(super::WikiArgs {
            command: WikiCommands::Digest(super::WikiDigestArgs {
                command: WikiDigestCommands::Cancel(super::WikiDigestCancelArgs {
                    project_id,
                    run_id,
                    json,
                    ..
                }),
            }),
        }) if project_id == "repo-abc123" && run_id == "cwrun_01abcd" && json
    ));
}

#[test]
fn parses_wiki_entry_discard_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "wiki",
        "entry",
        "discard",
        "--project-id",
        "repo-abc123",
        "--entry-id",
        "cw_01abcd",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Wiki(super::WikiArgs {
            command: WikiCommands::Entry(super::WikiEntryArgs {
                command: super::WikiEntryCommands::Discard(super::WikiEntryDiscardArgs {
                    args: super::WikiEntryMutationArgs {
                        project_id,
                        entry_id,
                        json,
                        ..
                    },
                }),
            }),
        }) if project_id == "repo-abc123" && entry_id == "cw_01abcd" && json
    ));
}

#[test]
fn parses_wiki_entry_restore_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "wiki",
        "entry",
        "restore",
        "--project-id",
        "repo-abc123",
        "--entry-id",
        "cw_01abcd",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Wiki(super::WikiArgs {
            command: WikiCommands::Entry(super::WikiEntryArgs {
                command: super::WikiEntryCommands::Restore(super::WikiEntryRestoreArgs {
                    args: super::WikiEntryMutationArgs {
                        project_id,
                        entry_id,
                        json,
                        ..
                    },
                }),
            }),
        }) if project_id == "repo-abc123" && entry_id == "cw_01abcd" && json
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
