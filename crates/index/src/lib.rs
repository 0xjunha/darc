mod engine;
#[cfg(test)]
mod tests;

#[cfg(feature = "test-support")]
pub use darc_store::test_support;
pub use darc_store::{
    INDEX_DB_FILE_NAME, count_project_index_rows_read_only, ensure_index_database, evidence,
    open_existing_index_database, open_index_database, open_index_database_read_only, policy,
};
pub use engine::{
    IndexProgress, IndexReport, ProjectIndexRequest, SkippedCodexRollout, SkippedRollout,
    index_project_archived_codex_turns, index_project_archived_sessions,
    index_project_archived_sessions_with_progress,
};
