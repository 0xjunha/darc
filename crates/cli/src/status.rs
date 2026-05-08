use super::*;

/// Shows Darc status for the active project or shared workspace.
pub(crate) fn run_status(args: StatusArgs) -> Result<()> {
    if args.workspace {
        let report = status_workspace(Some(args.root), args.check)?;
        if args.json {
            let output = QueryOutput::new(ColorArg::Never);
            print_json_envelope(&output, "darc.status.workspace.v1", &report)?;
            return status_check_exit(
                report.has_failed_check(),
                "workspace",
                "workspace status check failed",
            );
        }
        print_workspace_status(&report);
        return status_check_exit(
            report.has_failed_check(),
            "workspace",
            "workspace status check failed",
        );
    }

    let report = status_project(Some(args.root), args.check)
        .map_err(add_init_hint_for_unconfigured_project)?;
    if args.json {
        let output = QueryOutput::new(ColorArg::Never);
        print_json_envelope(&output, "darc.status.project.v1", &report)?;
        return status_check_exit(report.has_failed_check(), "project", "status check failed");
    }
    print_project_status(&report);
    status_check_exit(report.has_failed_check(), "project", "status check failed")
}

/// Converts an optional status sync-check failure into the final CLI exit result.
fn status_check_exit(
    has_failed_check: bool,
    scope: &'static str,
    message: &'static str,
) -> Result<()> {
    if has_failed_check {
        return Err(StatusJsonError::check_failed(scope, message).into());
    }
    Ok(())
}

/// Prints one active-project status report.
fn print_project_status(report: &darc_core::ProjectStatusReport) {
    let style = HumanStyle::stdout();
    print_status_header(style, &report.root, None);
    println!();
    print_sources(style, &report.sources);
    println!();
    print_active_project_identity(style, &report.project);
    println!();
    print_project_index_status(style, &report.project, 0);
    if report.project.sync_check.is_some() {
        println!();
        print_sync_check(style, report.project.sync_check.as_ref(), "Sync Check", 0);
    }
    if !report.project.issues.is_empty() {
        println!();
        print_project_issues(style, &report.project, 0);
    }
    println!();
    print_overall_status(
        style,
        format_overall_status(
            &report.root.issues,
            &report.sources,
            std::slice::from_ref(&report.project),
        ),
    );
}

/// Prints one workspace status report.
fn print_workspace_status(report: &WorkspaceStatusReport) {
    let style = HumanStyle::stdout();
    print_status_header(style, &report.root, Some(report.projects.len()));
    println!();
    print_sources(style, &report.sources);
    println!();
    print_workspace_summary(style, report);
    println!();
    print_workspace_projects(style, &report.projects);
    println!();
    print_overall_status(
        style,
        format_overall_status(&report.root.issues, &report.sources, &report.projects),
    );
}

/// Returns one archive availability label.
fn archive_status(style: HumanStyle, project: &StatusProject) -> String {
    if project.archive_exists {
        style.ok("ok")
    } else {
        style.error("missing")
    }
}

/// Returns one configured-source state label.
fn source_state(style: HumanStyle, source: &StatusSource) -> String {
    if !source.configured {
        style.muted("not configured")
    } else if source.enabled {
        style.ok("enabled")
    } else {
        style.muted("disabled")
    }
}

/// Returns one configured-source path availability label.
fn source_path_state(style: HumanStyle, source: &StatusSource) -> String {
    if source.path_exists {
        style.ok("ok")
    } else {
        style.error("missing")
    }
}

/// Returns one configured-source path label.
fn source_path(style: HumanStyle, source: &StatusSource) -> String {
    let path = source
        .path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "none".to_owned());
    style.path(path)
}

/// Returns one formatted source path with availability.
fn source_path_with_state(style: HumanStyle, source: &StatusSource) -> String {
    format!(
        "{} ({})",
        source_path(style, source),
        source_path_state(style, source)
    )
}

/// Returns one formatted indexed count summary.
fn indexed_summary(style: HumanStyle, project: &StatusProject) -> String {
    format!(
        "{} sessions, {} turns",
        style.count(project.session_count),
        style.count(project.turn_count)
    )
}

/// Prints the common root/config/database status header.
fn print_status_header(
    style: HumanStyle,
    root: &darc_core::query::RootInfo,
    project_count: Option<usize>,
) {
    print_section(style, "Darc");
    print_field(style, 2, "Version", env!("CARGO_PKG_VERSION"));
    print_field(
        style,
        2,
        "Root",
        style.path(root.resolved_root_path.display()),
    );
    let config_status = if !root.available.config_exists {
        style.error("missing")
    } else {
        match project_count {
            Some(count) => style.ok(format!(
                "ok ({})",
                count_label(count, "project", "projects")
            )),
            None => style.ok("ok"),
        }
    };
    print_field(style, 2, "Config", config_status);
    print_field(
        style,
        2,
        "Index DB",
        if root.available.database_exists {
            style.ok("ok")
        } else {
            style.error("missing")
        },
    );
}

/// Prints all supported source availability rows.
fn print_sources(style: HumanStyle, sources: &[StatusSource]) {
    print_section(style, "Sources");
    for source in sources {
        print_line(2, style.bold(source.kind.title()));
        print_field(style, 4, "State", source_state(style, source));
        if source.configured {
            print_field(style, 4, "Path", source_path_with_state(style, source));
        }
    }
}

/// Prints the active project identity and storage block.
fn print_active_project_identity(style: HumanStyle, project: &StatusProject) {
    print_section(style, "Active Project");
    print_field(style, 2, "Name", &project.name);
    print_field(style, 2, "ID", style.muted(&project.id));
    print_field(
        style,
        2,
        "Root",
        style.path(
            project
                .resolved_project_root
                .as_ref()
                .unwrap_or(&project.local_path)
                .display(),
        ),
    );
    print_field(style, 2, "Archive", archive_status(style, project));
    print_field(
        style,
        2,
        "Archive path",
        style.path(project.sessions_root.display()),
    );
    print_field(
        style,
        2,
        "Known paths",
        style.count(project.known_path_count),
    );
    if let Some(upstream) = &project.git_upstream {
        print_field(style, 2, "Upstream", style.path(upstream));
    }
}

/// Prints one indexed-data status block.
fn print_project_index_status(style: HumanStyle, project: &StatusProject, indent: usize) {
    let heading = if indent == 0 {
        "Indexed Data"
    } else {
        "Indexed"
    };
    if indent == 0 {
        print_section(style, heading);
    } else {
        print_line(indent, style.bold(heading));
    }
    print_field(
        style,
        indent + 2,
        "Sessions",
        style.count(project.session_count),
    );
    print_field(style, indent + 2, "Turns", style.count(project.turn_count));
    print_field(
        style,
        indent + 2,
        "Last activity",
        project
            .last_activity_at
            .as_ref()
            .map(|value| value.to_owned())
            .unwrap_or_else(|| style.muted("none")),
    );
    print_field(
        style,
        indent + 2,
        "Last sync",
        project
            .last_sync_at
            .as_ref()
            .map(|value| value.to_owned())
            .unwrap_or_else(|| style.muted("unknown")),
    );
}

/// Prints the workspace aggregate status block.
fn print_workspace_summary(style: HumanStyle, report: &WorkspaceStatusReport) {
    print_section(style, "Workspace Summary");
    print_field(style, 2, "Projects", style.count(report.projects.len()));
    print_field(
        style,
        2,
        "Indexed sessions",
        style.count(report.total_session_count()),
    );
    print_field(
        style,
        2,
        "Indexed turns",
        style.count(report.total_turn_count()),
    );
    print_field(
        style,
        2,
        "Last activity",
        report
            .latest_activity_at()
            .map(str::to_owned)
            .unwrap_or_else(|| style.muted("none")),
    );
}

/// Prints every workspace project as a readable multi-line block.
fn print_workspace_projects(style: HumanStyle, projects: &[StatusProject]) {
    print_section(style, "Projects");
    if projects.is_empty() {
        print_line(2, style.muted("none"));
        return;
    }

    for (index, project) in projects.iter().enumerate() {
        if index > 0 {
            println!();
        }
        print_workspace_project_status(style, project);
    }
}

/// Prints one compact workspace project row.
fn print_workspace_project_status(style: HumanStyle, project: &StatusProject) {
    print_line(2, style.bold(&project.name));
    print_field(style, 4, "ID", style.muted(&project.id));
    print_field(style, 4, "Path", style.path(project.local_path.display()));
    print_field(style, 4, "Archive", archive_status(style, project));
    print_field(
        style,
        4,
        "Archive path",
        style.path(project.sessions_root.display()),
    );
    print_field(style, 4, "Indexed", indexed_summary(style, project));
    print_field(
        style,
        4,
        "Last activity",
        project
            .last_activity_at
            .as_ref()
            .map(|value| value.to_owned())
            .unwrap_or_else(|| style.muted("none")),
    );
    print_field(
        style,
        4,
        "Last sync",
        project
            .last_sync_at
            .as_ref()
            .map(|value| value.to_owned())
            .unwrap_or_else(|| style.muted("unknown")),
    );
    if project.sync_check.is_some() {
        print_sync_check(style, project.sync_check.as_ref(), "Sync Check", 4);
    }
    if !project.issues.is_empty() {
        print_project_issues(style, project, 4);
    }
}

/// Prints one optional sync dry-run block.
fn print_sync_check(
    style: HumanStyle,
    check: Option<&StatusSyncCheck>,
    label: &str,
    indent: usize,
) {
    let Some(check) = check else {
        return;
    };

    match check {
        StatusSyncCheck::Planned(plan) => print_sync_plan(style, plan, label, indent),
        StatusSyncCheck::Failed(failure) => {
            print_line(
                indent,
                format!("{}: {}", style.bold(label), style.error("failed")),
            );
            print_field(style, indent + 2, "Error", style.error(&failure.message));
        }
    }
}

/// Prints one successful sync dry-run summary.
fn print_sync_plan(style: HumanStyle, plan: &StatusSyncPlan, label: &str, indent: usize) {
    print_line(indent, style.bold(label));
    print_field(
        style,
        indent + 2,
        "Providers",
        format_sources(&plan.sources),
    );
    print_field(
        style,
        indent + 2,
        "Sessions",
        format!(
            "{} pending, {} unchanged",
            style.count(plan.sessions_to_copy),
            style.count(plan.sessions_unchanged)
        ),
    );
    print_field(
        style,
        indent + 2,
        "Auxiliary",
        format!(
            "{} pending, {} unchanged",
            style.count(plan.auxiliary_to_copy),
            style.count(plan.auxiliary_unchanged)
        ),
    );
    print_field(
        style,
        indent + 2,
        "Known paths",
        format!("{} new", style.count(plan.new_known_path_count)),
    );
    print_field(
        style,
        indent + 2,
        "Manifest",
        if plan.manifest_written {
            style.warn("would update")
        } else {
            style.ok("up to date")
        },
    );
    print_field(
        style,
        indent + 2,
        "Config",
        if plan.config_written {
            style.warn("would update")
        } else {
            style.ok("up to date")
        },
    );
    if !plan.warnings.is_empty() {
        print_line(indent + 2, style.warn("Warnings"));
        for warning in &plan.warnings {
            print_line(indent + 4, style.warn(format!("- {warning}")));
        }
    }
}

/// Prints project-local issues when present.
fn print_project_issues(style: HumanStyle, project: &StatusProject, indent: usize) {
    if project.issues.is_empty() {
        return;
    }
    print_line(indent, style.error("Issues"));
    for issue in &project.issues {
        print_line(indent + 2, style.error(format!("- {issue}")));
    }
}

/// Prints the final overall status block.
fn print_overall_status(style: HumanStyle, status: &'static str) {
    print_section(style, "Status");
    let status = if status == "ok" {
        style.ok(status)
    } else {
        style.warn(status)
    };
    print_field(style, 2, "Overall", status);
}

/// Returns the overall human status label for one report.
fn format_overall_status(
    root_issues: &[String],
    sources: &[StatusSource],
    projects: &[StatusProject],
) -> &'static str {
    if root_issues.is_empty()
        && !sources.iter().any(source_needs_attention)
        && !projects.iter().any(project_needs_attention)
    {
        "ok"
    } else {
        "needs attention"
    }
}

/// Returns whether one source row deserves attention.
fn source_needs_attention(source: &StatusSource) -> bool {
    source.configured && source.enabled && !source.path_exists
}

/// Returns whether one project row deserves attention.
fn project_needs_attention(project: &StatusProject) -> bool {
    !project.issues.is_empty() || project.has_failed_check()
}
