use std::path::PathBuf;

use crate::{SourceKind, index::IndexReport, sync::SyncReport};

/// Reports one completed config-only project link operation.
#[derive(Debug, Clone)]
pub struct LinkReport {
    pub target_project_name: String,
    pub target_project_id: String,
    pub target_project_root: PathBuf,
    pub source_project_name: String,
    pub source_project_id: String,
    pub new_known_paths: Vec<PathBuf>,
    pub total_known_paths: usize,
    pub config_written: bool,
}

/// Reports one completed destructive project removal.
#[derive(Debug, Clone)]
pub struct RemoveReport {
    pub project_name: String,
    pub project_id: String,
    pub sessions_root: PathBuf,
    pub archive_deleted: bool,
    pub indexed_sessions_removed: usize,
    pub indexed_turns_removed: usize,
    pub config_written: bool,
}

/// Collects optional provider filters for the refresh workflow.
#[derive(Debug, Clone, Default)]
pub struct RefreshOptions {
    pub provider_filter: Vec<SourceKind>,
}

/// Reports one completed refresh workflow across sync and index.
#[derive(Debug, Clone)]
pub struct RefreshReport {
    pub sync: SyncReport,
    pub index: IndexReport,
}

/// Reports one completed multi-project refresh workflow.
#[derive(Debug, Clone)]
pub struct RefreshAllReport {
    pub projects: Vec<RefreshReport>,
}

/// Reports one completed rename workflow across config, archive sync, indexing, and cleanup.
#[derive(Debug, Clone)]
pub struct RenameReport {
    pub link: LinkReport,
    pub sync: SyncReport,
    pub index: IndexReport,
    pub remove: RemoveReport,
}
