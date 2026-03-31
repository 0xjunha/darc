use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use memstack_core::{
    SourceKind, SyncOptions, default_root_path, execute_sync, prepare_init, prepare_sync,
    write_init,
};

#[derive(Debug, Parser)]
#[command(name = "memstack", version, about = "memstack")]
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
        println!("\nDry run only. Config was not written.\n");
        println!("{}", draft.config_toml()?);
    } else {
        println!(
            "\n{} config.",
            if draft.config_exists {
                "Updated"
            } else {
                "Created"
            }
        );
    }

    Ok(())
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
    println!("Root: {}", plan.project_root.display());
    println!("Archive: {}", plan.sessions_root.display());
    println!("Sources: {}", format_sources(&plan.sources));
    println!(
        "Sessions: {} to copy, {} unchanged",
        plan.sessions_to_copy, plan.sessions_unchanged
    );
    println!(
        "Auxiliary: {} to copy, {} unchanged",
        plan.auxiliary_to_copy, plan.auxiliary_unchanged
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
