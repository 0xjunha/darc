use super::*;

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
fn post_upgrade_background_refresh_note_bolds_restart_command() {
    let note = super::post_upgrade_background_refresh_note(HumanStyle::new(
        true,
        false,
        Some("xterm-256color"),
    ));

    assert!(note.contains("\x1b[1mdarc refresh --auto\x1b[0m"));
    assert_eq!(
        strip_ansi_text(&note),
        "If Darc auto-refresh was running, run darc refresh --auto to restart it with the new version."
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
