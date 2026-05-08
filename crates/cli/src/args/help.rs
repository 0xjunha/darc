use clap::{
    Arg, ArgAction, Command as ClapCommand, CommandFactory,
    builder::styling::{AnsiColor, Styles},
};

use super::Cli;

/// Terminal styles for Clap-rendered help reference pages.
pub(crate) const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::BrightGreen.on_default().bold())
    .usage(AnsiColor::BrightGreen.on_default().bold())
    .literal(AnsiColor::BrightWhite.on_default().bold())
    .placeholder(AnsiColor::BrightBlue.on_default())
    .error(AnsiColor::BrightRed.on_default().bold())
    .valid(AnsiColor::BrightGreen.on_default())
    .invalid(AnsiColor::BrightYellow.on_default());

pub(crate) const LINK_LONG_ABOUT: &str = "Link one configured project's historical paths into the current project.\n\nRun this command from the target project directory.\nThe PROJECT argument is the old or source project name already stored in ~/.darc/config.toml.\n\nExample:\n- You renamed `/path/to/old-project` to `/path/to/new-project`.\n- Darc still has a configured project named `old-project`.\n- Run `cd /path/to/new-project && darc project link old-project`.\n\nThis command is non-destructive.\nIt updates config so the current project knows the source project's old local_path and known_paths.\nIt does not run `darc refresh` or remove the source project.\n\nUse `--dry-run` to preview the target project, source project, and known-path changes without writing config.";
pub(crate) const REMOVE_LONG_ABOUT: &str = "Remove one configured project and its archived/indexed data.\n\nThe PROJECT argument is matched against the configured project `name` in ~/.darc/config.toml.\nThe name must identify exactly one configured project.\n\nThis command deletes:\n- the project entry from config.toml\n- the project's archived sessions directory under ~/.darc/projects/...\n- the project's indexed SQLite rows\n\nUse `--dry-run` to preview the resolved project and deletion counts without writing.\nYou can run this command from any directory.";
pub(crate) const RENAME_FROM_LONG_ABOUT: &str = "Rebuild one old project's history into the current renamed project.\n\nUse this when you just renamed a project from one name to another.\nRun the command from the new project directory, and pass the old project name.\n\nExample:\n- Darc config still contains a project named `old-project`.\n- You renamed the checkout to `/path/to/new-project`.\n- Run `cd /path/to/new-project && darc project rename-from old-project`.\n\nThis command bootstraps or reuses the current project as the target, links the old project's paths into it, runs `darc refresh`, and removes the old source project after those steps succeed.\n\nIn other words, it is the safe built-in workflow for:\n`darc project link <old-project> && darc refresh && darc project remove <old-project>`\n\nUse `--dry-run` to preview the link target and cleanup counts without writing.\nIf ~/.darc/config.toml does not exist yet, run `darc init` first.";
pub(crate) const HELP_TRAILER_HEADER_STYLE: &str = "\x1b[1;97m";
pub(crate) const HELP_RESET_STYLE: &str = "\x1b[0m";
pub(crate) const AGENTS_MD_GUIDANCE_START_MARKER: &str = "<!-- darc:agent-help:start -->";
pub(crate) const AGENTS_MD_GUIDANCE_END_MARKER: &str = "<!-- darc:agent-help:end -->";
pub(crate) const AGENTS_MD_GUIDANCE_TEXT: &str = "When a task depends on prior decisions, regressions, repeated failures, PR handoffs, ambiguous references to earlier work, or file/module history, run `darc agent-help` and use Darc for exact prior-session evidence. Verify conclusions against current files and tests.";

/// Returns one styled help trailer section.
pub(crate) fn styled_help_section(title: &str, body: &str) -> String {
    format!("{HELP_TRAILER_HEADER_STYLE}{title}:{HELP_RESET_STYLE}\n{body}")
}

/// Returns top-level common workflow examples.
pub(crate) fn root_after_help() -> String {
    styled_help_section(
        "Common workflows",
        concat!(
            "  darc status                                      # check active-project health\n",
            "  darc agent-help                                  # show agent usage guidance\n",
            "  darc refresh --auto                              # keep Darc fresh automatically on macOS\n",
            "  darc refresh                                     # refresh once without background jobs\n",
            "  darc search \"panic\" --limit 5                    # find matching turns\n",
            "  darc show session <SESSION_ID> --turn-limit 5    # inspect a session\n",
            "  darc upgrade --check                             # check for a newer CLI release\n\n",
            "Run `darc help <command>` for details on a specific command.",
        ),
    )
}

/// Returns sync command examples.
pub(crate) fn sync_after_help() -> String {
    styled_help_section(
        "Examples",
        "  darc sync --dry-run\n  darc sync --provider codex",
    )
}

/// Returns index command examples.
pub(crate) fn index_after_help() -> String {
    styled_help_section("Examples", "  darc index\n  darc index --provider claude")
}

/// Returns canonical search command examples.
pub(crate) fn search_after_help() -> String {
    styled_help_section(
        "Examples",
        "  darc search \"panic unwrap\" --limit 5\n  darc search --mode literal --query \"--output-last-message\" --field user-message\n  darc search --mode regex --query \"error\\s+code\" --since 7d\n  darc search --mode file-path \"docs/**/*.md\" --limit 5\n  darc search --mode path-fragment query-protocol",
    )
}

/// Returns mode-specific guidance for search query text.
pub(crate) fn search_query_help() -> &'static str {
    "Query text or path pattern. The accepted form depends on --mode.\n\nMode-specific query forms:\n  keyword: one or more terms, e.g. \"panic unwrap\"; searches Darc's derived per-turn text.\n  literal: exact plain text, e.g. \"--output-last-message\"; use --query when the text starts with '-'.\n  regex: Rust regex, e.g. \"panic|unwrap\" or \"error\\s+code\"; quote shell metacharacters.\n  file-name: file basename text, e.g. \"lib.rs\".\n  file-path: project-relative glob, e.g. \"docs/**/*.md\".\n  path-fragment: path substring or prefix, e.g. \"query-protocol\"."
}

/// Builds the Clap command tree with Darc-specific help flag placement.
pub(crate) fn cli_command() -> ClapCommand {
    with_explicit_help_arg(Cli::command())
}

/// Adds the Darc help flag into a stable `Help` section for one command tree.
pub(crate) fn with_explicit_help_arg(command: ClapCommand) -> ClapCommand {
    command
        .disable_help_flag(true)
        .arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::Help)
                .help("Print help (see a summary with '-h')")
                .help_heading("Help"),
        )
        .mut_subcommands(with_explicit_help_arg)
}
