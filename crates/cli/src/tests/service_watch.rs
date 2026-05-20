use std::io::{self, Write};

use super::*;

#[derive(Default)]
struct FlushCountingWriter {
    bytes: Vec<u8>,
    flushes: usize,
}

impl Write for FlushCountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
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
fn macos_service_launchctl_args_cover_restart_sequence() {
    let target = "gui/501/com.0xjunha.darc.refresh";

    assert_eq!(
        super::macos_service_bootout_launchctl_args(target),
        vec!["bootout".to_owned(), target.to_owned()]
    );
    assert_eq!(
        super::macos_service_bootstrap_launchctl_args(Path::new("/tmp/darc.plist"), "gui/501"),
        vec![
            "bootstrap".to_owned(),
            "gui/501".to_owned(),
            "/tmp/darc.plist".to_owned()
        ]
    );
    assert_eq!(
        super::macos_service_kickstart_launchctl_args(target),
        vec!["kickstart".to_owned(), target.to_owned()]
    );
}

#[test]
fn macos_service_start_outcome_describes_auto_restart() {
    assert_eq!(
        super::MacosServiceStartOutcome::Started.auto_status(),
        "enabled and started"
    );
    assert_eq!(
        super::MacosServiceStartOutcome::Restarted.auto_status(),
        "enabled and restarted"
    );
    assert_eq!(
        super::MacosServiceStartOutcome::Restarted.service_status(),
        "restarted"
    );
    assert!(
        super::MacosServiceStartOutcome::Restarted
            .auto_hint()
            .unwrap()
            .contains("stopped the existing service")
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
fn watch_change_resets_debounce_to_latest_event() {
    let now = Instant::now();
    let mut state = super::WatchState::default();
    let mut dirty_since = Some(now - Duration::from_secs(120));

    super::record_watch_change(&mut state, &mut dirty_since, now);

    assert_eq!(dirty_since, Some(now));
    assert!(state.last_event_at.is_some());
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
        watch_identity: None,
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
    assert!(status["watch_pid"].is_null());
    assert!(status["watch_token"].is_null());
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
    assert!(plist.contains("<key>KeepAlive</key>"));
    assert!(plist.contains("<key>SuccessfulExit</key>\n    <false/>"));
    assert!(plist.contains("<key>ThrottleInterval</key>\n  <integer>30</integer>"));
    assert!(plist.contains("<string>/tmp/darc &amp; root/log/refresh-watch.out.log</string>"));
    assert!(plist.contains("<string>/tmp/darc &amp; root/log/refresh-watch.err.log</string>"));
}

#[test]
fn watch_status_stop_marker_preserves_refresh_details() -> Result<()> {
    let root = unique_test_dir("watch-status-stop");
    let settings = sample_watch_settings(
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(600),
    );
    let state = super::WatchState {
        watch_identity: None,
        last_event_at: Some("2026-04-30T04:00:00Z".to_owned()),
        last_refresh_reason: Some("change".to_owned()),
        last_refresh_started_at: Some("2026-04-30T04:00:30Z".to_owned()),
        last_refresh_completed_at: Some("2026-04-30T04:00:31Z".to_owned()),
        last_refresh_succeeded: Some(false),
        last_error: Some("synthetic failure".to_owned()),
    };

    super::write_watch_status(&root, &state, true, "refresh-watch", Some(&settings))?;
    super::mark_watch_status_stopped(&root)?;
    let status: Value = serde_json::from_str(&fs::read_to_string(root.join("run/status.json"))?)?;

    assert_eq!(status["running"], false);
    assert_eq!(status["last_refresh_reason"], "change");
    assert_eq!(status["last_error"], "synthetic failure");
    Ok(())
}

#[test]
fn watch_status_stop_marker_rewrites_malformed_status() -> Result<()> {
    let root = unique_test_dir("watch-status-stop-malformed");
    let status_path = root.join("run/status.json");
    fs::create_dir_all(status_path.parent().unwrap())?;
    write_file(&status_path, "not json")?;

    super::mark_watch_status_stopped(&root)?;
    let status: Value = serde_json::from_str(&fs::read_to_string(status_path)?)?;

    assert_eq!(status["schema"], "darc.watch.status.v1");
    assert_eq!(status["root"], root.display().to_string());
    assert_eq!(status["mode"], "refresh-watch");
    assert_eq!(status["running"], false);
    assert!(status["last_refresh_reason"].is_null());
    Ok(())
}

#[test]
fn watch_status_stop_marker_rewrites_non_object_status() -> Result<()> {
    let root = unique_test_dir("watch-status-stop-non-object");
    let status_path = root.join("run/status.json");
    fs::create_dir_all(status_path.parent().unwrap())?;
    write_file(&status_path, "[]")?;

    super::mark_watch_status_stopped(&root)?;
    let status: Value = serde_json::from_str(&fs::read_to_string(status_path)?)?;

    assert_eq!(status["running"], false);
    assert_eq!(status["root"], root.display().to_string());
    Ok(())
}

#[test]
fn watch_status_stop_marker_creates_missing_run_dir() -> Result<()> {
    let root = unique_test_dir("watch-status-stop-missing-run");

    super::mark_watch_status_stopped(&root)?;
    let status_path = root.join("run/status.json");
    let status: Value = serde_json::from_str(&fs::read_to_string(status_path)?)?;

    assert_eq!(status["running"], false);
    assert_eq!(status["root"], root.display().to_string());
    Ok(())
}

#[test]
fn watch_status_guard_stops_matching_watch_identity() -> Result<()> {
    let root = unique_test_dir("watch-status-stop-matching-identity");
    let identity = super::WatchIdentity {
        pid: 123,
        token: "watch-a".to_owned(),
    };
    let state = super::WatchState {
        watch_identity: Some(identity.clone()),
        last_refresh_reason: Some("change".to_owned()),
        ..super::WatchState::default()
    };

    super::write_watch_status(&root, &state, true, "refresh-watch", None)?;
    super::mark_watch_status_stopped_if_current(&root, &identity)?;
    let status: Value = serde_json::from_str(&fs::read_to_string(root.join("run/status.json"))?)?;

    assert_eq!(status["running"], false);
    assert_eq!(status["watch_pid"].as_u64(), Some(u64::from(identity.pid)));
    assert_eq!(
        status["watch_token"].as_str(),
        Some(identity.token.as_str())
    );
    assert_eq!(status["last_refresh_reason"], "change");
    Ok(())
}

#[test]
fn watch_status_guard_does_not_stop_newer_watch_identity() -> Result<()> {
    let root = unique_test_dir("watch-status-stop-newer-identity");
    let old_identity = super::WatchIdentity {
        pid: 123,
        token: "watch-old".to_owned(),
    };
    let new_identity = super::WatchIdentity {
        pid: 123,
        token: "watch-new".to_owned(),
    };
    let state = super::WatchState {
        watch_identity: Some(new_identity.clone()),
        last_refresh_reason: Some("reconcile".to_owned()),
        ..super::WatchState::default()
    };

    super::write_watch_status(&root, &state, true, "refresh-watch", None)?;
    super::mark_watch_status_stopped_if_current(&root, &old_identity)?;
    let status: Value = serde_json::from_str(&fs::read_to_string(root.join("run/status.json"))?)?;

    assert_eq!(status["running"], true);
    assert_eq!(
        status["watch_token"].as_str(),
        Some(new_identity.token.as_str())
    );
    assert_eq!(status["last_refresh_reason"], "reconcile");
    Ok(())
}

#[test]
fn refresh_lock_records_holder_metadata_and_clears_on_drop() -> Result<()> {
    let root = unique_test_dir("refresh-lock-metadata");
    let lock_path = root.join("run/refresh.lock");

    let lock = super::acquire_refresh_lock(&root)?;
    let info = super::read_refresh_lock_info(&lock_path)?.unwrap();

    assert_eq!(info.schema, super::REFRESH_LOCK_SCHEMA);
    assert_eq!(info.pid, std::process::id());
    assert!(!info.started_at.is_empty());

    drop(lock);

    assert!(fs::read_to_string(&lock_path)?.trim().is_empty());
    assert_eq!(
        super::inspect_refresh_lock(&root)?,
        super::RefreshLockSnapshot::Available { stale_info: None }
    );
    Ok(())
}

#[test]
fn refresh_lock_inspection_reports_stale_metadata() -> Result<()> {
    let root = unique_test_dir("refresh-lock-stale-metadata");
    let lock_path = root.join("run/refresh.lock");
    fs::create_dir_all(lock_path.parent().unwrap())?;
    let stale_info = super::RefreshLockInfo {
        schema: super::REFRESH_LOCK_SCHEMA.to_owned(),
        pid: 42,
        started_at: "2026-04-30T04:00:00Z".to_owned(),
    };
    write_file(&lock_path, &serde_json::to_string_pretty(&stale_info)?)?;

    assert_eq!(
        super::inspect_refresh_lock(&root)?,
        super::RefreshLockSnapshot::Available {
            stale_info: Some(stale_info)
        }
    );
    Ok(())
}

#[test]
fn service_status_helpers_flag_stale_watch_status() {
    let running_status = serde_json::json!({ "running": true });
    let stopped_status = serde_json::json!({ "running": false });
    let style = super::HumanStyle::new(false, false, None);

    assert_eq!(
        super::macos_watch_process_state(false, Some(&running_status), false),
        super::MacosWatchProcessState::StaleLaunchdStopped
    );
    assert_eq!(
        super::macos_watch_process_state(true, Some(&running_status), false),
        super::MacosWatchProcessState::Running
    );
    assert_eq!(
        super::macos_watch_process_state(true, Some(&stopped_status), false),
        super::MacosWatchProcessState::Starting
    );
    assert_eq!(
        super::macos_watch_process_state(true, Some(&stopped_status), true),
        super::MacosWatchProcessState::StaleLaunchdRunning
    );
    assert_eq!(
        super::macos_watch_process_state(true, Some(&running_status), true),
        super::MacosWatchProcessState::StaleLaunchdRunning
    );
    assert_eq!(
        super::macos_watch_process_state(false, Some(&stopped_status), true),
        super::MacosWatchProcessState::Stopped
    );
    assert_eq!(
        super::format_macos_watch_process_state(
            style,
            super::MacosWatchProcessState::StaleLaunchdRunning
        ),
        "stale; launchd running"
    );
    assert_eq!(
        super::format_macos_watch_process_state(
            style,
            super::MacosWatchProcessState::StaleLaunchdStopped
        ),
        "stale; launchd not running"
    );
}

#[test]
fn service_status_helpers_flag_old_watch_status_as_stale() {
    let status = serde_json::json!({ "running": true, "reconcile_interval": "10m" });

    assert!(!super::macos_watch_status_stale(
        &status,
        Duration::from_secs(1_200)
    ));
    assert!(super::macos_watch_status_stale(
        &status,
        Duration::from_secs(1_201)
    ));
}

#[test]
fn service_stop_marker_marks_running_status_stopped() -> Result<()> {
    let root = unique_test_dir("service-stop-status");
    let state = super::WatchState {
        last_refresh_reason: Some("change".to_owned()),
        last_refresh_succeeded: Some(true),
        ..super::WatchState::default()
    };

    super::write_watch_status(&root, &state, true, "refresh-watch", None)?;
    super::mark_macos_service_stopped(&root)?;
    let status: Value = serde_json::from_str(&fs::read_to_string(root.join("run/status.json"))?)?;

    assert_eq!(status["running"], false);
    assert_eq!(status["last_refresh_reason"], "change");
    assert_eq!(
        super::macos_watch_process_state(false, Some(&status), false),
        super::MacosWatchProcessState::Stopped
    );
    Ok(())
}

#[test]
fn service_stop_marker_tolerates_malformed_status() -> Result<()> {
    let root = unique_test_dir("service-stop-status-malformed");
    let status_path = root.join("run/status.json");
    fs::create_dir_all(status_path.parent().unwrap())?;
    write_file(&status_path, "{")?;

    super::mark_macos_service_stopped(&root)?;
    let status: Value = serde_json::from_str(&fs::read_to_string(status_path)?)?;

    assert_eq!(status["running"], false);
    assert_eq!(
        super::macos_watch_process_state(false, Some(&status), false),
        super::MacosWatchProcessState::Stopped
    );
    Ok(())
}

#[test]
fn service_status_helpers_format_stale_lock_metadata() {
    let style = super::HumanStyle::new(false, false, None);
    let info = super::RefreshLockInfo {
        schema: super::REFRESH_LOCK_SCHEMA.to_owned(),
        pid: 42,
        started_at: "2026-04-30T04:00:00Z".to_owned(),
    };

    let formatted = super::format_refresh_lock_snapshot(
        style,
        &super::RefreshLockSnapshot::Available {
            stale_info: Some(info),
        },
    );

    assert!(formatted.contains("stale holder metadata"));
    assert!(formatted.contains("pid 42 since 2026-04-30T04:00:00Z"));
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
        printer.record(RefreshProgress::SyncingSessions {
            project_name: "repo-a".to_owned(),
            synced_sessions: 2,
            total_sessions: 4,
        });
        printer.record(RefreshProgress::SyncFinished {
            project_name: "repo-a".to_owned(),
        });
        printer.record(RefreshProgress::IndexStarted {
            project_name: "repo-a".to_owned(),
        });
        printer.record(RefreshProgress::IndexingSessions {
            project_name: "repo-a".to_owned(),
            indexed_sessions: 3,
            total_sessions: 4,
        });
        printer.record(RefreshProgress::IndexFinished {
            project_name: "repo-a".to_owned(),
        });
        printer.record(RefreshProgress::ProjectFinished {
            project_name: "repo-a".to_owned(),
        });
    }

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Refreshing workspace (2 projects)"));
    assert!(output.contains("  [1/2] repo-a"));
    assert!(output.contains("    [1] Syncing archive..."));
    assert!(output.contains("        Syncing sessions   [############------------] 2/4  50%"));
    assert!(output.contains("    [2] Indexing sessions..."));
    assert!(output.contains("        Indexing sessions  [##################------] 3/4  75%"));
    assert!(output.contains("    done"));
    assert!(output.contains("Projects           [############------------] 1/2  50%"));
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
fn refresh_progress_printer_does_not_flush_skipped_session_updates() {
    let mut output = FlushCountingWriter::default();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::RefreshProgressPrinter::new(&mut output, style, true);
        printer.record(RefreshProgress::IndexStarted {
            project_name: "repo-a".to_owned(),
        });
        printer.record(RefreshProgress::IndexingSessions {
            project_name: "repo-a".to_owned(),
            indexed_sessions: 0,
            total_sessions: 1_000,
        });
        for indexed_sessions in 1..100 {
            printer.record(RefreshProgress::IndexingSessions {
                project_name: "repo-a".to_owned(),
                indexed_sessions,
                total_sessions: 1_000,
            });
        }
    }

    assert_eq!(output.flushes, 2);
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
