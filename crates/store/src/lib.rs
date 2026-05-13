//! SQLite storage, migrations, and derived-index analytics shared by indexing and query crates.

mod derived_data;
pub mod evidence;
mod index_db;
pub mod policy;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
mod turn_metrics;
mod write;

pub use index_db::{
    INDEX_DB_FILE_NAME, IndexDatabaseRebuildRecommendation, count_project_index_rows_read_only,
    ensure_index_database, open_existing_index_database, open_index_database,
    open_index_database_read_only, open_index_database_writer, remove_index_database,
    replace_index_database,
};
pub use write::{
    StoredSessionKind, StoredSessionRecord, StoredTurnRecord, insert_session_record,
    insert_turn_record,
};
