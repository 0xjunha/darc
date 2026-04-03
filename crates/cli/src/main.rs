use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use memstack_core::{
    CodexSchemaAuditOptions, CodexSchemaAuditOutcome, CodexSchemaAuditReport, InitDraft,
    SkippedCodexRollout, SourceKind, SyncOptions, default_root_path, execute_sync,
    parse_project_codex_turns, prepare_init, prepare_sync, run_codex_schema_audit_with_progress,
    write_init,
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
    #[command(
        hide = true,
        about = "Audit Codex rollout schema compatibility against stable release tags",
        long_about = "Audit Codex rollout schema compatibility against stable release tags.\n\nThe audit fetches release metadata from GitHub Releases and may hit GitHub API rate limits when run anonymously.\n\nGitHub API authentication:\n- Prefer GH_TOKEN when it is set.\n- Otherwise use GITHUB_TOKEN.\n- Personal access tokens are accepted."
    )]
    CodexSchemaAudit(CodexSchemaAuditArgs),
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

/// Audit Codex rollout schema compatibility against stable release tags.
#[derive(Debug, Args)]
struct CodexSchemaAuditArgs {
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,
}

/// Represents the supported source filters for `sync`.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum SourceArg {
    Claude,
    Codex,
}

fn main() {
    std::process::exit(run());
}

/// Parses CLI arguments and dispatches the selected command.
fn run() -> i32 {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init(args) => standard_exit(run_init(args)),
        Commands::Sync(args) => standard_exit(run_sync(args)),
        Commands::Parse(args) => standard_exit(run_parse(args)),
        Commands::CodexSchemaAudit(args) => run_codex_schema_audit_command(args),
    }
}

/// Maps standard command results to the default CLI exit code convention.
fn standard_exit(result: Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error:#}");
            1
        }
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

/// Runs the hidden Codex rollout schema audit command with dedicated exit codes.
fn run_codex_schema_audit_command(args: CodexSchemaAuditArgs) -> i32 {
    match run_codex_schema_audit_with_progress(
        CodexSchemaAuditOptions {
            cache_dir: args.cache_dir,
        },
        |message| eprintln!("[codex-schema-audit] {message}"),
    ) {
        Ok(report) => {
            println!("{}", format_codex_schema_audit_report(&report));
            codex_schema_audit_exit_code(&report)
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            2
        }
    }
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

/// Returns the CLI exit code for one Codex schema audit report.
fn codex_schema_audit_exit_code(report: &CodexSchemaAuditReport) -> i32 {
    if report.is_compatible() { 0 } else { 1 }
}

/// Formats one Codex schema audit report for the hidden CLI command.
fn format_codex_schema_audit_report(report: &CodexSchemaAuditReport) -> String {
    let mut lines = vec![
        format!(
            "Status: {}",
            if report.is_compatible() {
                "compatible"
            } else {
                "schema drift detected"
            }
        ),
        format!("Release Source: {}", report.release_source),
        format!("Binary Cache: {}", report.binary_cache_dir.display()),
        format!(
            "Latest Stable Codex Release Tag: {}",
            report.latest_stable_release_tag
        ),
        format!(
            "Latest Exact-Covered Memstack Version: {}",
            report.latest_exact_covered_version
        ),
        format!("Audited Tag Range: {}", report.audited_tag_range()),
    ];

    match &report.outcome {
        CodexSchemaAuditOutcome::Compatible => {
            lines.push(format!(
                "Compatible across {} audited stable release tag(s).",
                report.audited_tags.len()
            ));
        }
        CodexSchemaAuditOutcome::Drift(drift) => {
            lines.push(format!("First Drift Tag: {}", drift.first_drift_tag));
            lines.push("Schema Differences:".to_owned());
            lines.extend(
                drift
                    .difference_summary
                    .iter()
                    .map(|line| format!("- {line}")),
            );
            lines.push("Likely Memstack Files To Update:".to_owned());
            lines.extend(
                drift
                    .likely_files_to_update
                    .iter()
                    .map(|path| format!("- {path}")),
            );
        }
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};
    use memstack_core::{CodexSchemaAuditReport, CodexSchemaDrift};

    use super::{
        Cli, CodexSchemaAuditOutcome, Commands, codex_schema_audit_exit_code,
        format_codex_schema_audit_report,
    };

    fn compatible_report() -> CodexSchemaAuditReport {
        CodexSchemaAuditReport {
            release_source: "GitHub Releases (openai/codex)".to_owned(),
            binary_cache_dir: "/tmp/memstack-cache".into(),
            latest_stable_release_tag: "rust-v0.118.0".to_owned(),
            latest_exact_covered_version: "0.118.0".to_owned(),
            audited_tags: vec!["rust-v0.118.0".to_owned()],
            outcome: CodexSchemaAuditOutcome::Compatible,
        }
    }

    #[test]
    fn parses_hidden_codex_schema_audit_command() {
        let cli = Cli::try_parse_from(["memstack", "codex-schema-audit"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::CodexSchemaAudit(super::CodexSchemaAuditArgs { .. })
        ));
    }

    #[test]
    fn codex_schema_audit_help_mentions_github_tokens() {
        let mut command = Cli::command();
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
        assert!(output.contains("Likely Memstack Files To Update:"));
    }
}
