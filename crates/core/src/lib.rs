mod active_project;
pub mod config;
pub(crate) mod constants;
mod index;
mod init;
mod project;
pub mod query;
mod share;
mod status;
mod sync;

pub use config::SourceKind;
pub use darc_store::IndexDatabaseRebuildRecommendation;
pub use index::{
    IndexOptions, IndexReport, SkippedCodexRollout, SkippedRollout, WorkspaceIndexReport,
    index_project_codex_turns, index_project_sessions, index_rebuild_command,
    rebuild_workspace_index,
};
pub use init::{DetectedRolloutSource, InitDraft, default_root_path, prepare_init, write_init};
pub use project::{
    LinkReport, RefreshAllBestEffortReport, RefreshAllReport, RefreshOptions, RefreshProgress,
    RefreshProjectAttempt, RefreshProjectFailure, RefreshReport, RemovePreviewReport, RemoveReport,
    RenamePreviewReport, RenameReport, link_project, preview_link_project, preview_remove_project,
    preview_rename_project, refresh_all_projects, refresh_all_projects_best_effort,
    refresh_all_projects_best_effort_with_progress, refresh_project, refresh_project_with_progress,
    remove_project, rename_project,
};
pub use share::{
    ShareConfigReport, ShareFetchReport, ShareIdentity, ShareKeyInfo, ShareMergeReport,
    SharePolicy, SharePullReport, SharePushReport, ShareState, ShareStatus, add_share_recipient,
    add_share_remote, exclude_all_sessions, fetch_share_branch, include_all_sessions,
    merge_share_branch, pull_share_branch, push_share_branch, remove_share_recipient,
    set_session_share_state, set_share_policy, share_config, share_identity, share_key,
    share_status,
};
pub use status::{
    ProjectStatusReport, StatusProject, StatusSource, StatusSyncCheck, StatusSyncFailure,
    StatusSyncPlan, WorkspaceStatusReport, status_project, status_workspace,
};
pub use sync::{SyncOptions, SyncPlan, SyncReport, execute_sync, prepare_sync};
