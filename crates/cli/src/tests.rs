use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use clap::{ColorChoice, Parser};
use darc_core::{
    IndexReport, RefreshAllBestEffortReport, RefreshProgress, RefreshProjectAttempt,
    RefreshProjectFailure, RefreshReport, SourceKind, SyncReport,
    config::{ClaudeSourceConfig, CodexSourceConfig, SharedConfig, SourcesConfig, WatchConfig},
};
use darc_rollout_audit::claude::{
    ClaudeSchemaAuditFailure, ClaudeSchemaAuditReport, ClaudeSchemaDrift,
    ClaudeSchemaDriftBoundaryPrecision, ClaudeSchemaDriftWindow, ClaudeSchemaSurveyMode,
    ClaudeSdkSchemaDrift,
};
use darc_rollout_audit::codex::{CodexSchemaAuditReport, CodexSchemaDrift};
use darc_rollout_audit::{claude::ClaudeSchemaAuditOutcome, codex::CodexSchemaAuditOutcome};
use darc_test_utils::{unique_test_dir, write_file};
use serde_json::Value;

use super::{
    Cli, ColorArg, Commands, HELP_STYLES, claude_schema_audit_exit_code, cli_command,
    codex_schema_audit_exit_code, format_claude_schema_audit_report,
    format_codex_schema_audit_report, format_query_clap_error, format_query_error,
    parse_window_days, release_version_is_newer, render_agent_help_guide, render_agents_md_line,
    resolve_query_time_bound_at, should_auto_color_output, should_check_upgrade_nudge,
    should_color_output, should_notify_upgrade_nudge, upgrade_nudge_enabled,
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

/// Renders long help for one nested command path.
fn help_for_command_path(path: &[&str]) -> String {
    let mut command = cli_command();
    let mut current = &mut command;
    for name in path {
        current = current
            .find_subcommand_mut(name)
            .unwrap_or_else(|| panic!("subcommand `{name}` should be present"));
    }
    current.render_long_help().to_string()
}

/// Asserts that the given help sections appear in the expected order.
fn assert_contains_in_order(haystack: &str, needles: &[&str]) {
    let mut cursor = 0;
    for needle in needles {
        let tail = &haystack[cursor..];
        let Some(offset) = tail.find(needle) else {
            panic!("expected `{needle}` after byte {cursor} in:\n{haystack}");
        };
        cursor += offset + needle.len();
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
        compatible_inspected_versions: vec!["2.1.92".to_owned(), "2.1.87".to_owned()],
        failed_versions: Vec::new(),
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
    let help = cli_command().render_long_help().to_string();

    assert!(help.contains("Archive, index, and query coding-agent sessions"));
    assert!(help.contains("Agents can run `darc agent-help` for usage guidance."));
    assert!(help.contains("Common workflows:"));
    assert!(help.contains(
        "darc status                                      # check active-project health"
    ));
    assert!(
        help.contains(
            "darc agent-help                                  # show agent usage guidance"
        )
    );
    assert!(help.contains(
        "darc refresh --auto                              # keep Darc fresh automatically on macOS"
    ));
    assert!(help.contains(
        "darc refresh                                     # refresh once without background jobs"
    ));
    assert!(
        help.contains("darc search \"panic\" --limit 5                    # find matching turns")
    );
    assert!(help.contains("darc show session <SESSION_ID> --turn-limit 5"));
    assert!(help.contains(
        "darc upgrade --check                             # check for a newer CLI release"
    ));
    assert!(help.contains("darc help <command>"));
    let workflow_comment_columns: Vec<usize> = help
        .lines()
        .filter(|line| line.trim_start().starts_with("darc "))
        .filter_map(|line| line.find('#'))
        .collect();
    assert!(!workflow_comment_columns.is_empty());
    assert!(
        workflow_comment_columns
            .iter()
            .all(|column| *column == workflow_comment_columns[0])
    );
    assert!(help.contains("  search "));
    assert!(help.contains("  agent-help "));
    assert!(help.contains("  project "));
    assert!(!help.contains("  query "));
    assert!(!help.contains("  link "));
    assert!(!help.contains("  remove "));
    assert!(!help.contains("  rename-from "));
}

#[test]
fn parses_agent_help_command() {
    let guide = Cli::try_parse_from(["darc", "agent-help"]).unwrap();
    assert!(matches!(
        guide.command,
        Commands::AgentHelp(super::AgentHelpArgs {
            agents_md_line: false
        })
    ));

    let line = Cli::try_parse_from(["darc", "agent-help", "--agents-md-line"]).unwrap();
    assert!(matches!(
        line.command,
        Commands::AgentHelp(super::AgentHelpArgs {
            agents_md_line: true
        })
    ));
}

#[test]
fn agent_help_renders_operating_guide() {
    let guide = render_agent_help_guide();

    assert_contains_in_order(
        guide,
        &[
            "# Darc Agent Help",
            "## When to Use Darc",
            "## Safe First Commands",
            "## Task Recipes",
            "## Evidence Ladder",
            "## Reporting Darc Evidence",
            "## Output Discipline",
            "## Mutating Boundaries",
        ],
    );
    assert!(guide.contains("`darc status --json`"));
    assert!(guide.contains("`darc search --mode file-path <glob> --limit 5`"));
    assert!(guide.contains("`darc list files --co-touched-with <path> --limit 10`"));
    assert!(guide.contains("Treat historical churn as a map, not a verdict."));
    assert!(guide.contains("Darc showed:"));
    assert!(guide.contains("Current source/tests confirmed:"));
    assert!(guide.contains("Do not list Darc commands unless they help reproduce the evidence."));
    assert!(guide.contains("`darc refresh`, `darc sync`, `darc index`"));
    assert!(!guide.contains("AGENTS.md trigger"));
    assert!(!guide.contains("darc agent-help --agents-md-line >> AGENTS.md"));
}

#[test]
fn agents_md_line_is_single_marker_wrapped_line() {
    let line = render_agents_md_line();

    assert_eq!(
        line,
        "<!-- darc:agent-help:start --> When a task depends on prior decisions, regressions, repeated failures, PR handoffs, ambiguous references to earlier work, or file/module history, run `darc agent-help` and use Darc for exact prior-session evidence. Verify conclusions against current files and tests. <!-- darc:agent-help:end -->"
    );
    assert_eq!(line.lines().count(), 1);
    assert!(line.starts_with("<!-- darc:agent-help:start --> "));
    assert!(line.ends_with(" <!-- darc:agent-help:end -->"));
}

#[test]
fn help_uses_terminal_color_auto() {
    let mut command = cli_command();
    command.build();

    assert_eq!(command.get_color(), ColorChoice::Auto);
    assert_eq!(
        *command.get_styles().get_header(),
        *HELP_STYLES.get_header()
    );

    let search = command
        .find_subcommand("search")
        .expect("search subcommand should be present");
    assert_eq!(*search.get_styles().get_header(), *HELP_STYLES.get_header());
}

#[test]
fn rendered_help_is_plain_but_carries_ansi_styles() {
    let styled = cli_command().render_long_help();
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
fn refresh_command_accepts_auto_mode() {
    let cli = Cli::try_parse_from(["darc", "refresh", "--auto"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::Refresh(super::RefreshArgs { auto: true, .. })
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
fn refresh_help_mentions_auto_mode() {
    let help = help_for_command_path(&["refresh"]);

    assert!(help.contains("Use `--auto` to enable automatic background refresh and start it now."));
    assert!(help.contains("--auto"));
    assert!(help.contains("Enable automatic background refresh for all projects and start it now"));
}

#[test]
fn upgrade_command_accepts_check_and_json() {
    let cli = Cli::try_parse_from(["darc", "upgrade", "--check", "--json"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::Upgrade(super::UpgradeArgs {
            check: true,
            json: true,
            ..
        })
    ));
}

#[test]
fn upgrade_json_requires_check_mode() {
    let error = Cli::try_parse_from(["darc", "upgrade", "--json"]).unwrap_err();
    assert_eq!(
        error.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
}

#[test]
fn upgrade_json_parse_errors_use_json_surface() {
    assert!(super::is_json_read_invocation(&[
        OsString::from("darc"),
        OsString::from("upgrade"),
        OsString::from("--json"),
    ]));
    assert!(!super::is_json_read_invocation(&[
        OsString::from("darc"),
        OsString::from("upgrade"),
        OsString::from("--check"),
    ]));
}

#[test]
fn upgrade_json_parse_error_usage_mentions_required_check_mode() {
    let args = [
        OsString::from("darc"),
        OsString::from("upgrade"),
        OsString::from("--json"),
        OsString::from("--bad"),
    ];
    let error = Cli::try_parse_from(&args).unwrap_err();

    let payload = super::format_json_clap_error(&error, &args);
    let value: Value = serde_json::from_str(&payload).unwrap();
    let message = value["error"]["message"].as_str().unwrap();

    assert!(message.contains("Usage: darc upgrade --check --json"));
    assert!(!message.contains("Usage: darc upgrade --json"));
}

#[test]
fn upgrade_dismiss_command_accepts_version_and_root() {
    let cli = Cli::try_parse_from([
        "darc",
        "upgrade",
        "--root",
        "/tmp/darc-root",
        "dismiss",
        "0.2.0",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Upgrade(super::UpgradeArgs {
            root,
            command: Some(super::UpgradeCommands::Dismiss(super::UpgradeDismissArgs {
                version: Some(version),
            })),
            ..
        }) if root == Path::new("/tmp/darc-root") && version == "0.2.0"
    ));

    let cli = Cli::try_parse_from([
        "darc",
        "upgrade",
        "dismiss",
        "--root",
        "/tmp/darc-root",
        "0.2.0",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Upgrade(super::UpgradeArgs {
            root,
            command: Some(super::UpgradeCommands::Dismiss(super::UpgradeDismissArgs {
                version: Some(version),
            })),
            ..
        }) if root == Path::new("/tmp/darc-root") && version == "0.2.0"
    ));
}

#[test]
fn upgrade_dismiss_normalizes_v_prefixed_version() -> Result<()> {
    let root = unique_test_dir("upgrade-dismiss");

    super::run_upgrade_dismiss(
        &root,
        super::UpgradeDismissArgs {
            version: Some("v0.2.0".to_owned()),
        },
    )?;

    let cache: Value =
        serde_json::from_str(&fs::read_to_string(root.join("run/upgrade-check.json"))?)?;
    assert_eq!(cache["dismissed_version"], "0.2.0");

    Ok(())
}

#[test]
fn manual_upgrade_installer_command_targets_current_install_dir() {
    let command = super::manual_upgrade_installer_command_for_dir(Path::new("/tmp/darc bin"));
    assert_eq!(
        command,
        "curl -fsSL https://github.com/0xjunha/darc/releases/latest/download/darc-installer.sh | DARC_INSTALL_DIR='/tmp/darc bin' sh"
    );

    let quoted = super::manual_upgrade_installer_command_for_dir(Path::new("/tmp/darc'bin"));
    assert_eq!(
        quoted,
        "curl -fsSL https://github.com/0xjunha/darc/releases/latest/download/darc-installer.sh | DARC_INSTALL_DIR='/tmp/darc'\\''bin' sh"
    );
}

#[test]
fn upgrade_check_json_uses_current_install_command() {
    let status = super::UpgradeStatus {
        current_version: "0.1.0".to_owned(),
        latest_version: Some("0.2.0".to_owned()),
        upgrade_available: true,
        latest_release_url: Some("https://github.com/0xjunha/darc/releases/tag/v0.2.0".to_owned()),
    };

    let payload = super::UpgradeCheckJson::from(&status);

    assert_eq!(
        payload.install_command,
        super::manual_upgrade_installer_command()
    );
}

#[test]
fn upgrade_http_error_detail_is_bounded_and_single_line() {
    let detail = super::upgrade_http_error_detail("  first line\nsecond\tline  ").unwrap();
    assert_eq!(detail, "first line second line");

    let detail = super::upgrade_http_error_detail(&"a".repeat(300)).unwrap();
    assert_eq!(detail.chars().count(), super::UPGRADE_ERROR_BODY_LIMIT);
    assert!(detail.ends_with("..."));
    assert!(super::upgrade_http_error_detail(" \n\t ").is_none());
}

#[test]
fn passive_upgrade_headers_do_not_attach_github_token() -> Result<()> {
    let passive =
        super::build_upgrade_headers(super::UpgradeCheckAuth::Anonymous, Some("secret-token"))?;
    assert!(!passive.contains_key(super::AUTHORIZATION));

    let explicit = super::build_upgrade_headers(
        super::UpgradeCheckAuth::IncludeGitHubToken,
        Some("secret-token"),
    )?;
    assert_eq!(
        explicit
            .get(super::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer secret-token")
    );

    Ok(())
}

#[test]
fn refresh_watch_options_require_watch_mode() {
    for flag in [
        "--debounce",
        "--min-interval",
        "--reconcile-interval",
        "--poll",
    ] {
        let mut args = vec!["darc", "refresh", flag];
        if flag != "--poll" {
            args.push("30s");
        }
        let error = Cli::try_parse_from(args).unwrap_err();

        assert!(error.to_string().contains("--watch"));
    }
}

#[test]
fn refresh_auto_conflicts_with_refresh_selection_and_watch_options() {
    for args in [
        vec!["darc", "refresh", "--auto", "--provider", "claude"],
        vec!["darc", "refresh", "--auto", "--all"],
        vec!["darc", "refresh", "--auto", "--watch"],
        vec!["darc", "refresh", "--auto", "--debounce", "30s"],
        vec!["darc", "refresh", "--auto", "--min-interval", "60s"],
        vec!["darc", "refresh", "--auto", "--reconcile-interval", "10m"],
        vec!["darc", "refresh", "--auto", "--poll"],
    ] {
        let error = Cli::try_parse_from(args).unwrap_err();

        assert!(error.to_string().contains("--auto"));
    }
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
fn macos_service_start_launchctl_args_reload_loaded_service() {
    let args = super::macos_service_start_launchctl_args(
        Path::new("/tmp/darc.plist"),
        true,
        "gui/501".to_owned(),
        "gui/501/com.0xjunha.darc.refresh".to_owned(),
    );

    assert_eq!(
        args,
        vec![
            vec![
                "bootout".to_owned(),
                "gui/501/com.0xjunha.darc.refresh".to_owned()
            ],
            vec![
                "bootstrap".to_owned(),
                "gui/501".to_owned(),
                "/tmp/darc.plist".to_owned()
            ],
            vec![
                "kickstart".to_owned(),
                "-k".to_owned(),
                "gui/501/com.0xjunha.darc.refresh".to_owned()
            ],
        ]
    );
}

#[test]
fn macos_service_start_launchctl_args_load_unloaded_service() {
    let args = super::macos_service_start_launchctl_args(
        Path::new("/tmp/darc.plist"),
        false,
        "gui/501".to_owned(),
        "gui/501/com.0xjunha.darc.refresh".to_owned(),
    );

    assert_eq!(
        args,
        vec![
            vec![
                "bootstrap".to_owned(),
                "gui/501".to_owned(),
                "/tmp/darc.plist".to_owned()
            ],
            vec![
                "kickstart".to_owned(),
                "-k".to_owned(),
                "gui/501/com.0xjunha.darc.refresh".to_owned()
            ],
        ]
    );
}

#[test]
fn service_help_marks_feature_beta_and_macos_only() {
    let help = help_for_command_path(&["service"]);

    assert!(help.contains("beta background Darc refresh service"));
    assert!(help.contains("currently beta and supports macOS LaunchAgents only"));
    assert!(help.contains("Workspace:"));
    assert!(help.contains("Use this Darc root instead of the default"));
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
fn watched_refresh_waits_for_min_interval_after_debounce() {
    let settings = sample_watch_settings(
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(600),
    );
    let now = Instant::now();

    assert!(!super::should_run_watched_refresh(
        Some(now - Duration::from_secs(45)),
        Some(now - Duration::from_secs(59)),
        now,
        &settings
    ));
    assert_eq!(
        super::next_watch_refresh(
            Some(now - Duration::from_secs(45)),
            Some(now - Duration::from_secs(59)),
            now,
            &settings
        ),
        None
    );
}

#[test]
fn watched_refresh_runs_when_debounce_and_min_interval_are_ready() {
    let settings = sample_watch_settings(
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(600),
    );
    let now = Instant::now();

    assert_eq!(
        super::next_watch_refresh(
            Some(now - Duration::from_secs(30)),
            Some(now - Duration::from_secs(60)),
            now,
            &settings
        ),
        Some(super::WatchRefreshReason::Change)
    );
}

#[test]
fn reconcile_refresh_preempts_stale_dirty_event() {
    let settings = sample_watch_settings(
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(600),
    );
    let now = Instant::now();

    assert_eq!(
        super::next_watch_refresh(
            Some(now - Duration::from_secs(45)),
            Some(now - Duration::from_secs(600)),
            now,
            &settings
        ),
        Some(super::WatchRefreshReason::Reconcile)
    );
}

#[test]
fn watch_timeout_uses_reconcile_deadline_without_dirty_event() {
    let settings = sample_watch_settings(
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(600),
    );
    let now = Instant::now();

    assert_eq!(
        super::watch_loop_timeout_at(now, None, Some(now - Duration::from_secs(590)), &settings),
        Duration::from_secs(10)
    );
}

#[test]
fn watch_timeout_uses_earlier_change_deadline_before_reconcile() {
    let settings = sample_watch_settings(
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(600),
    );
    let now = Instant::now();

    assert_eq!(
        super::watch_loop_timeout_at(
            now,
            Some(now - Duration::from_secs(20)),
            Some(now - Duration::from_secs(300)),
            &settings
        ),
        Duration::from_secs(10)
    );
}

#[test]
fn watch_timeout_extends_dirty_deadline_to_min_interval() {
    let settings = sample_watch_settings(
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(600),
    );
    let now = Instant::now();

    assert_eq!(
        super::watch_loop_timeout_at(
            now,
            Some(now - Duration::from_secs(45)),
            Some(now - Duration::from_secs(50)),
            &settings
        ),
        Duration::from_secs(10)
    );
}

#[test]
fn watch_timeout_is_zero_when_no_refresh_has_run() {
    let settings = sample_watch_settings(
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(600),
    );
    let now = Instant::now();

    assert_eq!(
        super::watch_loop_timeout_at(now, None, None, &settings),
        Duration::ZERO
    );
}

#[test]
fn parse_duration_rejects_zero_duration() {
    let error = super::parse_duration("0s").unwrap_err();

    assert!(format!("{error:#}").contains("duration must be greater than zero"));
}

#[test]
fn parse_duration_rejects_unsupported_unit() {
    let error = super::parse_duration("1d").unwrap_err();

    assert!(format!("{error:#}").contains("unsupported duration unit"));
}

#[test]
fn load_watch_settings_uses_defaults_and_all_existing_sources() -> Result<()> {
    let fixture = WatchConfigFixture::new("watch-defaults")?;

    let settings =
        super::load_watch_settings(&fixture.root, &[], &super::WatchOverrides::default())?;

    assert_eq!(settings.debounce, super::DEFAULT_WATCH_DEBOUNCE);
    assert_eq!(settings.min_interval, super::DEFAULT_WATCH_MIN_INTERVAL);
    assert_eq!(
        settings.reconcile_interval,
        super::DEFAULT_WATCH_RECONCILE_INTERVAL
    );
    assert_eq!(settings.provider_filter, Vec::<SourceKind>::new());
    assert!(!settings.poll);
    assert_eq!(
        settings.watch_paths,
        vec![
            fixture.root.join("config.toml"),
            fixture.claude_projects_root,
            fixture.codex_sessions_root,
            fixture.codex_home.join("archived_sessions"),
        ]
    );
    Ok(())
}

#[test]
fn load_watch_settings_prefers_cli_over_config() -> Result<()> {
    let mut fixture = WatchConfigFixture::new("watch-cli-over-config")?;
    fixture.config.watch = WatchConfig {
        debounce: Some("9m".to_owned()),
        min_interval: Some("8m".to_owned()),
        reconcile_interval: Some("7m".to_owned()),
        providers: vec![SourceKind::Claude],
        poll: false,
    };
    fixture.write_config()?;

    let settings = super::load_watch_settings(
        &fixture.root,
        &[SourceKind::Codex],
        &super::WatchOverrides {
            debounce: Some("5s".to_owned()),
            min_interval: Some("6s".to_owned()),
            reconcile_interval: Some("7s".to_owned()),
            poll: true,
        },
    )?;

    assert_eq!(settings.debounce, Duration::from_secs(5));
    assert_eq!(settings.min_interval, Duration::from_secs(6));
    assert_eq!(settings.reconcile_interval, Duration::from_secs(7));
    assert_eq!(settings.provider_filter, vec![SourceKind::Codex]);
    assert!(settings.poll);
    assert_eq!(
        settings.watch_paths,
        vec![
            fixture.root.join("config.toml"),
            fixture.codex_sessions_root,
            fixture.codex_home.join("archived_sessions"),
        ]
    );
    Ok(())
}

#[test]
fn load_watch_settings_uses_config_provider_filter_when_cli_empty() -> Result<()> {
    let mut fixture = WatchConfigFixture::new("watch-config-providers")?;
    fixture.config.watch.providers = vec![SourceKind::Claude];
    fixture.write_config()?;

    let settings =
        super::load_watch_settings(&fixture.root, &[], &super::WatchOverrides::default())?;

    assert_eq!(settings.provider_filter, vec![SourceKind::Claude]);
    assert_eq!(
        settings.watch_paths,
        vec![
            fixture.root.join("config.toml"),
            fixture.claude_projects_root
        ]
    );
    Ok(())
}

#[test]
fn load_watch_settings_rejects_invalid_config_duration() -> Result<()> {
    let mut fixture = WatchConfigFixture::new("watch-invalid-duration")?;
    fixture.config.watch.debounce = Some("soon".to_owned());
    fixture.write_config()?;

    let error = super::load_watch_settings(&fixture.root, &[], &super::WatchOverrides::default())
        .unwrap_err();

    assert!(format!("{error:#}").contains("invalid watch `debounce` duration `soon`"));
    Ok(())
}

#[test]
fn watch_paths_skip_disabled_and_missing_sources() -> Result<()> {
    let mut fixture = WatchConfigFixture::new("watch-disabled-sources")?;
    fixture.config.sources.claude.as_mut().unwrap().enabled = false;
    fs::remove_dir_all(&fixture.codex_sessions_root)?;
    fixture.write_config()?;

    let paths = super::watch_paths(&fixture.root, &fixture.config, &[])?;

    assert_eq!(
        paths,
        vec![
            fixture.root.join("config.toml"),
            fixture.codex_home.join("archived_sessions"),
        ]
    );
    Ok(())
}

#[test]
fn write_watch_status_records_settings_and_refresh_state() -> Result<()> {
    let root = unique_test_dir("watch-status");
    let settings = sample_watch_settings(
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(600),
    );
    let state = super::WatchState {
        last_event_at: Some("2026-04-30T04:00:00Z".to_owned()),
        last_refresh_reason: Some("change".to_owned()),
        last_refresh_started_at: Some("2026-04-30T04:00:30Z".to_owned()),
        last_refresh_completed_at: Some("2026-04-30T04:00:31Z".to_owned()),
        last_refresh_succeeded: Some(false),
        last_error: Some("synthetic failure".to_owned()),
    };

    super::write_watch_status(&root, &state, true, "refresh-watch", Some(&settings))?;
    let status: Value = serde_json::from_str(&fs::read_to_string(root.join("run/status.json"))?)?;

    assert_eq!(status["schema"], "darc.watch.status.v1");
    assert_eq!(status["root"], root.display().to_string());
    assert_eq!(status["mode"], "refresh-watch");
    assert_eq!(status["running"], true);
    assert_eq!(status["debounce"], "30s");
    assert_eq!(status["min_interval"], "1m");
    assert_eq!(status["reconcile_interval"], "10m");
    assert_eq!(status["poll"], false);
    assert_eq!(status["last_event_at"], "2026-04-30T04:00:00Z");
    assert_eq!(status["last_refresh_reason"], "change");
    assert_eq!(status["last_refresh_started_at"], "2026-04-30T04:00:30Z");
    assert_eq!(status["last_refresh_completed_at"], "2026-04-30T04:00:31Z");
    assert_eq!(status["last_refresh_succeeded"], false);
    assert_eq!(status["last_error"], "synthetic failure");
    Ok(())
}

#[test]
fn write_watch_status_keeps_settings_optional_for_legacy_compatibility() -> Result<()> {
    let root = unique_test_dir("watch-status-no-settings");

    super::write_watch_status(
        &root,
        &super::WatchState::default(),
        false,
        "refresh-watch",
        None,
    )?;
    let status: Value = serde_json::from_str(&fs::read_to_string(root.join("run/status.json"))?)?;

    assert!(status["debounce"].is_null());
    assert!(status["min_interval"].is_null());
    assert!(status["reconcile_interval"].is_null());
    assert!(status["poll"].is_null());
    assert_eq!(status["running"], false);
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn macos_launch_agent_plist_uses_refresh_watch_all_and_escapes_paths() {
    let root = PathBuf::from("/tmp/darc & root");
    let executable = PathBuf::from("/tmp/darc <bin> & service");

    let plist = super::macos_launch_agent_plist(&root, &executable, true);

    assert!(plist.contains("<key>Label</key>"));
    assert!(plist.contains("<string>com.0xjunha.darc.refresh</string>"));
    assert!(plist.contains("<string>/tmp/darc &lt;bin&gt; &amp; service</string>"));
    assert!(plist.contains("<string>refresh</string>"));
    assert!(plist.contains("<string>--watch</string>"));
    assert!(plist.contains("<string>--all</string>"));
    assert!(plist.contains("<string>--root</string>"));
    assert!(plist.contains("<string>/tmp/darc &amp; root</string>"));
    assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
    assert!(plist.contains("<string>/tmp/darc &amp; root/log/refresh-watch.out.log</string>"));
    assert!(plist.contains("<string>/tmp/darc &amp; root/log/refresh-watch.err.log</string>"));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_runtime_plist_path_stays_under_darc_run() {
    let root = PathBuf::from("/tmp/darc-root");

    assert_eq!(
        super::macos_runtime_plist_path(&root),
        PathBuf::from("/tmp/darc-root/run/com.0xjunha.darc.refresh.plist")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn write_macos_service_plist_creates_runtime_dirs_and_file() -> Result<()> {
    let root = unique_test_dir("macos-service-plist");
    let plist_path = root.join("custom").join("service.plist");

    let written = super::write_macos_service_plist(&plist_path, &root, false)?;
    let plist = fs::read_to_string(&plist_path)?;

    assert_eq!(written, plist_path);
    assert!(root.join("log").is_dir());
    assert!(root.join("run").is_dir());
    assert!(plist.contains("<key>RunAtLoad</key>\n  <false/>"));
    assert!(plist.contains(&format!("<string>{}</string>", root.display())));
    Ok(())
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

struct WatchConfigFixture {
    root: PathBuf,
    claude_projects_root: PathBuf,
    codex_home: PathBuf,
    codex_sessions_root: PathBuf,
    config: SharedConfig,
}

impl WatchConfigFixture {
    fn new(prefix: &str) -> Result<Self> {
        let root = unique_test_dir(prefix);
        let claude_home = root.join("claude-home");
        let claude_projects_root = claude_home.join("projects");
        let codex_home = root.join("codex-home");
        let codex_sessions_root = codex_home.join("sessions");
        fs::create_dir_all(&claude_projects_root)?;
        fs::create_dir_all(&codex_sessions_root)?;
        fs::create_dir_all(codex_home.join("archived_sessions"))?;
        let config = SharedConfig::new(
            root.clone(),
            Vec::new(),
            SourcesConfig {
                claude: Some(ClaudeSourceConfig {
                    enabled: true,
                    home: claude_home,
                    include_subagents: true,
                    projects_root: claude_projects_root.clone(),
                }),
                codex: Some(CodexSourceConfig {
                    enabled: true,
                    home: codex_home.clone(),
                    sessions_root: codex_sessions_root.clone(),
                }),
            },
        );
        let fixture = Self {
            root,
            claude_projects_root,
            codex_home,
            codex_sessions_root,
            config,
        };
        fixture.write_config()?;
        Ok(fixture)
    }

    fn write_config(&self) -> Result<()> {
        write_config_fixture(&self.root, &self.config)
    }
}

fn write_config_fixture(root: &Path, config: &SharedConfig) -> Result<()> {
    write_file(&root.join("config.toml"), &toml::to_string_pretty(config)?)?;
    Ok(())
}

#[test]
fn human_command_help_groups_options() {
    let status_help = help_for_command_path(&["status"]);
    assert_contains_in_order(&status_help, &["Scope:", "Mode:", "Output:", "Workspace:"]);

    let upgrade_help = help_for_command_path(&["upgrade"]);
    assert_contains_in_order(&upgrade_help, &["Mode:", "Output:"]);
    assert!(upgrade_help.contains("Only check whether a newer Darc release is available"));
    assert!(
        upgrade_help.contains("Write the upgrade check result as a machine-readable JSON envelope")
    );

    let sync_help = help_for_command_path(&["sync"]);
    assert_contains_in_order(&sync_help, &["Mode:", "Selection:", "Workspace:"]);
    assert!(sync_help.contains("Preview pending copies without writing files"));
    assert!(sync_help.contains("darc sync --dry-run"));

    let refresh_help = help_for_command_path(&["refresh"]);
    assert_contains_in_order(
        &refresh_help,
        &["Selection:", "Scope:", "Mode:", "Workspace:"],
    );
    assert!(refresh_help.contains("--watch"));
    assert!(refresh_help.contains("Quiet period before a watched refresh"));
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
fn release_version_comparison_handles_v_prefix_and_prerelease() -> Result<()> {
    assert!(release_version_is_newer("v0.2.0", "0.1.9")?);
    assert!(release_version_is_newer("0.2.0", "0.2.0-beta.1")?);
    assert!(release_version_is_newer("0.2.0-beta.10", "0.2.0-beta.2")?);
    assert!(release_version_is_newer("0.2.0-beta.2.1", "0.2.0-beta.2")?);
    assert!(!release_version_is_newer("0.2.0-beta.1", "0.2.0")?);
    assert!(!release_version_is_newer("0.2.0-beta.2", "0.2.0-beta.10")?);
    assert!(!release_version_is_newer("0.2.0", "0.2.0")?);
    Ok(())
}

#[test]
fn upgrade_nudge_requires_interactive_non_ci_context() {
    assert!(upgrade_nudge_enabled(
        true,
        true,
        Some("xterm-256color"),
        false,
        false
    ));
    assert!(!upgrade_nudge_enabled(
        false,
        true,
        Some("xterm-256color"),
        false,
        false
    ));
    assert!(!upgrade_nudge_enabled(
        true,
        false,
        Some("xterm-256color"),
        false,
        false
    ));
    assert!(!upgrade_nudge_enabled(
        true,
        true,
        Some("dumb"),
        false,
        false
    ));
    assert!(!upgrade_nudge_enabled(
        true,
        true,
        Some("xterm-256color"),
        true,
        false
    ));
    assert!(!upgrade_nudge_enabled(
        true,
        true,
        Some("xterm-256color"),
        false,
        true
    ));
}

#[test]
fn upgrade_nudge_cache_respects_check_and_notification_intervals() {
    let mut cache = super::UpgradeNudgeCache {
        checked_at_unix: Some(1_000),
        last_notified_at_unix: Some(1_000),
        latest_version: Some("0.2.0".to_owned()),
        latest_release_url: Some("https://github.com/0xjunha/darc/releases/tag/v0.2.0".to_owned()),
        dismissed_version: None,
        upgrade_available: true,
    };

    assert!(!should_check_upgrade_nudge(1_000 + 60, &cache));
    assert!(!should_notify_upgrade_nudge(1_000 + 60, &cache, "0.1.0"));
    assert!(should_check_upgrade_nudge(
        1_000 + super::UPGRADE_NUDGE_CHECK_INTERVAL.as_secs(),
        &cache
    ));
    assert!(should_notify_upgrade_nudge(
        1_000 + super::UPGRADE_NUDGE_NOTIFY_INTERVAL.as_secs(),
        &cache,
        "0.1.0"
    ));

    cache.dismissed_version = Some("0.2.0".to_owned());
    assert!(!should_notify_upgrade_nudge(
        1_000 + super::UPGRADE_NUDGE_NOTIFY_INTERVAL.as_secs(),
        &cache,
        "0.1.0"
    ));
    cache.dismissed_version = None;
    assert!(!should_notify_upgrade_nudge(
        1_000 + super::UPGRADE_NUDGE_NOTIFY_INTERVAL.as_secs(),
        &cache,
        "0.2.0"
    ));
}

#[test]
fn startup_upgrade_nudge_skips_json_watch_and_no_write_commands() {
    let refresh = Cli::try_parse_from(["darc", "refresh", "--root", "/tmp/darc-root"]).unwrap();
    assert_eq!(
        super::upgrade_nudge_root(&refresh.command),
        Some(Path::new("/tmp/darc-root"))
    );

    let watch =
        Cli::try_parse_from(["darc", "refresh", "--watch", "--root", "/tmp/darc-root"]).unwrap();
    assert!(super::upgrade_nudge_root(&watch.command).is_none());

    let status_json =
        Cli::try_parse_from(["darc", "status", "--json", "--root", "/tmp/darc-root"]).unwrap();
    assert!(super::upgrade_nudge_root(&status_json.command).is_none());

    let status = Cli::try_parse_from(["darc", "status", "--root", "/tmp/darc-root"]).unwrap();
    assert!(super::upgrade_nudge_root(&status.command).is_none());

    let status_check =
        Cli::try_parse_from(["darc", "status", "--check", "--root", "/tmp/darc-root"]).unwrap();
    assert!(super::upgrade_nudge_root(&status_check.command).is_none());

    let sync_dry_run =
        Cli::try_parse_from(["darc", "sync", "--dry-run", "--root", "/tmp/darc-root"]).unwrap();
    assert!(super::upgrade_nudge_root(&sync_dry_run.command).is_none());

    let project_link_dry_run = Cli::try_parse_from([
        "darc",
        "project",
        "link",
        "old-project",
        "--dry-run",
        "--root",
        "/tmp/darc-root",
    ])
    .unwrap();
    assert!(super::upgrade_nudge_root(&project_link_dry_run.command).is_none());

    let project_remove_dry_run = Cli::try_parse_from([
        "darc",
        "project",
        "remove",
        "old-project",
        "--dry-run",
        "--root",
        "/tmp/darc-root",
    ])
    .unwrap();
    assert!(super::upgrade_nudge_root(&project_remove_dry_run.command).is_none());

    let project_rename_dry_run = Cli::try_parse_from([
        "darc",
        "project",
        "rename-from",
        "old-project",
        "--dry-run",
        "--root",
        "/tmp/darc-root",
    ])
    .unwrap();
    assert!(super::upgrade_nudge_root(&project_rename_dry_run.command).is_none());

    let link_dry_run = Cli::try_parse_from([
        "darc",
        "link",
        "old-project",
        "--dry-run",
        "--root",
        "/tmp/darc-root",
    ])
    .unwrap();
    assert!(super::upgrade_nudge_root(&link_dry_run.command).is_none());

    let service_status =
        Cli::try_parse_from(["darc", "service", "--root", "/tmp/darc-root", "status"]).unwrap();
    assert!(super::upgrade_nudge_root(&service_status.command).is_none());

    let service_enable =
        Cli::try_parse_from(["darc", "service", "--root", "/tmp/darc-root", "enable"]).unwrap();
    assert_eq!(
        super::upgrade_nudge_root(&service_enable.command),
        Some(Path::new("/tmp/darc-root"))
    );

    let search =
        Cli::try_parse_from(["darc", "search", "--root", "/tmp/darc-root", "panic"]).unwrap();
    assert!(super::upgrade_nudge_root(&search.command).is_none());
}

#[test]
fn refresh_progress_printer_writes_interactive_steps() {
    let mut output = Vec::new();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::RefreshProgressPrinter::new(&mut output, style, true);
        printer.record(RefreshProgress::WorkspaceStarted { total_projects: 2 });
        printer.record(RefreshProgress::ProjectStarted {
            project_name: "repo-a".to_owned(),
            project_root: PathBuf::from("/tmp/repo-a"),
            project_index: 1,
            total_projects: 2,
        });
        printer.record(RefreshProgress::SyncStarted {
            project_name: "repo-a".to_owned(),
        });
        printer.record(RefreshProgress::IndexStarted {
            project_name: "repo-a".to_owned(),
        });
        printer.record(RefreshProgress::ProjectFinished {
            project_name: "repo-a".to_owned(),
        });
    }

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Refreshing workspace (2 projects)"));
    assert!(output.contains("  [1/2] repo-a"));
    assert!(output.contains("    [1/2] Syncing archive..."));
    assert!(output.contains("    [2/2] Indexing sessions..."));
    assert!(output.contains("    done"));
}

#[test]
fn refresh_progress_printer_stays_silent_when_disabled() {
    let mut output = Vec::new();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::RefreshProgressPrinter::new(&mut output, style, false);
        printer.record(RefreshProgress::ProjectStarted {
            project_name: "repo-a".to_owned(),
            project_root: PathBuf::from("/tmp/repo-a"),
            project_index: 1,
            total_projects: 1,
        });
        printer.record(RefreshProgress::SyncStarted {
            project_name: "repo-a".to_owned(),
        });
    }

    assert!(output.is_empty());
}

#[test]
fn service_progress_printer_writes_interactive_steps() {
    let mut output = Vec::new();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::ServiceProgressPrinter::new(&mut output, style, true);
        printer.started();
        printer.step(1, 2, "Writing LaunchAgent...");
        printer.step(2, 2, "Starting background service...");
        printer.done();
    }

    let output = String::from_utf8(output).unwrap();
    assert_contains_in_order(
        &output,
        &[
            "Enabling background auto-refresh.",
            "Initial refresh backfills the SQLite index and may take a few seconds.",
            "  [1/2] Writing LaunchAgent...",
            "  [2/2] Starting background service...",
            "  done",
        ],
    );
}

#[test]
fn service_progress_printer_stays_silent_when_disabled() {
    let mut output = Vec::new();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::ServiceProgressPrinter::new(&mut output, style, false);
        printer.started();
        printer.step(1, 2, "Writing LaunchAgent...");
        printer.done();
    }

    assert!(output.is_empty());
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
fn parses_project_management_namespace() {
    let link = Cli::try_parse_from(["darc", "project", "link", "memstack"]).unwrap();
    assert!(matches!(
        link.command,
        Commands::Project(super::ProjectArgs {
            command: super::ProjectCommands::Link(super::LinkArgs { project, .. }),
        }) if project == "memstack"
    ));

    let remove = Cli::try_parse_from(["darc", "project", "remove", "memstack"]).unwrap();
    assert!(matches!(
        remove.command,
        Commands::Project(super::ProjectArgs {
            command: super::ProjectCommands::Remove(super::RemoveArgs { project, .. }),
        }) if project == "memstack"
    ));

    let rename = Cli::try_parse_from(["darc", "project", "rename-from", "memstack"]).unwrap();
    assert!(matches!(
        rename.command,
        Commands::Project(super::ProjectArgs {
            command: super::ProjectCommands::RenameFrom(super::RenameArgs { project, .. }),
        }) if project == "memstack"
    ));
}

#[test]
fn project_link_help_keeps_safety_contract() {
    let help = help_for_command_path(&["project", "link"]);
    assert!(help.contains("This command is non-destructive."));
    assert!(help.contains("It does not run `darc refresh` or remove the source project."));
    assert!(help.contains("Configured source project name"));
    assert!(help.contains("--dry-run"));
}

#[test]
fn project_remove_and_rename_help_explain_project_argument() {
    let remove_help = help_for_command_path(&["project", "remove"]);
    assert!(remove_help.contains("matched against the configured project `name`"));
    assert!(remove_help.contains("Configured project name to remove"));
    assert_contains_in_order(&remove_help, &["Mode:", "Workspace:"]);

    let rename_help = help_for_command_path(&["project", "rename-from"]);
    assert!(rename_help.contains("Run the command from the new project directory"));
    assert!(rename_help.contains("Old configured project name"));
    assert_contains_in_order(&rename_help, &["Mode:", "Workspace:"]);
}

#[test]
fn canonical_read_commands_accept_shared_options_around_subcommands() {
    let cli = Cli::try_parse_from([
        "darc",
        "list",
        "--root",
        "/tmp/darc-root",
        "sessions",
        "--color",
        "never",
        "--limit",
        "1",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::List(super::ListArgs {
            root,
            color: super::ColorArg::Never,
            command: super::ListCommands::Sessions(super::ListSessionsArgs {
                limit,
                ..
            }),
        }) if root.as_path() == Path::new("/tmp/darc-root") && limit == 1
    ));

    let cli = Cli::try_parse_from([
        "darc",
        "stats",
        "workspace",
        "--root",
        "/tmp/darc-root",
        "--color",
        "never",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::Stats(super::StatsArgs {
            root,
            color: super::ColorArg::Never,
            command: super::StatsCommands::Workspace(_),
        }) if root.as_path() == Path::new("/tmp/darc-root")
    ));
}

#[test]
fn parses_canonical_list_show_search_stats_and_resolve_commands() {
    let sessions = Cli::try_parse_from([
        "darc",
        "list",
        "sessions",
        "--project-id",
        "repo-abc123",
        "--touching",
        "docs/**",
        "--limit",
        "5",
    ])
    .unwrap();
    assert!(matches!(
        sessions.command,
        Commands::List(super::ListArgs {
            command: super::ListCommands::Sessions(super::ListSessionsArgs {
                project_id,
                touching,
                limit,
                ..
            }),
            ..
        }) if project_id.as_deref() == Some("repo-abc123")
            && touching.as_deref() == Some("docs/**")
            && limit == 5
    ));

    let files = Cli::try_parse_from([
        "darc",
        "list",
        "files",
        "--session",
        "11111111",
        "--provider",
        "codex",
    ])
    .unwrap();
    assert!(matches!(
        files.command,
        Commands::List(super::ListArgs {
            command: super::ListCommands::Files(super::ListFilesArgs {
                session,
                provider,
                ..
            }),
            ..
        }) if session.as_deref() == Some("11111111")
            && matches!(provider, Some(super::ProviderArg::Codex))
    ));

    let path_files = Cli::try_parse_from([
        "darc",
        "list",
        "files",
        "crates/cli/src/lib.rs",
        "--matched-path-limit",
        "1",
    ])
    .unwrap();
    assert!(matches!(
        path_files.command,
        Commands::List(super::ListArgs {
            command: super::ListCommands::Files(super::ListFilesArgs {
                path_arg,
                matched_path_limit,
                ..
            }),
            ..
        }) if path_arg.as_deref() == Some("crates/cli/src/lib.rs")
            && matched_path_limit == Some(1)
    ));

    let flagged_path_files =
        Cli::try_parse_from(["darc", "list", "files", "--path", "docs/**"]).unwrap();
    assert!(matches!(
        flagged_path_files.command,
        Commands::List(super::ListArgs {
            command: super::ListCommands::Files(super::ListFilesArgs {
                path,
                ..
            }),
            ..
        }) if path.as_deref() == Some("docs/**")
    ));

    let show =
        Cli::try_parse_from(["darc", "show", "session", "11111111", "--turn-limit", "3"]).unwrap();
    assert!(matches!(
        show.command,
        Commands::Show(super::ShowArgs {
            command: super::ShowCommands::Session(super::QuerySessionBundleArgs {
                session_id_arg,
                turn_limit,
                ..
            }),
            ..
        }) if session_id_arg.as_deref() == Some("11111111") && turn_limit == 3
    ));

    let search = Cli::try_parse_from([
        "darc",
        "search",
        "--mode",
        "literal",
        "--query",
        "--output-last-message",
        "--field",
        "user-message",
    ])
    .unwrap();
    assert!(matches!(
        search.command,
        Commands::Search(super::SearchArgs {
            mode,
            query,
            fields,
            ..
        }) if matches!(mode, super::SearchModeArg::Literal)
            && query.as_deref() == Some("--output-last-message")
            && fields == [super::SearchEvidenceField::UserMessage]
    ));

    let path_search =
        Cli::try_parse_from(["darc", "search", "--mode", "file-path", "docs/**"]).unwrap();
    assert!(matches!(
        path_search.command,
        Commands::Search(super::SearchArgs {
            mode,
            query_arg,
            ..
        }) if matches!(mode, super::SearchModeArg::FilePath)
            && query_arg.as_deref() == Some("docs/**")
    ));

    assert!(Cli::try_parse_from(["darc", "search", "--regex", "panic"]).is_err());
    assert!(Cli::try_parse_from(["darc", "search", "--path", "docs/**"]).is_err());

    let stats = Cli::try_parse_from(["darc", "stats", "project", "--turn-limit", "5"]).unwrap();
    assert!(matches!(
        stats.command,
        Commands::Stats(super::StatsArgs {
            command: super::StatsCommands::Project(super::QueryProjectInsightsArgs {
                turn_limit,
                ..
            }),
            ..
        }) if turn_limit == 5
    ));

    let resolve = Cli::try_parse_from(["darc", "resolve", "session", "11111111"]).unwrap();
    assert!(matches!(
        resolve.command,
        Commands::Resolve(super::ResolveArgs {
            command: super::ResolveCommands::Session(super::QueryResolveSessionArgs {
                input,
                ..
            }),
            ..
        }) if input == "11111111"
    ));
}

#[test]
fn query_namespace_is_not_callable() {
    let error = Cli::try_parse_from(["darc", "query", "sessions"]).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("unrecognized subcommand 'query'")
    );
}

#[test]
fn canonical_show_session_help_mentions_prompt_and_final_message_projection() {
    let help = help_for_command_path(&["show", "session"]);

    assert!(help.contains("--session-view"));
    assert!(help.contains("session prompt/final message"));
    assert!(help.contains("compact previews"));
    assert!(help.contains("Identity:"));
    assert!(help.contains("Output:"));
    assert!(help.contains("Result Size:"));
    assert!(help.contains("darc list files --session <SESSION_ID>"));
}

#[test]
fn canonical_search_parses_literal_filters() {
    let cli = Cli::try_parse_from([
        "darc",
        "search",
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
        Commands::Search(super::SearchArgs {
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

#[cfg(target_os = "macos")]
#[test]
fn service_status_helpers_use_human_style_colors() {
    let style = super::HumanStyle::new(true, false, Some("xterm-256color"));
    let yes = super::yes_no(style, true);
    let no = super::yes_no(style, false);
    let failed = super::json_success_or_dash(style, &Value::Bool(false));
    let missing = super::json_error_or_dash(style, &Value::Null);

    assert!(yes.contains("\x1b["));
    assert!(no.contains("\x1b["));
    assert!(failed.contains("\x1b["));
    assert!(missing.contains("\x1b["));
    assert_eq!(strip_ansi_text(&yes), "yes");
    assert_eq!(strip_ansi_text(&no), "no");
    assert_eq!(strip_ansi_text(&failed), "false");
    assert_eq!(strip_ansi_text(&missing), "-");
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
fn show_workspace_help_hides_json_flag() {
    let help = help_for_command_path(&["show", "workspace"]);

    assert!(!help.contains("--json"));
}

#[test]
fn list_sessions_help_mentions_examples_for_time_bounds() {
    let help = help_for_command_path(&["list", "sessions"]);

    assert!(help.contains("--since"));
    assert!(help.contains("--until"));
    assert!(help.contains("--touching"));
    assert!(help.contains("5d"));
    assert!(help.contains("2026-04-07T00:00:00Z"));
    assert!(help.contains("current directory"));
}

#[test]
fn show_turn_help_mentions_narrative_view_behavior() {
    let help = help_for_command_path(&["show", "turn"]);

    assert!(help.contains("--view"));
    assert!(help.contains("narrative"));
    assert!(help.contains("tool arguments"));
    assert!(help.contains("[SESSION_ID] [TURN_ORDINAL]"));
    assert!(!help.contains("[TURN_ORDINAL]..."));
    assert!(help.contains("required unless --session-id is set"));
    assert!(help.contains("required unless --turn-ordinal is set"));
}

#[test]
fn list_turns_help_omits_removed_grep_surface() {
    let help = help_for_command_path(&["list", "turns"]);

    assert!(!help.contains("--grep"));
    assert!(help.contains("--view"));
    assert!(help.contains("oneline"));
    assert!(help.contains("--since"));
    assert!(help.contains("--until"));
    assert!(help.contains("required unless --session-id is set"));
}

#[test]
fn search_help_mentions_tool_output_opt_in() {
    let help = help_for_command_path(&["search"]);

    assert!(!help.contains("Options:"));
    assert!(help.contains("Help:"));
    assert!(help.contains("--include-tool-output"));
    assert!(help.contains("--field"));
    assert!(help.contains("--exclude-field"));
    assert!(help.contains("--match-limit <MATCH_LIMIT>"));
    assert!(help.contains("Maximum nested matches per literal/regex turn hit [default: 20]"));
    assert!(help.contains("literal and regex"));
    assert!(help.contains("Accepted fields:"));
    assert!(help.contains("messages: user-message, final-answer"));
    assert!(help.contains("path-fragment"));
    assert!(help.contains("Mode-specific query forms:"));
    assert!(help.contains("use --query when the text starts with '-'"));
    assert!(help.contains("Workspace:"));
    assert!(help.contains("Scope:"));
    assert!(help.contains("Search:"));
    assert!(help.contains("Evidence:"));
    assert!(help.contains("Time Filters:"));
    assert!(help.contains("Result Size:"));
    assert!(help.contains("Presentation:"));
    assert!(!help.contains("Output:"));
    assert_contains_in_order(
        &help,
        &[
            "Search:",
            "Scope:",
            "Evidence:",
            "Time Filters:",
            "Result Size:",
            "Workspace:",
            "Presentation:",
            "Help:",
            "Examples:",
        ],
    );
    assert!(help.contains("darc search \"panic unwrap\" --limit 5"));
    assert!(help.contains("darc search --mode regex --query \"error\\s+code\" --since 7d"));
}

#[test]
fn trailer_help_headers_are_white_not_green() {
    let root_ansi = cli_command().render_long_help().ansi().to_string();
    assert!(root_ansi.contains("\x1b[1;97mCommon workflows:\x1b[0m"));
    assert!(!root_ansi.contains("\x1b[1;92mCommon workflows:\x1b[0m"));

    let mut command = cli_command();
    let search = command
        .find_subcommand_mut("search")
        .expect("search subcommand should be present");
    let styled = search.render_long_help();
    let plain = styled.to_string();
    let ansi = styled.ansi().to_string();

    assert!(plain.contains("Examples:"));
    assert!(!plain.contains("\x1b["));
    assert!(ansi.contains("\x1b[1;97mExamples:\x1b[0m"));
    assert!(!ansi.contains("\x1b[1;92mExamples:\x1b[0m"));

    for path in [&["sync"][..], &["index"][..]] {
        let mut command = cli_command();
        let mut current = &mut command;
        for name in path {
            current = current.find_subcommand_mut(name).unwrap();
        }
        let ansi = current.render_long_help().ansi().to_string();
        assert!(ansi.contains("\x1b[1;97mExamples:\x1b[0m"));
        assert!(!ansi.contains("\x1b[1;92mExamples:\x1b[0m"));
    }
}

#[test]
fn list_files_help_mentions_path_and_co_touch_modes() {
    let help = help_for_command_path(&["list", "files"]);

    assert!(help.contains("[PATH]"));
    assert!(help.contains("--path"));
    assert!(help.contains("--co-touched-with"));
    assert!(help.contains("--matched-path-limit"));
    assert!(help.contains("most-touched files"));
    assert!(help.contains("paginated per-session file summary"));
    assert!(help.contains("top/path/co-touch/session modes"));
    assert!(help.contains("Selection:"));
    assert!(help.contains("Time Filters:"));
    assert!(help.contains("Result Size:"));
    assert!(help.contains("Workspace:"));
    assert_contains_in_order(
        &help,
        &[
            "Scope:",
            "Selection:",
            "Time Filters:",
            "Result Size:",
            "Workspace:",
        ],
    );
}

#[test]
fn parses_stats_workspace_command() {
    let cli = Cli::try_parse_from(["darc", "stats", "workspace", "--window", "14d"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::Stats(super::StatsArgs {
            command: super::StatsCommands::Workspace(super::QueryWorkspaceInsightsArgs {
                window_days,
                recent_session_limit,
                recent_session_offset,
                ..
            }),
            ..
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
    let error = Cli::try_parse_from(["darc", "list", "projects", "--json"]).unwrap_err();
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
    let mut command = cli_command();
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
    let mut command = cli_command();
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
    assert!(output.contains("Compatible Inspected Versions: 2.1.92, 2.1.87"));
    assert!(output.contains("Compatible across 2 inspected Claude version(s)."));
    assert!(output.contains("Supplementary Agent SDK Drift Version: 2.1.92"));
    assert!(output.contains("Sampling Stride: 1"));
    assert!(output.contains("Survey Mode: refine"));
    assert!(output.contains("Auth Mode: isolated (no auth)"));
}

#[test]
fn formats_failed_claude_schema_audit_versions() {
    let report = ClaudeSchemaAuditReport {
        compatible_inspected_versions: vec!["2.1.92".to_owned()],
        failed_versions: vec![ClaudeSchemaAuditFailure {
            version: "2.1.91".to_owned(),
            reason: "fixture `read_tool` did not trigger required Claude tool `Read`".to_owned(),
        }],
        supplementary_sdk_drift: None,
        ..compatible_claude_report()
    };
    let output = format_claude_schema_audit_report(&report);

    assert_eq!(claude_schema_audit_exit_code(&report), 1);
    assert!(output.contains("Status: audit incomplete"));
    assert!(output.contains("Compatible Inspected Versions: 2.1.92"));
    assert!(output.contains(
        "No transcript drift detected across 1 compatible inspected Claude version(s), but 1 inspected version(s) failed."
    ));
    assert!(output.contains("Failed Inspected Versions:"));
    assert!(
        output
            .contains("- 2.1.91: fixture `read_tool` did not trigger required Claude tool `Read`")
    );
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
            boundary_precision: ClaudeSchemaDriftBoundaryPrecision::Exact,
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

#[test]
fn formats_sampled_claude_schema_drift_without_first_boundary_claim() {
    let report = ClaudeSchemaAuditReport {
        outcome: ClaudeSchemaAuditOutcome::Drift(ClaudeSchemaDrift {
            first_drift_version: "2.1.90".to_owned(),
            boundary_precision: ClaudeSchemaDriftBoundaryPrecision::Sampled,
            difference_summary: vec![
                "$/line_types[2]: changed from \"system\" to \"mystery-event\"".to_owned(),
            ],
            likely_files_to_update: vec!["crates/rollout/src/claude/version.rs".to_owned()],
        }),
        supplementary_sdk_drift: None,
        ..compatible_claude_report()
    };
    let output = format_claude_schema_audit_report(&report);

    assert!(output.contains("Status: schema drift detected"));
    assert!(output.contains("Sampled Drift Version: 2.1.90"));
    assert!(output.contains("Drift Boundary Precision: sampled window (first drift unproven)"));
    assert!(!output.contains("First Drift Version: 2.1.90"));
}
