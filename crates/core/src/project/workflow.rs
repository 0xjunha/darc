use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use super::{
    link::prepare_link,
    registry::{registered_projects, write_shared_config},
    remove::{remove_project_by_id, remove_project_named},
    types::{
        LinkReport, RefreshAllBestEffortReport, RefreshAllReport, RefreshOptions,
        RefreshProjectAttempt, RefreshProjectFailure, RefreshReport, RemoveReport, RenameReport,
    },
};
use crate::{
    default_root_path,
    index::{index_project_sessions_from, selected_index_providers},
    sync::{SyncOptions, execute_sync, prepare_sync_from},
};

/// Links one named project's historical paths into the active project.
pub fn link_project(root: Option<PathBuf>, source_name: &str) -> Result<LinkReport> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    link_project_from(
        &current_dir,
        root.unwrap_or_else(default_root_path),
        source_name,
    )
}

/// Removes one named project from config, archive storage, and SQLite.
pub fn remove_project(root: Option<PathBuf>, project_name: &str) -> Result<RemoveReport> {
    remove_project_named(&root.unwrap_or_else(default_root_path), project_name)
}

/// Renames one historical project into the active project by rebuilding under the active id.
pub fn rename_project(root: Option<PathBuf>, source_name: &str) -> Result<RenameReport> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    rename_project_from(
        &current_dir,
        root.unwrap_or_else(default_root_path),
        source_name,
    )
}

/// Refreshes one active project by running sync and then index.
pub fn refresh_project(root: Option<PathBuf>, options: RefreshOptions) -> Result<RefreshReport> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    refresh_project_from(
        &current_dir,
        root.unwrap_or_else(default_root_path),
        &options,
    )
}

/// Refreshes every registered project by running sync and then index for each one.
pub fn refresh_all_projects(
    root: Option<PathBuf>,
    options: RefreshOptions,
) -> Result<RefreshAllReport> {
    let root = root.unwrap_or_else(default_root_path);
    let projects = registered_projects(&root)?;
    if projects.is_empty() {
        bail!("no configured darc projects found under {}", root.display());
    }

    let mut reports = Vec::with_capacity(projects.len());
    for project in projects {
        reports.push(
            refresh_project_from(&project.local_path, root.clone(), &options)
                .with_context(|| format!("failed to refresh project `{}`", project.name))?,
        );
    }

    Ok(RefreshAllReport { projects: reports })
}

/// Refreshes every registered project and records structured per-project failures.
pub fn refresh_all_projects_best_effort(
    root: Option<PathBuf>,
    options: RefreshOptions,
) -> Result<RefreshAllBestEffortReport> {
    let root = root.unwrap_or_else(default_root_path);
    let projects = registered_projects(&root)?;
    if projects.is_empty() {
        bail!("no configured darc projects found under {}", root.display());
    }

    let mut reports = Vec::with_capacity(projects.len());
    for project in projects {
        let attempt = match refresh_project_from(&project.local_path, root.clone(), &options) {
            Ok(report) => RefreshProjectAttempt::Refreshed(Box::new(report)),
            Err(error) => RefreshProjectAttempt::Failed(RefreshProjectFailure {
                project_name: project.name.clone(),
                project_root: project.local_path.clone(),
                error: error.context(format!("failed to refresh project `{}`", project.name)),
            }),
        };
        reports.push(attempt);
    }

    Ok(RefreshAllBestEffortReport { projects: reports })
}

/// Links one named project's historical paths into one explicit active project.
pub(crate) fn link_project_from(
    current_dir: &Path,
    root: PathBuf,
    source_name: &str,
) -> Result<LinkReport> {
    let prepared = prepare_link(current_dir, &root, source_name)?;
    if prepared.config_written {
        write_shared_config(&prepared.config_path, &prepared.config)?;
    }

    Ok(LinkReport {
        target_project_name: prepared.target_project.name,
        target_project_id: prepared.target_project.id,
        target_project_root: prepared.current_root,
        source_project_name: prepared.source_project.name,
        source_project_id: prepared.source_project.id,
        total_known_paths: prepared.total_known_paths,
        new_known_paths: prepared.new_known_paths,
        config_written: prepared.config_written,
    })
}

/// Runs the full rename workflow from one explicit active project directory.
pub(crate) fn rename_project_from(
    current_dir: &Path,
    root: PathBuf,
    source_name: &str,
) -> Result<RenameReport> {
    let prepared = prepare_link(current_dir, &root, source_name)?;
    if prepared.config_written {
        write_shared_config(&prepared.config_path, &prepared.config)?;
    }

    let link = LinkReport {
        target_project_name: prepared.target_project.name.clone(),
        target_project_id: prepared.target_project.id.clone(),
        target_project_root: prepared.current_root.clone(),
        source_project_name: prepared.source_project.name.clone(),
        source_project_id: prepared.source_project.id.clone(),
        total_known_paths: prepared.total_known_paths,
        new_known_paths: prepared.new_known_paths.clone(),
        config_written: prepared.config_written,
    };
    let refresh = refresh_project_from(current_dir, root.clone(), &RefreshOptions::default())?;
    let remove = remove_project_by_id(&root, &link.source_project_id)?;

    Ok(RenameReport {
        link,
        sync: refresh.sync,
        index: refresh.index,
        remove,
    })
}

/// Runs sync and index for one explicit active project directory.
pub(crate) fn refresh_project_from(
    current_dir: &Path,
    root: PathBuf,
    options: &RefreshOptions,
) -> Result<RefreshReport> {
    let sync = execute_sync(prepare_sync_from(
        current_dir,
        root.clone(),
        SyncOptions {
            provider_filter: options.provider_filter.clone(),
        },
    )?)?;
    let index = index_project_sessions_from(
        current_dir,
        root,
        &selected_index_providers(&options.provider_filter),
    )?;

    Ok(RefreshReport { sync, index })
}
