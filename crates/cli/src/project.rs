use anyhow::Result;
use darc_core::{
    InitDraft, LinkReport, link_project, prepare_init, preview_link_project,
    preview_remove_project, preview_rename_project, remove_project, rename_project, write_init,
};

use crate::args::{InitArgs, LinkArgs, ProjectArgs, ProjectCommands, RemoveArgs, RenameArgs};
use crate::output::{HumanStyle, print_field, print_line, print_section};
use crate::sync_index::print_index_summary;

/// Dispatches the supported project-management commands.
pub(crate) fn run_project(args: ProjectArgs) -> Result<()> {
    match args.command {
        ProjectCommands::Link(args) => run_link(args),
        ProjectCommands::Remove(args) => run_remove(args),
        ProjectCommands::RenameFrom(args) => run_rename_from(args),
    }
}

/// Prepares and optionally writes the shared init draft.
pub(crate) fn run_init(args: InitArgs) -> Result<()> {
    let draft = prepare_init(Some(args.root))?;

    if !args.dry_run {
        write_init(&draft)?;
    }

    let style = HumanStyle::stdout();
    print_init_draft(style, &draft);
    if args.dry_run {
        println!();
        print_init_status(style, &draft, true);
        println!();
        print_section(style, "Config Preview");
        println!("{}", draft.config_toml()?);
    } else {
        println!();
        print_init_status(style, &draft, false);
    }

    Ok(())
}

/// Prints the prepared init summary.
fn print_init_draft(style: HumanStyle, draft: &InitDraft) {
    print_section(style, "Darc");
    print_field(
        style,
        2,
        "Config",
        if draft.global_config_exists {
            style.ok("existing")
        } else {
            style.warn("not found")
        },
    );
    print_field(style, 2, "Root", style.path(draft.root().display()));
    print_field(
        style,
        2,
        "Config path",
        style.path(draft.root().join("config.toml").display()),
    );
    print_field(
        style,
        2,
        "Index DB path",
        style.path(draft.root().join("index.sqlite").display()),
    );

    if !draft.global_config_exists {
        println!();
        print_section(style, "Detected Sources");
        if draft.sources.is_empty() {
            print_line(2, style.muted("none"));
        }
        for source in &draft.sources {
            print_line(2, style.bold(source.kind.title()));
            print_field(style, 4, "Path", style.path(source.root.display()));
            print_field(style, 4, "Rollouts", style.count(source.rollout_files));
            if source.subagent_rollout_files > 0 {
                print_field(
                    style,
                    4,
                    "Subagents",
                    style.count(source.subagent_rollout_files),
                );
            }
        }
    }

    println!();
    print_section(style, "Project");
    print_field(style, 2, "Name", &draft.project.name);
    print_field(
        style,
        2,
        "Root",
        style.path(draft.project.local_path.display()),
    );
    print_field(
        style,
        2,
        "State",
        if draft.project_exists {
            style.ok("already configured")
        } else {
            style.warn("new")
        },
    );
    if let Some(upstream) = &draft.project.git_upstream {
        print_field(style, 2, "Upstream", style.path(upstream));
    }
}

/// Prints the final init status block.
fn print_init_status(style: HumanStyle, draft: &InitDraft, dry_run: bool) {
    print_section(style, "Status");
    for line in format_init_status(draft, dry_run).lines() {
        let line = if dry_run {
            style.warn(line)
        } else {
            style.ok(line)
        };
        print_line(2, line);
    }
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
pub(crate) fn run_link(args: LinkArgs) -> Result<()> {
    let style = HumanStyle::stdout();
    if args.dry_run {
        let report = preview_link_project(Some(args.root), &args.project)?;
        print_section(style, "Link Preview");
        print_link_report(style, &report);
        println!();
        print_section(style, "Would Update");
        if report.config_written {
            print_field(style, 2, "Config", style.warn("yes"));
        } else {
            print_field(style, 2, "Config", style.muted("unchanged"));
        }
        println!();
        print_section(style, "Status");
        print_field(style, 2, "Overall", style.ok("dry run only"));
        return Ok(());
    }

    let report = link_project(Some(args.root), &args.project)?;
    print_section(style, "Link");
    print_link_report(style, &report);
    println!();
    print_section(style, "Status");
    if report.config_written {
        print_field(style, 2, "Config", style.ok("updated"));
    } else {
        print_field(style, 2, "Config", style.ok("already covered linked paths"));
    }

    Ok(())
}

/// Prints the shared project-link identity and known-path summary.
fn print_link_report(style: HumanStyle, report: &LinkReport) {
    print_field(style, 2, "Target project", &report.target_project_name);
    print_field(
        style,
        2,
        "Target ID",
        style.muted(&report.target_project_id),
    );
    print_field(style, 2, "Linked from", &report.source_project_name);
    print_field(
        style,
        2,
        "Source ID",
        style.muted(&report.source_project_id),
    );
    print_field(
        style,
        2,
        "Project root",
        style.path(report.target_project_root.display()),
    );
    print_field(
        style,
        2,
        "Known paths",
        format!(
            "{} total, {} added",
            style.count(report.total_known_paths),
            style.count(report.new_known_paths.len())
        ),
    );
}

/// Removes one configured project and its archived/indexed data.
pub(crate) fn run_remove(args: RemoveArgs) -> Result<()> {
    let style = HumanStyle::stdout();
    if args.dry_run {
        let report = preview_remove_project(Some(args.root), &args.project)?;
        print_section(style, "Remove Preview");
        print_field(style, 2, "Project", &report.project_name);
        print_field(style, 2, "Project ID", style.muted(&report.project_id));
        print_field(
            style,
            2,
            "Archive",
            style.path(report.sessions_root.display()),
        );
        println!();
        print_section(style, "Would Delete");
        if report.archive_would_delete {
            print_field(style, 2, "Archive", style.warn("yes"));
        } else {
            print_field(style, 2, "Archive", style.muted("not present"));
        }
        print_field(
            style,
            2,
            "Indexed sessions",
            style.count(report.indexed_sessions_would_remove),
        );
        print_field(
            style,
            2,
            "Indexed turns",
            style.count(report.indexed_turns_would_remove),
        );
        print_field(
            style,
            2,
            "Config",
            if report.config_would_change {
                style.warn("would update")
            } else {
                style.muted("unchanged")
            },
        );
        println!();
        print_section(style, "Status");
        print_field(style, 2, "Overall", style.ok("dry run only"));
        return Ok(());
    }

    let report = remove_project(Some(args.root), &args.project)?;
    print_section(style, "Remove");
    print_field(style, 2, "Project", &report.project_name);
    print_field(style, 2, "Project ID", style.muted(&report.project_id));
    print_field(
        style,
        2,
        "Archive",
        style.path(report.sessions_root.display()),
    );
    println!();
    print_section(style, "Deleted Data");
    if report.archive_deleted {
        print_field(style, 2, "Archive", style.warn("deleted"));
    } else {
        print_field(style, 2, "Archive", style.muted("did not exist"));
    }
    print_field(
        style,
        2,
        "Indexed sessions",
        style.count(report.indexed_sessions_removed),
    );
    print_field(
        style,
        2,
        "Indexed turns",
        style.count(report.indexed_turns_removed),
    );
    println!();
    print_section(style, "Status");
    if report.config_written {
        print_field(style, 2, "Config", style.ok("updated"));
    }

    Ok(())
}

/// Rebuilds one configured project's history under the active project's id.
pub(crate) fn run_rename_from(args: RenameArgs) -> Result<()> {
    let style = HumanStyle::stdout();
    if args.dry_run {
        let report = preview_rename_project(Some(args.root), &args.project)?;
        print_section(style, "Rename Preview");
        print_field(style, 2, "Project", &report.target_project_name);
        print_field(
            style,
            2,
            "Project ID",
            style.muted(&report.target_project_id),
        );
        print_field(style, 2, "Renamed from", &report.source_project_name);
        print_field(
            style,
            2,
            "Source ID",
            style.muted(&report.source_project_id),
        );
        print_field(
            style,
            2,
            "Project root",
            style.path(report.target_project_root.display()),
        );
        print_field(
            style,
            2,
            "Known paths",
            format!(
                "{} total, {} would add",
                style.count(report.total_known_paths),
                style.count(report.new_known_paths.len())
            ),
        );
        println!();
        print_section(style, "Would Run");
        print_field(style, 2, "Refresh", "sync and index target project");
        print_field(
            style,
            2,
            "Source archive",
            style.path(report.source_sessions_root.display()),
        );
        if report.source_archive_would_delete {
            print_field(
                style,
                2,
                "Source archive cleanup",
                style.warn("would delete"),
            );
        } else {
            print_field(
                style,
                2,
                "Source archive cleanup",
                style.muted("not present"),
            );
        }
        print_field(
            style,
            2,
            "Indexed sessions cleanup",
            style.count(report.indexed_sessions_would_remove),
        );
        print_field(
            style,
            2,
            "Indexed turns cleanup",
            style.count(report.indexed_turns_would_remove),
        );
        print_field(
            style,
            2,
            "Config",
            if report.config_would_change {
                style.warn("would update")
            } else {
                style.muted("unchanged")
            },
        );
        println!();
        print_section(style, "Status");
        print_field(style, 2, "Overall", style.ok("dry run only"));
        return Ok(());
    }

    let report = rename_project(Some(args.root), &args.project)?;
    print_section(style, "Rename");
    print_field(style, 2, "Project", &report.link.target_project_name);
    print_field(style, 2, "Renamed from", &report.link.source_project_name);
    print_field(
        style,
        2,
        "Known paths",
        format!(
            "{} total, {} added",
            style.count(report.link.total_known_paths),
            style.count(report.link.new_known_paths.len())
        ),
    );
    println!();
    print_section(style, "Sync");
    print_field(
        style,
        2,
        "Sessions",
        format!(
            "{} copied, {} unchanged",
            style.count(report.sync.sessions_copied),
            style.count(report.sync.sessions_unchanged)
        ),
    );
    print_field(
        style,
        2,
        "Auxiliary",
        format!(
            "{} copied, {} unchanged",
            style.count(report.sync.auxiliary_copied),
            style.count(report.sync.auxiliary_unchanged)
        ),
    );
    println!();
    print_index_summary(style, &report.index);
    println!();
    print_section(style, "Cleanup");
    print_field(
        style,
        2,
        "Old archive",
        if report.remove.archive_deleted {
            style.warn("deleted")
        } else {
            style.muted("did not exist")
        },
    );
    print_field(
        style,
        2,
        "Indexed sessions",
        style.count(report.remove.indexed_sessions_removed),
    );
    println!();
    print_section(style, "Status");
    print_field(style, 2, "Overall", style.ok("renamed"));

    Ok(())
}
