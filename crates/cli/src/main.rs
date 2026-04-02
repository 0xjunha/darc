use std::{fs, path::Path, path::PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use memstack_core::{
    CodexRollout, CodexTurn, CodexTurnStatus, CodexTurnStep, InitDraft, SourceKind, SyncOptions,
    default_root_path, execute_sync, parse_codex_rollout, parse_project_codex_turns, prepare_init,
    prepare_sync, write_init,
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
    /// Parse one archived Codex rollout and print its reconstructed turns.
    InspectCodex(InspectCodexArgs),
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

/// Parse one archived Codex rollout and print its reconstructed turns.
#[derive(Debug, Args)]
struct InspectCodexArgs {
    #[arg(value_name = "ROLLOUT")]
    rollout: PathBuf,

    #[arg(long, value_name = "PATH")]
    output: Option<PathBuf>,
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
        Commands::InspectCodex(args) => run_inspect_codex(args),
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

    println!("Project: {}", report.project_name);
    println!("Project Root: {}", report.project_root.display());
    println!("Archive: {}", report.codex_archive_root.display());
    println!("Index DB: {}", report.index_db_path.display());
    println!("Sessions indexed: {}", report.sessions_indexed);
    println!("Turns indexed: {}", report.turns_indexed);

    Ok(())
}

/// Parses one archived Codex rollout and prints reconstructed turns.
fn run_inspect_codex(args: InspectCodexArgs) -> Result<()> {
    let rollout = parse_codex_rollout(&args.rollout)?;

    if let Some(output) = &args.output {
        write_rollout_json(output, &rollout)?;
        println!("Wrote parsed rollout JSON to {}", output.display());
        return Ok(());
    }

    println!("Session: {}", rollout.session_id);
    println!("Source: {}", args.rollout.display());
    println!("Cwd: {}", rollout.cwd.display());
    println!("CLI Version: {}", rollout.cli_version);
    println!("Schema: {}", rollout.schema_id);
    println!("Determinism: {:?}", rollout.determinism);
    println!("Turns: {}", rollout.turns.len());

    for (index, turn) in rollout.turns.iter().enumerate() {
        print_turn(index + 1, turn);
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

/// Writes one parsed rollout to a pretty-printed JSON file.
fn write_rollout_json(path: &Path, rollout: &CodexRollout) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let content =
        serde_json::to_vec_pretty(rollout).context("failed to serialize parsed rollout")?;
    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Prints one parsed Codex turn in a readable CLI form.
fn print_turn(index: usize, turn: &CodexTurn) {
    println!("\nTurn {index}");
    println!("Status: {}", format_turn_status(turn.status));
    if let Some(turn_id) = &turn.turn_id {
        println!("Turn ID: {turn_id}");
    }
    println!("Started: {}", turn.started_at);
    if let Some(completed_at) = &turn.completed_at {
        println!("Completed: {completed_at}");
    }
    print_text_block("User", &turn.user_message);
    if let Some(final_answer) = &turn.final_answer {
        print_text_block("Final Answer", &final_answer.text);
    }

    for step in &turn.steps {
        match step {
            CodexTurnStep::Reasoning {
                summary, encrypted, ..
            } => {
                let details = if summary.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", summary.join(", "))
                };
                println!(
                    "Reasoning: {}{}",
                    if *encrypted { "encrypted" } else { "plain" },
                    details
                );
            }
            CodexTurnStep::Commentary { text, .. } => print_text_block("Commentary", text),
            CodexTurnStep::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                println!("Tool Call: {name} ({call_id})");
                print_text_block("Arguments", arguments);
            }
            CodexTurnStep::ToolCallOutput {
                call_id, output, ..
            } => {
                println!("Tool Output: {call_id}");
                print_text_block("Output", output);
            }
            CodexTurnStep::ProviderResponseItem {
                item_type,
                payload_json,
                ..
            } => {
                println!("Provider Response Item: {item_type}");
                print_text_block("Payload", payload_json);
            }
        }
    }
}

/// Formats a turn status for human-readable CLI output.
fn format_turn_status(status: CodexTurnStatus) -> &'static str {
    match status {
        CodexTurnStatus::Completed => "completed",
        CodexTurnStatus::Aborted => "aborted",
        CodexTurnStatus::Incomplete => "incomplete",
    }
}

/// Prints a multi-line text block with a stable indentation.
fn print_text_block(label: &str, text: &str) {
    println!("{label}:");
    for line in text.lines() {
        println!("  {line}");
    }
    if text.is_empty() {
        println!("  ");
    }
}
