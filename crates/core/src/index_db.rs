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
                cli_version TEXT,
                schema_id TEXT,
                determinism TEXT,
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
    ensure_codex_session_metadata_columns(connection)?;
    Ok(())
}

/// Ensures `codex_sessions` contains the rollout-schema metadata columns for old databases.
fn ensure_codex_session_metadata_columns(connection: &Connection) -> Result<()> {
    for (column, sql_type) in [
        ("cli_version", "TEXT"),
        ("schema_id", "TEXT"),
        ("determinism", "TEXT"),
    ] {
        if has_table_column(connection, "codex_sessions", column)? {
            continue;
        }
        connection
            .execute(
                &format!("ALTER TABLE codex_sessions ADD COLUMN {column} {sql_type}"),
                [],
            )
            .with_context(|| format!("failed to add `{column}` column to codex_sessions"))?;
    }
    Ok(())
}

/// Returns whether one SQLite table already contains a named column.
fn has_table_column(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .with_context(|| format!("failed to inspect SQLite schema for table `{table}`"))?;
    let mut rows = statement
        .query([])
        .with_context(|| format!("failed to query SQLite schema for table `{table}`"))?;
    while let Some(row) = rows.next().context("failed to read SQLite schema row")? {
        let existing: String = row.get(1).context("failed to read SQLite column name")?;
        if existing == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Creates the parent directory for one SQLite database path.
fn create_parent_dir(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("database path {} is missing a parent", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use anyhow::Result;
    use rusqlite::Connection;

    use super::open_index_database;

    fn unique_db_path(prefix: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "test-{prefix}-{}-{}.sqlite",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ))
    }

    #[test]
    fn open_index_database_migrates_codex_session_metadata_columns() -> Result<()> {
        let path = unique_db_path("index-db-migrate");
        let connection = Connection::open(&path)?;
        connection.execute_batch(
            "
            CREATE TABLE codex_sessions (
                project_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                archive_path TEXT NOT NULL,
                cwd TEXT NOT NULL,
                PRIMARY KEY (project_id, session_id),
                UNIQUE (project_id, archive_path)
            );

            INSERT INTO codex_sessions (project_id, session_id, archive_path, cwd)
            VALUES ('project', 'session', 'codex/rollout.jsonl', '/tmp/repo');
            ",
        )?;
        drop(connection);

        let migrated = open_index_database(&path)?;
        let columns = ["cli_version", "schema_id", "determinism"]
            .into_iter()
            .map(|column| {
                migrated.query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('codex_sessions') WHERE name = ?1",
                    [column],
                    |row| row.get::<_, i64>(0),
                )
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let preserved_row: (String, String, String, String) = migrated.query_row(
            "
            SELECT project_id, session_id, archive_path, cwd
            FROM codex_sessions
            WHERE project_id = 'project' AND session_id = 'session'
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;

        assert_eq!(columns, vec![1, 1, 1]);
        assert_eq!(
            preserved_row,
            (
                "project".to_owned(),
                "session".to_owned(),
                "codex/rollout.jsonl".to_owned(),
                "/tmp/repo".to_owned(),
            )
        );

        Ok(())
    }
}
