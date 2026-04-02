use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use memstack_core::{
    InitDraft, SkippedCodexRollout, SourceKind, SyncOptions, default_root_path, execute_sync,
    parse_project_codex_turns, prepare_init, prepare_sync, write_init,
};

#[derive(Debug, Parser)]
#[command(name = "memstack", version, about = "Memstack CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Supported CLI subcommands.
#[derive(Debug, Subcommand)]
enum Commands {
    /// Detect local sources and create the shared memstack config.
    Init(InitArgs),
    /// Sync matching Claude and Codex sessions into the project archive.
    Sync(SyncArgs),
    /// Parse archived Codex rollouts for the active project into SQLite.
    Parse(ParseArgs),
}

/// Detect local sources and create the shared memstack config.
#[derive(Debug, Args)]
struct InitArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(long)]
    dry_run: bool,
}

/// Sync matching Claude and Codex sessions into the project archive.
#[derive(Debug, Args)]
struct SyncArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(long)]
    dry_run: bool,

    #[arg(long = "source", value_enum)]
    source: Vec<SourceArg>,
}

/// Parse archived Codex rollouts for the active project into SQLite.
#[derive(Debug, Args)]
struct ParseArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,
}

/// Represents the supported source filters for `sync`.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum SourceArg {
    Claude,
    Codex,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

/// Parses CLI arguments and dispatches the selected command.
fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init(args) => run_init(args),
        Commands::Sync(args) => run_sync(args),
        Commands::Parse(args) => run_parse(args),
    }
}

/// Prepares and optionally writes the shared init draft.
fn run_init(args: InitArgs) -> Result<()> {
    let draft = prepare_init(Some(args.root))?;

    if !args.dry_run {
        write_init(&draft)?;
    }

    println!("{draft}");
    if args.dry_run {
        println!("\n{}", format_init_status(&draft, true));
        println!();
        println!("{}", draft.config_toml()?);
    } else {
        println!("\n{}", format_init_status(&draft, false));
    }

    Ok(())
}

/// Formats the post-summary status lines for `init`.
fn format_init_status(draft: &InitDraft, dry_run: bool) -> String {
    if dry_run {
        return if draft.global_config_exists {
            if draft.project_exists {
                "Dry run only. Existing memstack config was left unchanged.".to_owned()
            } else {
                "Dry run only. Project was not added to memstack.".to_owned()
            }
        } else {
            "Dry run only. Global memstack config and project registration were not written."
                .to_owned()
        };
    }

    let mut lines = Vec::new();
    if !draft.global_config_exists {
        lines.push("Initialized global memstack config.".to_owned());
    }
    lines.push(if draft.project_exists {
        "Project is already configured in memstack.".to_owned()
    } else {
        "Added project to memstack.".to_owned()
    });
    lines.join("\n")
}

/// Prepares and optionally executes the project-scoped sync workflow.
fn run_sync(args: SyncArgs) -> Result<()> {
    let plan = prepare_sync(
        Some(args.root),
        SyncOptions {
            source_filter: args.source.into_iter().map(SourceArg::into).collect(),
        },
    )?;

    println!("Project: {}", plan.project_name);
    println!("Project Root: {}", plan.project_root.display());
    println!("Archive: {}", plan.sessions_root.display());
    println!("Sources: {}", format_sources(&plan.sources));
    println!(
        "Sessions: {} to copy, {} unchanged",
        plan.sessions_to_copy(),
        plan.sessions_unchanged
    );
    println!(
        "Auxiliary: {} to copy, {} unchanged",
        plan.auxiliary_to_copy(),
        plan.auxiliary_unchanged
    );
    if !plan.new_known_paths.is_empty() {
        println!("Known paths: {} new", plan.new_known_paths.len());
    }
    for warning in &plan.warnings {
        eprintln!("warning: {warning}");
    }

    if args.dry_run {
        println!("\nDry run only. No files were written.");
        return Ok(());
    }

    let report = execute_sync(plan)?;
    println!(
        "\nSynced {} session files and {} auxiliary files.",
        report.sessions_copied, report.auxiliary_copied
    );
    if report.manifest_written {
        println!("Updated manifest.");
    }
    if report.config_written {
        println!("Updated config.");
    }

    Ok(())
}

/// Parses archived Codex rollouts for the active project into SQLite.
fn run_parse(args: ParseArgs) -> Result<()> {
    let report = parse_project_codex_turns(Some(args.root))?;

    for skipped in &report.skipped_rollouts {
        eprintln!("warning: {}", format_skipped_rollout(skipped));
    }

    println!("Project: {}", report.project_name);
    println!("Project Root: {}", report.project_root.display());
    println!("Archive: {}", report.codex_archive_root.display());
    println!("Index DB: {}", report.index_db_path.display());
    println!("Sessions discovered: {}", report.sessions_discovered);
    println!(
        "Sessions skipped this run: {}",
        report.sessions_skipped_this_run
    );
    println!(
        "Sessions currently indexed: {}",
        report.sessions_currently_indexed
    );
    println!(
        "Turns currently indexed: {}",
        report.turns_currently_indexed
    );
    println!("Skipped rollout files: {}", report.skipped_rollouts.len());

    Ok(())
}

impl From<SourceArg> for SourceKind {
    fn from(value: SourceArg) -> Self {
        match value {
            SourceArg::Claude => SourceKind::Claude,
            SourceArg::Codex => SourceKind::Codex,
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

/// Formats one skipped rollout warning for `memstack parse`.
fn format_skipped_rollout(skipped: &SkippedCodexRollout) -> String {
    let mut details = Vec::new();
    if let Some(session_id) = &skipped.logical_session_id {
        details.push(format!("session_id={session_id}"));
    }
    if let Some(cli_version) = &skipped.cli_version {
        details.push(format!("cli_version={cli_version}"));
    }
    if details.is_empty() {
        format!(
            "skipped Codex rollout {}: {}",
            skipped.source_path.display(),
            skipped.reason
        )
    } else {
        format!(
            "skipped Codex rollout {} ({}): {}",
            skipped.source_path.display(),
            details.join(", "),
            skipped.reason
        )
    }
}
