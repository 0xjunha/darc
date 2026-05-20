//! SQLite storage, migrations, and derived-index analytics shared by indexing and query crates.

mod derived_data;
pub mod evidence;
mod index_db;
pub mod policy;
mod redaction;
mod sharing;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
mod turn_metrics;
mod write;

pub use index_db::{
    INDEX_DB_FILE_NAME, IndexDatabaseRebuildRecommendation, count_project_index_rows_read_only,
    ensure_index_database, open_existing_index_database, open_index_database,
    open_index_database_read_only, open_index_database_writer, preserve_index_sharing_state,
    preserve_index_sharing_state_for_projects, remove_index_database, replace_index_database,
};
pub use sharing::{
    OriginKind, SessionProvenance, SharePolicy, ShareSessionExport, ShareSessionExportState,
    ShareState, ShareStatus, ShareTurnExport, ShareTurnImport, ShareUserRecord,
    clear_project_share_states, import_shared_turn, import_shared_turns, parse_origin_kind,
    parse_share_policy, parse_share_state, project_share_policy, prune_shared_turns,
    query_share_export_session_states, query_share_export_turns, query_share_status,
    set_project_share_policy, set_session_share_state, upsert_share_user,
    validate_shared_session_id, validate_shared_session_kind, validate_shared_turn_status,
};
pub use write::{
    StoredSessionKind, StoredSessionRecord, StoredTurnRecord, insert_session_record,
    insert_turn_record,
};
