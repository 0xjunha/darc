use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use darc_core::{
    ClaudeSchemaAuditOptions, ClaudeSchemaAuditOutcome, ClaudeSchemaAuditReport,
    ClaudeSchemaSurveyMode, CodexSchemaAuditOptions, CodexSchemaAuditOutcome,
    CodexSchemaAuditReport, InitDraft, ParseOptions, SkippedRollout, SourceKind, SyncOptions,
    default_root_path, execute_sync, link_project, parse_project_sessions, prepare_init,
    prepare_sync, remove_project, rename_project, run_claude_schema_audit_with_progress,
    run_codex_schema_audit_with_progress, write_init,
};

#[derive(Debug, Parser)]
#[command(name = "darc", version, about = "Darc CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Supported CLI subcommands.
#[derive(Debug, Subcommand)]
enum Commands {
    /// Detect local sources and create the shared darc config.
    Init(InitArgs),
    #[command(
        about = "Link one configured project's historical paths into the current project",
        long_about = "Link one configured project's historical paths into the current project.\n\nRun this command from the target project directory.\nThe PROJECT argument is the old or source project name already stored in ~/.darc/config.toml.\n\nExample:\n- You renamed `/path/to/old-project` to `/path/to/new-project`.\n- Darc still has a configured project named `old-project`.\n- Run `cd /path/to/new-project && darc link old-project`.\n\nThis command is non-destructive.\nIt updates config so the current project knows the source project's old local_path and known_paths.\nIt does not run `darc sync`, `darc parse`, or remove the source project."
    )]
    Link(LinkArgs),
    #[command(
        about = "Remove one configured project and its archived/indexed data",
        long_about = "Remove one configured project and its archived/indexed data.\n\nThe PROJECT argument is matched against the configured project `name` in ~/.darc/config.toml.\nThe name must identify exactly one configured project.\n\nThis command deletes:\n- the project entry from config.toml\n- the project's archived sessions directory under ~/.darc/projects/...\n- the project's indexed SQLite rows\n\nYou can run this command from any directory."
    )]
    Remove(RemoveArgs),
    #[command(
        name = "rename-from",
        about = "Rebuild one old project's history into the current renamed project",
        long_about = "Rebuild one old project's history into the current renamed project.\n\nUse this when you just renamed a project from one name to another.\nRun the command from the new project directory, and pass the old project name.\n\nExample:\n- Darc config still contains a project named `old-project`.\n- You renamed the checkout to `/path/to/new-project`.\n- Run `cd /path/to/new-project && darc rename-from old-project`.\n\nThis command bootstraps or reuses the current project as the target, links the old project's paths into it, runs `darc sync`, runs `darc parse`, and removes the old source project after those steps succeed.\n\nIn other words, it is the safe built-in workflow for:\n`darc link <old-project> && darc sync && darc parse && darc remove <old-project>`\n\nIf ~/.darc/config.toml does not exist yet, run `darc init` first."
    )]
    RenameFrom(RenameArgs),
    /// Sync matching Claude and Codex sessions into the project archive.
    Sync(SyncArgs),
    /// Parse archived sessions from selected providers for the active project into SQLite.
    Parse(ParseArgs),
    #[command(
        hide = true,
        about = "Audit Codex rollout schema compatibility against stable release tags",
        long_about = "Audit Codex rollout schema compatibility against stable release tags.\n\nThe audit fetches release metadata from GitHub Releases and may hit GitHub API rate limits when run anonymously.\n\nGitHub API authentication:\n- Prefer GH_TOKEN when it is set.\n- Otherwise use GITHUB_TOKEN.\n- Personal access tokens are accepted."
    )]
    CodexSchemaAudit(CodexSchemaAuditArgs),
    #[command(
        hide = true,
        about = "Audit Claude rollout transcript compatibility against published npm releases",
        long_about = "Audit Claude rollout transcript compatibility against published npm releases.\n\nThe audit downloads published @anthropic-ai/claude-code packages from the npm registry, runs deterministic fixture prompts against each audited version, and derives transcript schema manifests from the emitted local JSONL transcripts.\n\nSecurity note:\n- Darc does not provide an OS-level sandbox for executing published Claude packages.\n- You must pass `--use-host-auth` to run this hidden maintainer command.\n- When you do, the downloaded package executes with your host Claude login state plus an allowlist of Claude/cloud provider auth environment variables, not your full shell environment.\n\nRuntime requirements:\n- A working `node` runtime must be installed locally.\n- A working Python runtime (`python3` or `python`) must be installed locally for hook capture.\n- Claude authentication must be available through an existing local Claude login or the supported auth environment variables."
    )]
    ClaudeSchemaAudit(ClaudeSchemaAuditArgs),
}

/// Detect local sources and create the shared darc config.
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

    #[arg(long = "provider", value_enum)]
    provider: Vec<ProviderArg>,
}

/// Link one configured project's historical paths into the active project.
#[derive(Debug, Args)]
struct LinkArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(value_name = "PROJECT")]
    project: String,
}

/// Remove one configured project and its archived/indexed data.
#[derive(Debug, Args)]
struct RemoveArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(value_name = "PROJECT")]
    project: String,
}

/// Rebuild one configured project's history under the active project's id, then remove the old project.
#[derive(Debug, Args)]
struct RenameArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(value_name = "PROJECT")]
    project: String,
}

/// Parse archived sessions from selected providers for the active project into SQLite.
#[derive(Debug, Args)]
struct ParseArgs {
    #[arg(long, default_value_os_t = default_root_path())]
    root: PathBuf,

    #[arg(long = "provider", value_enum)]
    provider: Vec<ProviderArg>,
}

/// Audit Codex rollout schema compatibility against stable release tags.
#[derive(Debug, Args)]
struct CodexSchemaAuditArgs {
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,
}

/// Audit Claude rollout transcript compatibility against published npm releases.
#[derive(Debug, Args)]
struct ClaudeSchemaAuditArgs {
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,

    #[arg(long, default_value_t = 1, value_name = "N")]
    sample_stride: usize,

    #[arg(long)]
    use_host_auth: bool,

    #[arg(long, value_name = "VERSION")]
    from_version: Option<String>,

    #[arg(long, value_enum, default_value_t = ClaudeSurveyModeArg::Refine)]
    survey_mode: ClaudeSurveyModeArg,
}

/// Represents the supported provider filters for parse and sync.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderArg {
    Claude,
    Codex,
}

/// Represents the supported Claude schema audit survey modes.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ClaudeSurveyModeArg {
    Refine,
    Coarse,
}

fn main() {
    std::process::exit(run());
}

/// Parses CLI arguments and dispatches the selected command.
fn run() -> i32 {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init(args) => standard_exit(run_init(args)),
        Commands::Link(args) => standard_exit(run_link(args)),
        Commands::Remove(args) => standard_exit(run_remove(args)),
        Commands::RenameFrom(args) => standard_exit(run_rename_from(args)),
        Commands::Sync(args) => standard_exit(run_sync(args)),
        Commands::Parse(args) => standard_exit(run_parse(args)),
        Commands::CodexSchemaAudit(args) => run_codex_schema_audit_command(args),
        Commands::ClaudeSchemaAudit(args) => run_claude_schema_audit_command(args),
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
                "Dry run only. Existing darc config was left unchanged.".to_owned()
            } else {
                "Dry run only. Project was not added to darc.".to_owned()
            }
        } else {
            "Dry run only. Global darc config and project registration were not written.".to_owned()
        };
    }

    let mut lines = Vec::new();
    if !draft.global_config_exists {
        lines.push("Initialized global darc config.".to_owned());
    }
    lines.push(if draft.project_exists {
        "Project is already configured in darc.".to_owned()
    } else {
        "Added project to darc.".to_owned()
    });
    lines.join("\n")
}

/// Links one configured project's historical paths into the active project.
fn run_link(args: LinkArgs) -> Result<()> {
    let report = link_project(Some(args.root), &args.project)?;

    println!("Project: {}", report.target_project_name);
    println!("Linked From: {}", report.source_project_name);
    println!("Project Root: {}", report.target_project_root.display());
    println!(
        "Known paths: {} total, {} added",
        report.total_known_paths,
        report.new_known_paths.len()
    );
    if report.config_written {
        println!("Updated config.");
    } else {
        println!("Config already covered all linked paths.");
    }

    Ok(())
}

/// Removes one configured project and its archived/indexed data.
fn run_remove(args: RemoveArgs) -> Result<()> {
    let report = remove_project(Some(args.root), &args.project)?;

    println!("Project: {}", report.project_name);
    println!("Project ID: {}", report.project_id);
    println!("Archive: {}", report.sessions_root.display());
    println!(
        "Indexed sessions removed: {}",
        report.indexed_sessions_removed
    );
    println!("Indexed turns removed: {}", report.indexed_turns_removed);
    if report.archive_deleted {
        println!("Deleted archive.");
    } else {
        println!("Archive did not exist.");
    }
    if report.config_written {
        println!("Updated config.");
    }

    Ok(())
}

/// Rebuilds one configured project's history under the active project's id.
fn run_rename_from(args: RenameArgs) -> Result<()> {
    let report = rename_project(Some(args.root), &args.project)?;

    println!("Project: {}", report.link.target_project_name);
    println!("Renamed From: {}", report.link.source_project_name);
    println!(
        "Known paths: {} total, {} added",
        report.link.total_known_paths,
        report.link.new_known_paths.len()
    );
    println!(
        "Synced {} session files and {} auxiliary files.",
        report.sync.sessions_copied, report.sync.auxiliary_copied
    );
    println!(
        "Parsed {} discovered sessions into {} indexed sessions and {} turns.",
        report.parse.sessions_discovered,
        report.parse.sessions_currently_indexed,
        report.parse.turns_currently_indexed
    );
    println!(
        "Removed old project archive and {} indexed sessions.",
        report.remove.indexed_sessions_removed
    );

    Ok(())
}

/// Prepares and optionally executes the project-scoped sync workflow.
fn run_sync(args: SyncArgs) -> Result<()> {
    let plan = prepare_sync(
        Some(args.root),
        SyncOptions {
            provider_filter: args.provider.into_iter().map(ProviderArg::into).collect(),
        },
    )?;

    println!("Project: {}", plan.project_name);
    println!("Project Root: {}", plan.project_root.display());
    println!("Archive: {}", plan.sessions_root.display());
    println!("Providers: {}", format_sources(&plan.sources));
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

/// Parses archived sessions for the active project into SQLite.
fn run_parse(args: ParseArgs) -> Result<()> {
    let report = parse_project_sessions(
        Some(args.root),
        ParseOptions {
            provider_filter: args.provider.into_iter().map(ProviderArg::into).collect(),
        },
    )?;

    for skipped in &report.skipped_rollouts {
        eprintln!("warning: {}", format_skipped_rollout(skipped));
    }

    println!("Project: {}", report.project_name);
    println!("Project Root: {}", report.project_root.display());
    println!("Archive: {}", report.sessions_root.display());
    println!("Providers: {}", format_sources(&report.providers));
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

/// Runs the hidden Claude rollout schema audit command with dedicated exit codes.
fn run_claude_schema_audit_command(args: ClaudeSchemaAuditArgs) -> i32 {
    match run_claude_schema_audit_with_progress(
        ClaudeSchemaAuditOptions {
            cache_dir: args.cache_dir,
            use_host_auth: args.use_host_auth,
            sample_stride: args.sample_stride,
            from_version: args.from_version,
            survey_mode: args.survey_mode.into(),
        },
        |message| eprintln!("[claude-schema-audit] {message}"),
    ) {
        Ok(report) => {
            println!("{}", format_claude_schema_audit_report(&report));
            claude_schema_audit_exit_code(&report)
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            2
        }
    }
}

impl From<ProviderArg> for SourceKind {
    fn from(value: ProviderArg) -> Self {
        match value {
            ProviderArg::Claude => SourceKind::Claude,
            ProviderArg::Codex => SourceKind::Codex,
        }
    }
}

impl From<ClaudeSurveyModeArg> for ClaudeSchemaSurveyMode {
    fn from(value: ClaudeSurveyModeArg) -> Self {
        match value {
            ClaudeSurveyModeArg::Refine => ClaudeSchemaSurveyMode::Refine,
            ClaudeSurveyModeArg::Coarse => ClaudeSchemaSurveyMode::Coarse,
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

/// Formats one skipped rollout warning for `darc parse`.
fn format_skipped_rollout(skipped: &SkippedRollout) -> String {
    let mut details = Vec::new();
    if let Some(session_id) = &skipped.logical_session_id {
        details.push(format!("session_id={session_id}"));
    }
    if let Some(cli_version) = &skipped.cli_version {
        details.push(format!("cli_version={cli_version}"));
    }
    if details.is_empty() {
        format!(
            "skipped {} rollout {}: {}",
            skipped.provider.title(),
            skipped.source_path.display(),
            skipped.reason
        )
    } else {
        format!(
            "skipped {} rollout {} ({}): {}",
            skipped.provider.title(),
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

/// Returns the CLI exit code for one Claude schema audit report.
fn claude_schema_audit_exit_code(report: &ClaudeSchemaAuditReport) -> i32 {
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
            "Latest Exact-Covered Darc Version: {}",
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
            lines.push("Likely Darc Files To Update:".to_owned());
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

/// Formats one Claude schema audit report for the hidden CLI command.
fn format_claude_schema_audit_report(report: &ClaudeSchemaAuditReport) -> String {
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
            "Latest Published Claude Version: {}",
            report.latest_published_version
        ),
        format!(
            "Latest Exact-Covered Darc Version: {}",
            report.latest_exact_covered_version
        ),
        format!("Audited Version Range: {}", report.audited_version_range()),
        format!(
            "Inspected Versions: {}",
            report.inspected_versions.join(", ")
        ),
        format!("Sampling Stride: {}", report.sample_stride),
        format!(
            "Survey Mode: {}",
            match report.survey_mode {
                ClaudeSchemaSurveyMode::Refine => "refine",
                ClaudeSchemaSurveyMode::Coarse => "coarse",
            }
        ),
        format!(
            "Auth Mode: {}",
            if report.used_host_auth {
                "host (explicit opt-in)"
            } else {
                "isolated (no auth)"
            }
        ),
    ];

    match &report.outcome {
        ClaudeSchemaAuditOutcome::Compatible => {
            if report.sample_stride == 1 {
                lines.push(format!(
                    "Compatible across {} audited Claude version(s).",
                    report.audited_versions.len()
                ));
            } else {
                lines.push(format!(
                    "Compatible across {} Claude version(s) in range with {} directly inspected version(s).",
                    report.audited_versions.len(),
                    report.inspected_versions.len()
                ));
            }
        }
        ClaudeSchemaAuditOutcome::Drift(drift) => {
            lines.push(format!(
                "First Drift Version: {}",
                drift.first_drift_version
            ));
            lines.push("Transcript Differences:".to_owned());
            lines.extend(
                drift
                    .difference_summary
                    .iter()
                    .map(|line| format!("- {line}")),
            );
            lines.push("Likely Darc Files To Update:".to_owned());
            lines.extend(
                drift
                    .likely_files_to_update
                    .iter()
                    .map(|path| format!("- {path}")),
            );
        }
    }

    if let Some(drift) = &report.supplementary_sdk_drift {
        lines.push(format!(
            "Supplementary Agent SDK Drift Version: {}",
            drift.first_drift_version
        ));
        lines.push("Supplementary Agent SDK Differences:".to_owned());
        lines.extend(
            drift
                .difference_summary
                .iter()
                .map(|line| format!("- {line}")),
        );
    }

    if !report.assumed_compatible_intervals.is_empty() {
        lines.push("Assumed Compatible Unsampled Intervals:".to_owned());
        lines.extend(
            report
                .assumed_compatible_intervals
                .iter()
                .map(|line| format!("- {line}")),
        );
    }

    if !report.transcript_drift_windows.is_empty() {
        lines.push("Sampled Transcript Drift Windows:".to_owned());
        lines.extend(report.transcript_drift_windows.iter().map(|window| {
            format!(
                "- {} ..= {} (sampled compatible {}, sampled drift {})",
                window.window_start_version,
                window.window_end_version,
                window.sampled_compatible_version,
                window.sampled_drift_version
            )
        }));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};
    use darc_core::{
        ClaudeSchemaAuditReport, ClaudeSchemaDrift, ClaudeSchemaDriftWindow,
        ClaudeSchemaSurveyMode, ClaudeSdkSchemaDrift, CodexSchemaAuditReport, CodexSchemaDrift,
    };

    use super::{
        ClaudeSchemaAuditOutcome, Cli, CodexSchemaAuditOutcome, Commands,
        claude_schema_audit_exit_code, codex_schema_audit_exit_code,
        format_claude_schema_audit_report, format_codex_schema_audit_report,
    };

    fn compatible_report() -> CodexSchemaAuditReport {
        CodexSchemaAuditReport {
            release_source: "GitHub Releases (openai/codex)".to_owned(),
            binary_cache_dir: "/tmp/darc-cache".into(),
            latest_stable_release_tag: "rust-v0.118.0".to_owned(),
            latest_exact_covered_version: "0.118.0".to_owned(),
            audited_tags: vec!["rust-v0.118.0".to_owned()],
            outcome: CodexSchemaAuditOutcome::Compatible,
        }
    }

    fn compatible_claude_report() -> ClaudeSchemaAuditReport {
        ClaudeSchemaAuditReport {
            release_source: "npm registry (@anthropic-ai/claude-code)".to_owned(),
            binary_cache_dir: "/tmp/darc-claude-cache".into(),
            latest_published_version: "2.1.92".to_owned(),
            latest_exact_covered_version: "2.1.87".to_owned(),
            audited_versions: vec!["2.1.92".to_owned(), "2.1.87".to_owned()],
            inspected_versions: vec!["2.1.92".to_owned(), "2.1.87".to_owned()],
            assumed_compatible_intervals: Vec::new(),
            sample_stride: 1,
            used_host_auth: false,
            survey_mode: ClaudeSchemaSurveyMode::Refine,
            transcript_drift_windows: Vec::new(),
            outcome: ClaudeSchemaAuditOutcome::Compatible,
            supplementary_sdk_drift: Some(ClaudeSdkSchemaDrift {
                first_drift_version: "2.1.92".to_owned(),
                difference_summary: vec![
                    "$/agent_sdk_version: changed from \"0.2.87\" to \"0.2.92\"".to_owned(),
                ],
            }),
        }
    }

    #[test]
    fn parses_hidden_codex_schema_audit_command() {
        let cli = Cli::try_parse_from(["darc", "codex-schema-audit"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::CodexSchemaAudit(super::CodexSchemaAuditArgs { .. })
        ));
    }

    #[test]
    fn parses_hidden_claude_schema_audit_command() {
        let cli = Cli::try_parse_from([
            "darc",
            "claude-schema-audit",
            "--sample-stride",
            "10",
            "--use-host-auth",
            "--from-version",
            "2.1.84",
            "--survey-mode",
            "coarse",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::ClaudeSchemaAudit(super::ClaudeSchemaAuditArgs {
                sample_stride,
                use_host_auth,
                from_version,
                survey_mode,
                ..
            }) if sample_stride == 10
                && use_host_auth
                && from_version.as_deref() == Some("2.1.84")
                && matches!(survey_mode, super::ClaudeSurveyModeArg::Coarse)
        ));
    }

    #[test]
    fn parse_command_accepts_provider_filters() {
        let cli = Cli::try_parse_from(["darc", "parse", "--provider", "claude"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Parse(super::ParseArgs { provider, .. }) if provider.len() == 1
        ));
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
    fn claude_schema_audit_help_mentions_explicit_host_auth() {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("claude-schema-audit")
            .expect("hidden subcommand should still be present")
            .render_long_help()
            .to_string();

        assert!(help.contains("--use-host-auth"));
        assert!(help.contains("does not provide an OS-level sandbox"));
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
        assert!(output.contains("Likely Darc Files To Update:"));
    }

    #[test]
    fn formats_compatible_claude_schema_audit_report() {
        let report = compatible_claude_report();
        let output = format_claude_schema_audit_report(&report);

        assert_eq!(claude_schema_audit_exit_code(&report), 0);
        assert!(output.contains("Status: compatible"));
        assert!(output.contains("Latest Published Claude Version: 2.1.92"));
        assert!(output.contains("Compatible across 2 audited Claude version(s)."));
        assert!(output.contains("Supplementary Agent SDK Drift Version: 2.1.92"));
        assert!(output.contains("Sampling Stride: 1"));
        assert!(output.contains("Survey Mode: refine"));
        assert!(output.contains("Auth Mode: isolated (no auth)"));
    }

    #[test]
    fn formats_drift_claude_schema_audit_report() {
        let report = ClaudeSchemaAuditReport {
            survey_mode: ClaudeSchemaSurveyMode::Coarse,
            transcript_drift_windows: vec![ClaudeSchemaDriftWindow {
                window_start_version: "2.1.88".to_owned(),
                window_end_version: "2.1.90".to_owned(),
                sampled_compatible_version: "2.1.87".to_owned(),
                sampled_drift_version: "2.1.90".to_owned(),
                difference_summary: vec![
                    "$/line_types: array length changed from 4 to 5".to_owned(),
                ],
            }],
            outcome: ClaudeSchemaAuditOutcome::Drift(ClaudeSchemaDrift {
                first_drift_version: "2.1.90".to_owned(),
                difference_summary: vec![
                    "$/line_types[2]: changed from \"system\" to \"mystery-event\"".to_owned(),
                ],
                likely_files_to_update: vec![
                    "crates/core/src/rollout/claude/version.rs".to_owned(),
                    "crates/core/src/rollout/claude/mod.rs".to_owned(),
                ],
            }),
            supplementary_sdk_drift: None,
            ..compatible_claude_report()
        };
        let output = format_claude_schema_audit_report(&report);

        assert_eq!(claude_schema_audit_exit_code(&report), 1);
        assert!(output.contains("Status: schema drift detected"));
        assert!(output.contains("First Drift Version: 2.1.90"));
        assert!(output.contains("Likely Darc Files To Update:"));
        assert!(output.contains("Survey Mode: coarse"));
        assert!(output.contains("Sampled Transcript Drift Windows:"));
    }
}
