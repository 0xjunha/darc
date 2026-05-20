use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use super::{
    link::prepare_link,
    registry::{registered_projects, write_shared_config},
    remove::{
        preview_remove_project_by_id, preview_remove_project_named, remove_project_by_id,
        remove_project_named,
    },
    types::{
        LinkReport, RefreshAllBestEffortReport, RefreshAllReport, RefreshOptions, RefreshProgress,
        RefreshProjectAttempt, RefreshProjectFailure, RefreshReport, RemovePreviewReport,
        RemoveReport, RenamePreviewReport, RenameReport,
    },
};
use crate::{
    active_project::{ActiveProject, load_active_project},
    config::ProjectConfig,
    default_root_path,
    index::{
        IndexProgress, index_project_sessions_for_active_project_with_progress,
        selected_index_providers,
    },
    sync::{
        SyncOptions, SyncProgress, execute_sync_with_progress, prepare_sync_for_active_project,
    },
};

/// Stores display identity for one project refresh progress entry.
#[derive(Debug, Clone)]
struct RefreshProjectProgress {
    project_name: String,
    project_root: PathBuf,
    project_index: usize,
    total_projects: usize,
}

impl RefreshProjectProgress {
    /// Builds one progress identity from a configured project row.
    fn from_project(project: &ProjectConfig, project_index: usize, total_projects: usize) -> Self {
        Self {
            project_name: project.name.clone(),
            project_root: project.local_path.clone(),
            project_index,
            total_projects,
        }
    }

    /// Builds one progress identity from a resolved active project.
    fn from_active_project(
        active_project: &ActiveProject,
        project_index: usize,
        total_projects: usize,
    ) -> Self {
        Self {
            project_name: active_project.project.name.clone(),
            project_root: active_project.current_root.clone(),
            project_index,
            total_projects,
        }
    }
}

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

/// Previews one project link without writing the shared config.
pub fn preview_link_project(root: Option<PathBuf>, source_name: &str) -> Result<LinkReport> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    preview_link_project_from(
        &current_dir,
        root.unwrap_or_else(default_root_path),
        source_name,
    )
}

/// Removes one named project from config, archive storage, and SQLite.
pub fn remove_project(root: Option<PathBuf>, project_name: &str) -> Result<RemoveReport> {
    remove_project_named(&root.unwrap_or_else(default_root_path), project_name)
}

/// Previews one named project removal without changing config, archive storage, or SQLite.
pub fn preview_remove_project(
    root: Option<PathBuf>,
    project_name: &str,
) -> Result<RemovePreviewReport> {
    preview_remove_project_named(&root.unwrap_or_else(default_root_path), project_name)
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

/// Previews one project rename workflow without writing config, archive files, or SQLite.
pub fn preview_rename_project(
    root: Option<PathBuf>,
    source_name: &str,
) -> Result<RenamePreviewReport> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    preview_rename_project_from(
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

/// Refreshes one active project while reporting neutral workflow progress events.
pub fn refresh_project_with_progress(
    root: Option<PathBuf>,
    options: RefreshOptions,
    mut progress: impl FnMut(RefreshProgress),
) -> Result<RefreshReport> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    refresh_project_from_with_progress(
        &current_dir,
        root.unwrap_or_else(default_root_path),
        &options,
        None,
        &mut progress,
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
    let total_projects = projects.len();
    let mut progress = |_| {};
    for (index, project) in projects.into_iter().enumerate() {
        reports.push(
            refresh_project_from_with_progress(
                &project.local_path,
                root.clone(),
                &options,
                Some(RefreshProjectProgress::from_project(
                    &project,
                    index + 1,
                    total_projects,
                )),
                &mut progress,
            )
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
    let mut progress = |_| {};
    refresh_all_projects_best_effort_with_progress(root, options, &mut progress)
}

/// Refreshes every registered project while reporting neutral workflow progress events.
pub fn refresh_all_projects_best_effort_with_progress(
    root: Option<PathBuf>,
    options: RefreshOptions,
    mut progress: impl FnMut(RefreshProgress),
) -> Result<RefreshAllBestEffortReport> {
    let root = root.unwrap_or_else(default_root_path);
    let projects = registered_projects(&root)?;
    if projects.is_empty() {
        bail!("no configured darc projects found under {}", root.display());
    }

    progress(RefreshProgress::WorkspaceStarted {
        total_projects: projects.len(),
    });

    let mut reports = Vec::with_capacity(projects.len());
    let total_projects = projects.len();
    for (index, project) in projects.into_iter().enumerate() {
        let attempt = match refresh_project_from_with_progress(
            &project.local_path,
            root.clone(),
            &options,
            Some(RefreshProjectProgress::from_project(
                &project,
                index + 1,
                total_projects,
            )),
            &mut progress,
        ) {
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

/// Previews one named project link from one explicit active project directory.
pub(crate) fn preview_link_project_from(
    current_dir: &Path,
    root: PathBuf,
    source_name: &str,
) -> Result<LinkReport> {
    let prepared = prepare_link(current_dir, &root, source_name)?;
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

/// Previews the full rename workflow from one explicit active project directory.
pub(crate) fn preview_rename_project_from(
    current_dir: &Path,
    root: PathBuf,
    source_name: &str,
) -> Result<RenamePreviewReport> {
    let prepared = prepare_link(current_dir, &root, source_name)?;
    let remove_preview = preview_remove_project_by_id(&root, &prepared.source_project.id)?;

    Ok(RenamePreviewReport {
        target_project_name: prepared.target_project.name,
        target_project_id: prepared.target_project.id,
        target_project_root: prepared.current_root,
        source_project_name: prepared.source_project.name,
        source_project_id: prepared.source_project.id,
        total_known_paths: prepared.total_known_paths,
        new_known_paths: prepared.new_known_paths,
        config_would_change: prepared.config_written || remove_preview.config_would_change,
        source_sessions_root: remove_preview.sessions_root,
        source_archive_would_delete: remove_preview.archive_would_delete,
        indexed_sessions_would_remove: remove_preview.indexed_sessions_would_remove,
        indexed_turns_would_remove: remove_preview.indexed_turns_would_remove,
    })
}

/// Runs sync and index for one explicit active project directory.
pub(crate) fn refresh_project_from(
    current_dir: &Path,
    root: PathBuf,
    options: &RefreshOptions,
) -> Result<RefreshReport> {
    let mut progress = |_| {};
    refresh_project_from_with_progress(current_dir, root, options, None, &mut progress)
}

/// Runs sync and index while reporting neutral workflow progress events.
fn refresh_project_from_with_progress(
    current_dir: &Path,
    root: PathBuf,
    options: &RefreshOptions,
    progress_project: Option<RefreshProjectProgress>,
    progress: &mut impl FnMut(RefreshProgress),
) -> Result<RefreshReport> {
    let mut active_project = None;
    let project = match progress_project {
        Some(project) => project,
        None => {
            let active = load_active_project(current_dir, &root)?;
            let project = RefreshProjectProgress::from_active_project(&active, 1, 1);
            active_project = Some(active);
            project
        }
    };

    progress(RefreshProgress::ProjectStarted {
        project_name: project.project_name.clone(),
        project_root: project.project_root.clone(),
        project_index: project.project_index,
        total_projects: project.total_projects,
    });

    let result = refresh_loaded_project_from(
        current_dir,
        root,
        options,
        active_project,
        &project.project_name,
        progress,
    );
    match &result {
        Ok(_) => progress(RefreshProgress::ProjectFinished {
            project_name: project.project_name.clone(),
        }),
        Err(_) => progress(RefreshProgress::ProjectFailed {
            project_name: project.project_name.clone(),
        }),
    }
    result
}

/// Runs sync and index for one project, reusing a resolved active project when available.
fn refresh_loaded_project_from(
    current_dir: &Path,
    root: PathBuf,
    options: &RefreshOptions,
    active_project: Option<ActiveProject>,
    project_name: &str,
    progress: &mut impl FnMut(RefreshProgress),
) -> Result<RefreshReport> {
    let active_project = match active_project {
        Some(active_project) => active_project,
        None => load_active_project(current_dir, &root)?,
    };
    let index_project = active_project.clone();

    progress(RefreshProgress::SyncStarted {
        project_name: project_name.to_owned(),
    });
    let sync_plan = prepare_sync_for_active_project(
        active_project,
        SyncOptions {
            provider_filter: options.provider_filter.clone(),
        },
    )?;
    let sync_unchanged_sessions = sync_plan.sessions_unchanged;
    let sync_total_sessions = sync_unchanged_sessions + sync_plan.sessions_to_copy();
    progress(RefreshProgress::SyncingSessions {
        project_name: project_name.to_owned(),
        synced_sessions: sync_unchanged_sessions,
        total_sessions: sync_total_sessions,
    });
    let sync = execute_sync_with_progress(sync_plan, |event| {
        let SyncProgress::CopyingSessions {
            copied_sessions, ..
        } = event;
        progress(RefreshProgress::SyncingSessions {
            project_name: project_name.to_owned(),
            synced_sessions: sync_unchanged_sessions + copied_sessions,
            total_sessions: sync_total_sessions,
        });
    })?;
    progress(RefreshProgress::SyncFinished {
        project_name: project_name.to_owned(),
    });

    progress(RefreshProgress::IndexStarted {
        project_name: project_name.to_owned(),
    });
    let index = index_project_sessions_for_active_project_with_progress(
        index_project,
        root,
        &selected_index_providers(&options.provider_filter),
        |event| {
            let IndexProgress::IndexingSessions {
                indexed_sessions,
                total_sessions,
            } = event;
            progress(RefreshProgress::IndexingSessions {
                project_name: project_name.to_owned(),
                indexed_sessions,
                total_sessions,
            });
        },
    )?;
    progress(RefreshProgress::IndexFinished {
        project_name: project_name.to_owned(),
    });

    Ok(RefreshReport { sync, index })
}
