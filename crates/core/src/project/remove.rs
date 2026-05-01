use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use darc_index::{INDEX_DB_FILE_NAME, count_project_index_rows_read_only, open_index_database};
use rusqlite::params;

use super::{
    registry::{
        find_project_index_by_id, find_unique_project_index_by_name, load_normalized_shared_config,
        write_shared_config,
    },
    types::{RemovePreviewReport, RemoveReport},
};
use crate::constants::CONFIG_FILE_NAME;

/// Stores the project session-count query used during SQLite cleanup.
const COUNT_PROJECT_SESSIONS_SQL: &str = "SELECT COUNT(*) FROM sessions WHERE project_id = ?1";

/// Stores the project turn-count query used during SQLite cleanup.
const COUNT_PROJECT_TURNS_SQL: &str = "SELECT COUNT(*) FROM turns WHERE project_id = ?1";

/// Removes one named project from config, archive storage, and SQLite.
pub(super) fn remove_project_named(root: &Path, project_name: &str) -> Result<RemoveReport> {
    let config_path = root.join(CONFIG_FILE_NAME);
    let config = load_config_for_removal(root, &config_path)?;
    let project_index = find_unique_project_index_by_name(&config.projects, project_name)?;
    let project_id = config.projects[project_index].id.clone();
    remove_project_by_id(root, &project_id)
}

/// Previews one named project removal without changing config, archive storage, or SQLite.
pub(super) fn preview_remove_project_named(
    root: &Path,
    project_name: &str,
) -> Result<RemovePreviewReport> {
    let config_path = root.join(CONFIG_FILE_NAME);
    let config = load_config_for_removal(root, &config_path)?;
    let project_index = find_unique_project_index_by_name(&config.projects, project_name)?;
    build_remove_preview(root, &config.projects[project_index])
}

/// Previews one project removal by stable id without changing storage.
pub(super) fn preview_remove_project_by_id(
    root: &Path,
    project_id: &str,
) -> Result<RemovePreviewReport> {
    let config_path = root.join(CONFIG_FILE_NAME);
    let config = load_config_for_removal(root, &config_path)?;
    let project_index = find_project_index_by_id(&config.projects, project_id)?;
    build_remove_preview(root, &config.projects[project_index])
}

/// Removes one project by stable id from config, archive storage, and SQLite.
pub(super) fn remove_project_by_id(root: &Path, project_id: &str) -> Result<RemoveReport> {
    let config_path = root.join(CONFIG_FILE_NAME);
    let mut config = load_config_for_removal(root, &config_path)?;
    let project_index = find_project_index_by_id(&config.projects, project_id)?;
    let project = config.projects.remove(project_index);
    let (indexed_sessions_removed, indexed_turns_removed) =
        delete_project_index_rows(&root.join(INDEX_DB_FILE_NAME), &project.id)?;
    let archive_deleted = delete_project_archive(&project.sessions_root)?;
    write_shared_config(&config_path, &config)?;

    Ok(RemoveReport {
        project_name: project.name,
        project_id: project.id,
        sessions_root: project.sessions_root,
        archive_deleted,
        indexed_sessions_removed,
        indexed_turns_removed,
        config_written: true,
    })
}

/// Loads the shared config for remove or preview operations.
fn load_config_for_removal(root: &Path, config_path: &Path) -> Result<crate::config::SharedConfig> {
    if !config_path.exists() {
        bail!(
            "shared config not found at {}\nrun `darc init --root {}` from your project root first",
            config_path.display(),
            root.display()
        );
    }

    load_normalized_shared_config(config_path)
}

/// Builds one non-mutating removal preview for a resolved project.
fn build_remove_preview(
    root: &Path,
    project: &crate::config::ProjectConfig,
) -> Result<RemovePreviewReport> {
    let (indexed_sessions_removed, indexed_turns_removed) =
        count_project_index_rows_read_only(&root.join(INDEX_DB_FILE_NAME), &project.id)?;

    Ok(RemovePreviewReport {
        project_name: project.name.clone(),
        project_id: project.id.clone(),
        sessions_root: project.sessions_root.clone(),
        archive_would_delete: project.sessions_root.exists(),
        indexed_sessions_would_remove: indexed_sessions_removed,
        indexed_turns_would_remove: indexed_turns_removed,
        config_would_change: true,
    })
}

/// Deletes one archived project sessions directory when it exists.
fn delete_project_archive(sessions_root: &Path) -> Result<bool> {
    if !sessions_root.exists() {
        return Ok(false);
    }

    fs::remove_dir_all(sessions_root)
        .with_context(|| format!("failed to remove {}", sessions_root.display()))?;
    Ok(true)
}

/// Deletes one project's indexed SQLite rows and returns the removed row counts.
fn delete_project_index_rows(index_db_path: &Path, project_id: &str) -> Result<(usize, usize)> {
    if !index_db_path.exists() {
        return Ok((0, 0));
    }

    let mut connection = open_index_database(index_db_path)?;
    let transaction = connection
        .transaction()
        .context("failed to begin SQLite removal transaction")?;
    let indexed_sessions_removed = count_project_rows(
        &transaction,
        COUNT_PROJECT_SESSIONS_SQL,
        "sessions",
        project_id,
    )?;
    let indexed_turns_removed =
        count_project_rows(&transaction, COUNT_PROJECT_TURNS_SQL, "turns", project_id)?;
    transaction
        .execute(
            "DELETE FROM sessions WHERE project_id = ?1",
            params![project_id],
        )
        .with_context(|| format!("failed to delete indexed sessions for project `{project_id}`"))?;
    transaction
        .commit()
        .context("failed to commit SQLite project removal transaction")?;

    Ok((indexed_sessions_removed, indexed_turns_removed))
}

/// Counts one project's rows in a selected normalized SQLite table.
fn count_project_rows(
    connection: &rusqlite::Connection,
    sql: &str,
    label: &str,
    project_id: &str,
) -> Result<usize> {
    let count: i64 = connection
        .query_row(sql, params![project_id], |row| row.get(0))
        .with_context(|| format!("failed to count indexed {label} for project `{project_id}`"))?;
    usize::try_from(count).with_context(|| format!("indexed {label} count exceeds usize range"))
}
