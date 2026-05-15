use super::*;

#[cfg(target_os = "macos")]
#[test]
fn macos_runtime_plist_path_stays_under_darc_run() {
    let root = PathBuf::from("/tmp/darc-root");

    assert_eq!(
        super::macos_runtime_plist_path(&root),
        PathBuf::from("/tmp/darc-root/run/com.0xjunha.darc.refresh.plist")
    );
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

    let index_help = help_for_command_path(&["index"]);
    assert_contains_in_order(&index_help, &["Selection:", "Mode:", "Workspace:"]);
    assert!(
        index_help.contains(
            "Delete the shared SQLite index and rebuild it from every configured project's archived sessions"
        )
    );
    assert!(index_help.contains("darc index --rebuild"));

    let refresh_help = help_for_command_path(&["refresh"]);
    assert_contains_in_order(
        &refresh_help,
        &["Selection:", "Scope:", "Mode:", "Workspace:"],
    );
    assert!(refresh_help.contains("--watch"));
    assert!(refresh_help.contains("Quiet period before a watched refresh"));
}

#[test]
fn index_command_accepts_provider_filters() {
    let cli = Cli::try_parse_from(["darc", "index", "--provider", "claude"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::Index(super::IndexArgs {
            provider,
            rebuild: false,
            ..
        }) if provider.len() == 1
    ));

    let rebuild = Cli::try_parse_from(["darc", "index", "--rebuild"]).unwrap();
    assert!(matches!(
        rebuild.command,
        Commands::Index(super::IndexArgs {
            provider,
            rebuild: true,
            ..
        }) if provider.is_empty()
    ));
}

#[test]
fn index_rebuild_rejects_provider_filters() {
    let error = run_index(super::IndexArgs {
        provider: vec![super::ProviderArg::Codex],
        rebuild: true,
        root: unique_test_dir("index-rebuild-provider-filter"),
    })
    .expect_err("rebuild should reject provider filters");

    assert!(
        format!("{error:#}")
            .contains("`darc index --rebuild` rebuilds all providers; remove `--provider`")
    );
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
        "--shared",
    ])
    .unwrap();
    assert!(matches!(
        files.command,
        Commands::List(super::ListArgs {
            command: super::ListCommands::Files(super::ListFilesArgs {
                session,
                provider,
                shared,
                ..
            }),
            ..
        }) if session.as_deref() == Some("11111111")
            && matches!(provider, Some(super::ProviderArg::Codex))
            && shared
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
