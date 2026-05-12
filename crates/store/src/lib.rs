//! SQLite storage, migrations, and derived-index analytics shared by indexing and query crates.

mod derived_data;
pub mod evidence;
mod index_db;
pub mod policy;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
mod turn_metrics;

pub use derived_data::{TurnDerivedContext, insert_turn_derived_records};
pub use index_db::{
    INDEX_DB_FILE_NAME, count_project_index_rows_read_only, ensure_index_database,
    open_existing_index_database, open_index_database, open_index_database_read_only,
    open_index_database_writer, schema,
};
pub use turn_metrics::{IndexedTurnMetrics, summarize_turn_metrics};
