use super::*;

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
fn launchctl_failure_message_structures_bootstrap_errors() {
    let args = vec![
        "bootstrap".to_owned(),
        "gui/501".to_owned(),
        "/tmp/darc/LaunchAgents/com.0xjunha.darc.refresh.plist".to_owned(),
    ];
    let message = super::launchctl_failure_message(
        &args,
        "Bootstrap failed: 5: Input/output error\nTry re-running the command as root for richer errors.",
    );

    assert_contains_in_order(
        &message,
        &[
            "failed to manage the macOS LaunchAgent",
            "Command: launchctl bootstrap gui/501 /tmp/darc/LaunchAgents/com.0xjunha.darc.refresh.plist",
            "Detail:",
            "Bootstrap failed: 5: Input/output error",
            "Try re-running the command as root for richer errors.",
            "Hint:",
            "Darc retried the start before giving up",
        ],
    );
}

#[test]
fn launchctl_retryable_bootstrap_detection_is_specific() {
    let bootstrap_args = vec![
        "bootstrap".to_owned(),
        "gui/501".to_owned(),
        "/tmp/darc.plist".to_owned(),
    ];
    let bootout_args = vec![
        "bootout".to_owned(),
        "gui/501/com.0xjunha.darc.refresh".to_owned(),
    ];

    assert!(super::launchctl_failure_is_retryable_bootstrap(
        &bootstrap_args,
        "Bootstrap failed: 5: Input/output error",
    ));
    assert!(!super::launchctl_failure_is_retryable_bootstrap(
        &bootout_args,
        "Bootstrap failed: 5: Input/output error",
    ));
    assert!(!super::launchctl_failure_is_retryable_bootstrap(
        &bootstrap_args,
        "Bootstrap failed: 37: Operation already in progress",
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
fn parse_duration_rejects_zero_duration() {
    let error = super::parse_duration("0s").unwrap_err();

    assert!(format!("{error:#}").contains("duration must be greater than zero"));
}

#[test]
fn parse_duration_rejects_unsupported_unit() {
    let error = super::parse_duration("1d").unwrap_err();

    assert!(format!("{error:#}").contains("unsupported duration unit"));
}
