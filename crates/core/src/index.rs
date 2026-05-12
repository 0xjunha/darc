use std::{
    collections::BTreeSet,
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
pub use darc_index::{IndexReport, SkippedCodexRollout, SkippedRollout};
use darc_index::{ProjectIndexRequest, index_project_archived_sessions};
use darc_paths::SourceKind;
use darc_store::INDEX_DB_FILE_NAME;

use crate::{
    active_project::{ActiveProject, load_active_project},
    default_root_path,
};

/// Collects optional provider filters for the indexing workflow.
#[derive(Debug, Clone, Default)]
pub struct IndexOptions {
    pub provider_filter: Vec<SourceKind>,
}

/// Indexes archived sessions for the active project into SQLite.
pub fn index_project_sessions(root: Option<PathBuf>, options: IndexOptions) -> Result<IndexReport> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    index_project_sessions_from(
        &current_dir,
        root.unwrap_or_else(default_root_path),
        &selected_index_providers(&options.provider_filter),
    )
}

/// Indexes archived Codex rollouts for the active project into SQLite.
pub fn index_project_codex_turns(root: Option<PathBuf>) -> Result<IndexReport> {
    index_project_sessions(
        root,
        IndexOptions {
            provider_filter: vec![SourceKind::Codex],
        },
    )
}

/// Indexes archived provider rollouts for one explicit current directory and darc root.
pub(crate) fn index_project_sessions_from(
    current_dir: &Path,
    root: PathBuf,
    providers: &[SourceKind],
) -> Result<IndexReport> {
    let active_project = load_active_project(current_dir, &root)?;
    index_project_sessions_for_active_project(active_project, root, providers)
}

/// Indexes archived provider rollouts for one already-resolved active project.
pub(crate) fn index_project_sessions_for_active_project(
    active_project: ActiveProject,
    root: PathBuf,
    providers: &[SourceKind],
) -> Result<IndexReport> {
    let request = ProjectIndexRequest {
        project_id: active_project.project.id,
        project_name: active_project.project.name,
        project_root: active_project.current_root,
        sessions_root: active_project.project.sessions_root,
        index_db_path: root.join(INDEX_DB_FILE_NAME),
    };
    index_project_archived_sessions(&request, providers)
}

/// Resolves the selected provider list for one indexing run.
pub(crate) fn selected_index_providers(filter: &[SourceKind]) -> Vec<SourceKind> {
    if filter.is_empty() {
        return vec![SourceKind::Claude, SourceKind::Codex];
    }

    filter
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
