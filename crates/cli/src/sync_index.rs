use super::*;

/// Prepares and optionally executes the project-scoped sync workflow.
pub(crate) fn run_sync(args: SyncArgs) -> Result<()> {
    let plan = prepare_sync(
        Some(args.root),
        SyncOptions {
            provider_filter: args.provider.into_iter().map(ProviderArg::into).collect(),
        },
    )
    .map_err(add_init_hint_for_unconfigured_project)?;
    let style = HumanStyle::stdout();

    print_project_run_header(
        style,
        "Sync",
        &plan.project_name,
        &plan.project_root,
        Some(plan.sessions_root.as_path()),
    );
    println!();
    print_section(style, "Plan");
    print_field(style, 2, "Providers", format_sources(&plan.sources));
    print_field(
        style,
        2,
        "Sessions",
        format!(
            "{} to copy, {} unchanged",
            style.count(plan.sessions_to_copy()),
            style.count(plan.sessions_unchanged)
        ),
    );
    print_field(
        style,
        2,
        "Auxiliary",
        format!(
            "{} to copy, {} unchanged",
            style.count(plan.auxiliary_to_copy()),
            style.count(plan.auxiliary_unchanged)
        ),
    );
    print_field(
        style,
        2,
        "Known paths",
        format!("{} new", style.count(plan.new_known_paths.len())),
    );
    for warning in &plan.warnings {
        print_warning(warning);
    }

    if args.dry_run {
        println!();
        print_section(style, "Status");
        print_field(style, 2, "Overall", style.warn("dry run only"));
        print_line(2, style.muted("No files were written."));
        return Ok(());
    }

    let report = execute_sync(plan)?;
    println!();
    print_sync_result(style, &report);
    println!();
    print_section(style, "Status");
    print_field(style, 2, "Overall", style.ok("synced"));

    Ok(())
}

/// Prints the common project/path header for human workflow commands.
pub(crate) fn print_project_run_header(
    style: HumanStyle,
    title: &str,
    project_name: &str,
    project_root: &std::path::Path,
    archive: Option<&std::path::Path>,
) {
    print_section(style, title);
    print_field(style, 2, "Project", project_name);
    print_field(style, 2, "Project root", style.path(project_root.display()));
    if let Some(archive) = archive {
        print_field(style, 2, "Archive", style.path(archive.display()));
    }
}

/// Prints one executed sync summary block.
pub(crate) fn print_sync_result(style: HumanStyle, report: &SyncReport) {
    print_section(style, "Sync");
    print_field(
        style,
        2,
        "Sessions",
        format!(
            "{} copied, {} unchanged",
            style.count(report.sessions_copied),
            style.count(report.sessions_unchanged)
        ),
    );
    print_field(
        style,
        2,
        "Auxiliary",
        format!(
            "{} copied, {} unchanged",
            style.count(report.auxiliary_copied),
            style.count(report.auxiliary_unchanged)
        ),
    );
    print_field(
        style,
        2,
        "Known paths",
        format!("{} new", style.count(report.new_known_paths.len())),
    );
}

/// Prints one index summary block.
pub(crate) fn print_index_summary(style: HumanStyle, report: &IndexReport) {
    print_section(style, "Indexed Data");
    print_field(style, 2, "Providers", format_sources(&report.providers));
    print_field(
        style,
        2,
        "Index DB",
        style.path(report.index_db_path.display()),
    );
    print_field(
        style,
        2,
        "Sessions discovered",
        style.count(report.sessions_discovered),
    );
    print_field(
        style,
        2,
        "Sessions skipped this run",
        style.count(report.sessions_skipped_this_run),
    );
    print_field(
        style,
        2,
        "Sessions currently indexed",
        style.count(report.sessions_currently_indexed),
    );
    print_field(
        style,
        2,
        "Turns currently indexed",
        style.count(report.turns_currently_indexed),
    );
    let skipped = report.skipped_rollouts.len();
    let skipped = if skipped == 0 {
        style.ok(skipped)
    } else {
        style.warn(skipped)
    };
    print_field(style, 2, "Skipped rollout files", skipped);
}

/// Adds a `darc init` hint when sync or refresh runs outside a configured project.
pub(crate) fn add_init_hint_for_unconfigured_project(error: anyhow::Error) -> anyhow::Error {
    if error.chain().any(|cause| {
        cause.to_string() == "current directory does not match any configured darc project"
    }) {
        anyhow::anyhow!(
            "{error:#}\nrun `darc init` from this project root first (reuse the same `--root` flag if you passed one here)"
        )
    } else {
        error
    }
}

/// Indexes archived sessions for the active project into SQLite.
pub(crate) fn run_index(args: IndexArgs) -> Result<()> {
    let report = index_project_sessions(
        Some(args.root),
        IndexOptions {
            provider_filter: args.provider.into_iter().map(ProviderArg::into).collect(),
        },
    )?;
    let style = HumanStyle::stdout();

    for skipped in &report.skipped_rollouts {
        print_warning(format_skipped_rollout(skipped));
    }

    print_project_run_header(
        style,
        "Index",
        &report.project_name,
        &report.project_root,
        Some(report.sessions_root.as_path()),
    );
    println!();
    print_index_summary(style, &report);
    println!();
    print_section(style, "Status");
    let status = if report.skipped_rollouts.is_empty() {
        style.ok("indexed")
    } else {
        style.warn("indexed with skipped rollouts")
    };
    print_field(style, 2, "Overall", status);

    Ok(())
}

/// Formats a source list for compact CLI output.
pub(crate) fn format_sources(sources: &[SourceKind]) -> String {
    sources
        .iter()
        .map(|source| source.title())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Formats one skipped rollout warning for `darc index`.
pub(crate) fn format_skipped_rollout(skipped: &SkippedRollout) -> String {
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
