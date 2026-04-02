use std::{fs, path::Path, time::Duration};

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Opens the index database and creates the current schema when missing.
pub(crate) fn open_index_database(path: &Path) -> Result<Connection> {
    create_parent_dir(path)?;

    let connection = Connection::open(path)
        .with_context(|| format!("failed to open index database {}", path.display()))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .context("failed to configure SQLite busy timeout")?;
    initialize_index_database(&connection)?;
    Ok(connection)
}

/// Ensures the index database file and schema exist.
pub(crate) fn ensure_index_database(path: &Path) -> Result<()> {
    let _connection = open_index_database(path)?;
    Ok(())
}

/// Creates the supported SQLite schema when missing.
fn initialize_index_database(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS codex_sessions (
                project_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                archive_path TEXT NOT NULL,
                cwd TEXT NOT NULL,
                PRIMARY KEY (project_id, session_id),
                UNIQUE (project_id, archive_path)
            );

            CREATE TABLE IF NOT EXISTS codex_turns (
                project_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                turn_ordinal INTEGER NOT NULL,
                turn_id TEXT,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                status TEXT NOT NULL,
                user_message TEXT NOT NULL,
                final_answer_at TEXT,
                final_answer_text TEXT,
                steps_json TEXT NOT NULL,
                PRIMARY KEY (project_id, session_id, turn_ordinal),
                FOREIGN KEY (project_id, session_id)
                    REFERENCES codex_sessions(project_id, session_id)
                    ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS codex_turns_project_started_idx
                ON codex_turns (project_id, started_at);
            ",
        )
        .context("failed to initialize index database schema")?;
    Ok(())
}

/// Creates the parent directory for one SQLite database path.
fn create_parent_dir(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("database path {} is missing a parent", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    Ok(())
}
