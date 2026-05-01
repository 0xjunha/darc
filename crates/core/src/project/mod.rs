mod link;
mod registry;
mod remove;
#[cfg(test)]
mod tests;
mod types;
mod workflow;

pub(crate) use registry::write_shared_config;
pub use types::{
    LinkReport, RefreshAllBestEffortReport, RefreshAllReport, RefreshOptions, RefreshProgress,
    RefreshProjectAttempt, RefreshProjectFailure, RefreshReport, RemovePreviewReport, RemoveReport,
    RenamePreviewReport, RenameReport,
};
pub use workflow::{
    link_project, preview_link_project, preview_remove_project, preview_rename_project,
    refresh_all_projects, refresh_all_projects_best_effort,
    refresh_all_projects_best_effort_with_progress, refresh_project, refresh_project_with_progress,
    remove_project, rename_project,
};
