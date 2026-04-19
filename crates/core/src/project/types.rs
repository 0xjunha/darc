use std::path::PathBuf;

use anyhow::Error;

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

/// Stores one structured project refresh failure for best-effort workspace refreshes.
#[derive(Debug)]
pub struct RefreshProjectFailure {
    pub project_name: String,
    pub project_root: PathBuf,
    pub error: Error,
}

/// Stores one attempted project refresh inside a best-effort workspace refresh.
#[derive(Debug)]
pub enum RefreshProjectAttempt {
    Refreshed(Box<RefreshReport>),
    Failed(RefreshProjectFailure),
}

impl RefreshProjectAttempt {
    /// Returns the completed refresh report when this project refreshed successfully.
    pub fn refreshed_report(&self) -> Option<&RefreshReport> {
        match self {
            Self::Refreshed(report) => Some(report.as_ref()),
            Self::Failed(_) => None,
        }
    }

    /// Returns the structured failure when this project refresh failed.
    pub fn failure(&self) -> Option<&RefreshProjectFailure> {
        match self {
            Self::Refreshed(_) => None,
            Self::Failed(failure) => Some(failure),
        }
    }
}

/// Reports one completed best-effort multi-project refresh workflow.
#[derive(Debug)]
pub struct RefreshAllBestEffortReport {
    pub projects: Vec<RefreshProjectAttempt>,
}

impl RefreshAllBestEffortReport {
    /// Returns how many projects refreshed successfully.
    pub fn refreshed_count(&self) -> usize {
        self.projects
            .iter()
            .filter(|project| project.refreshed_report().is_some())
            .count()
    }

    /// Returns how many projects failed during refresh.
    pub fn failed_count(&self) -> usize {
        self.projects
            .iter()
            .filter(|project| project.failure().is_some())
            .count()
    }

    /// Returns whether any project failed during refresh.
    pub fn has_failures(&self) -> bool {
        self.failed_count() > 0
    }
}

/// Reports one completed rename workflow across config, archive sync, indexing, and cleanup.
#[derive(Debug, Clone)]
pub struct RenameReport {
    pub link: LinkReport,
    pub sync: SyncReport,
    pub index: IndexReport,
    pub remove: RemoveReport,
}
