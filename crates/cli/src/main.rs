use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use memstack_core::{default_root_path, prepare_init, write_init};

#[derive(Debug, Parser)]
#[command(name = "memstack", version, about = "memstack")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Supported CLI subcommands.
#[derive(Debug, Subcommand)]
enum Commands {
    Init(InitArgs),
}

/// Collects arguments for the `init` subcommand.
#[derive(Debug, Args)]
struct InitArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(long)]
    dry_run: bool,
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
