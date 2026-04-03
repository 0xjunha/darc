use std::{fs, path::Path, time::Duration};

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Opens the index database and creates the current schema when missing.
pub(crate) fn open_index_database(path: &Path) -> Result<Connection> {
    create_parent_dir(path)?;

    let mut connection = Connection::open(path)
        .with_context(|| format!("failed to open index database {}", path.display()))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .context("failed to configure SQLite busy timeout")?;
    initialize_index_database(&mut connection)?;
    Ok(connection)
}

/// Ensures the index database file and schema exist.
pub(crate) fn ensure_index_database(path: &Path) -> Result<()> {
    let _connection = open_index_database(path)?;
    Ok(())
}

/// Creates the supported SQLite schema when missing.
fn initialize_index_database(connection: &mut Connection) -> Result<()> {
    connection
        .execute_batch(
            "
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS sessions (
                project_id TEXT NOT NULL,
                provider TEXT NOT NULL,
                session_id TEXT NOT NULL,
                parent_session_id TEXT,
                session_kind TEXT NOT NULL,
                archive_path TEXT NOT NULL,
                cwd TEXT NOT NULL,
                cli_version TEXT,
                schema_id TEXT,
                determinism TEXT,
                source_size INTEGER,
                source_mtime_ms INTEGER,
                PRIMARY KEY (project_id, provider, session_id),
                UNIQUE (project_id, archive_path)
            );

            CREATE TABLE IF NOT EXISTS turns (
                project_id TEXT NOT NULL,
                provider TEXT NOT NULL,
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
                PRIMARY KEY (project_id, provider, session_id, turn_ordinal),
                FOREIGN KEY (project_id, provider, session_id)
                    REFERENCES sessions(project_id, provider, session_id)
                    ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS turns_project_provider_started_idx
                ON turns (project_id, provider, started_at);
            ",
        )
        .context("failed to initialize index database schema")?;
    if has_table(connection, "codex_sessions")? {
        ensure_codex_session_columns(connection)?;
    }
    migrate_legacy_codex_tables(connection)?;
    Ok(())
}

/// Ensures `codex_sessions` contains all columns required by the current parser schema.
fn ensure_codex_session_columns(connection: &Connection) -> Result<()> {
    for (column, sql_type) in [
        ("cli_version", "TEXT"),
        ("schema_id", "TEXT"),
        ("determinism", "TEXT"),
        ("source_size", "INTEGER"),
        ("source_mtime_ms", "INTEGER"),
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

/// Migrates any legacy Codex-only index rows into the provider-neutral tables once.
fn migrate_legacy_codex_tables(connection: &mut Connection) -> Result<()> {
    let has_legacy_sessions = has_table(connection, "codex_sessions")?;
    let has_legacy_turns = has_table(connection, "codex_turns")?;
    if !has_legacy_sessions && !has_legacy_turns {
        return Ok(());
    }

    let should_import_legacy_rows = normalized_index_is_empty(connection)?;
    let transaction = connection
        .transaction()
        .context("failed to begin legacy Codex migration transaction")?;

    if should_import_legacy_rows && has_legacy_sessions {
        transaction
            .execute(
                "
                INSERT OR IGNORE INTO sessions (
                    project_id,
                    provider,
                    session_id,
                    parent_session_id,
                    session_kind,
                    archive_path,
                    cwd,
                    cli_version,
                    schema_id,
                    determinism,
                    source_size,
                    source_mtime_ms
                )
                SELECT
                    project_id,
                    'codex',
                    session_id,
                    NULL,
                    'primary',
                    archive_path,
                    cwd,
                    cli_version,
                    schema_id,
                    determinism,
                    source_size,
                    source_mtime_ms
                FROM codex_sessions
                ",
                [],
            )
            .context("failed to migrate legacy Codex sessions into normalized index")?;
    }

    if should_import_legacy_rows && has_legacy_sessions && has_legacy_turns {
        transaction
            .execute(
                "
                INSERT OR IGNORE INTO turns (
                    project_id,
                    provider,
                    session_id,
                    turn_ordinal,
                    turn_id,
                    started_at,
                    completed_at,
                    status,
                    user_message,
                    final_answer_at,
                    final_answer_text,
                    steps_json
                )
                SELECT
                    project_id,
                    'codex',
                    session_id,
                    turn_ordinal,
                    turn_id,
                    started_at,
                    completed_at,
                    status,
                    user_message,
                    final_answer_at,
                    final_answer_text,
                    steps_json
                FROM codex_turns
                ",
                [],
            )
            .context("failed to migrate legacy Codex turns into normalized index")?;
    }

    if has_legacy_turns {
        transaction
            .execute("DROP TABLE codex_turns", [])
            .context("failed to drop legacy codex_turns table")?;
    }
    if has_legacy_sessions {
        transaction
            .execute("DROP TABLE codex_sessions", [])
            .context("failed to drop legacy codex_sessions table")?;
    }
    transaction
        .commit()
        .context("failed to commit legacy Codex migration transaction")?;

    Ok(())
}

/// Returns whether the normalized provider-neutral index still has no cached rows.
fn normalized_index_is_empty(connection: &Connection) -> Result<bool> {
    let has_rows: i64 = connection
        .query_row(
            "
            SELECT
                EXISTS(SELECT 1 FROM sessions LIMIT 1)
                OR EXISTS(SELECT 1 FROM turns LIMIT 1)
            ",
            [],
            |row| row.get(0),
        )
        .context("failed to inspect normalized index rows")?;
    Ok(has_rows == 0)
}

/// Returns whether one SQLite table already exists.
fn has_table(connection: &Connection, table: &str) -> Result<bool> {
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .with_context(|| format!("failed to inspect SQLite table `{table}`"))?;
    Ok(count > 0)
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

    /// Returns whether one named SQLite table currently exists.
    fn sqlite_table_exists(connection: &Connection, table: &str) -> Result<bool> {
        let table_count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )?;
        Ok(table_count > 0)
    }

    #[test]
    fn open_index_database_migrates_legacy_codex_rows_into_normalized_tables() -> Result<()> {
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

            CREATE TABLE codex_turns (
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
                PRIMARY KEY (project_id, session_id, turn_ordinal)
            );

            INSERT INTO codex_sessions (project_id, session_id, archive_path, cwd)
            VALUES ('project', 'session', 'codex/rollout.jsonl', '/tmp/repo');

            INSERT INTO codex_turns (
                project_id,
                session_id,
                turn_ordinal,
                turn_id,
                started_at,
                completed_at,
                status,
                user_message,
                final_answer_at,
                final_answer_text,
                steps_json
            )
            VALUES (
                'project',
                'session',
                0,
                'turn-1',
                '2026-04-01T00:00:00Z',
                '2026-04-01T00:00:01Z',
                'completed',
                'Task',
                '2026-04-01T00:00:01Z',
                'Reply',
                '[]'
            );
            ",
        )?;
        drop(connection);

        let migrated = open_index_database(&path)?;
        let preserved_row: (String, String, String, String, String, String) = migrated.query_row(
            "
            SELECT project_id, provider, session_id, session_kind, archive_path, cwd
            FROM sessions
            WHERE project_id = 'project' AND provider = 'codex' AND session_id = 'session'
            ",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        let migrated_turn: (String, String, String) = migrated.query_row(
            "
            SELECT user_message, final_answer_text, steps_json
            FROM turns
            WHERE project_id = 'project' AND provider = 'codex' AND session_id = 'session'
                AND turn_ordinal = 0
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

        assert_eq!(
            preserved_row,
            (
                "project".to_owned(),
                "codex".to_owned(),
                "session".to_owned(),
                "primary".to_owned(),
                "codex/rollout.jsonl".to_owned(),
                "/tmp/repo".to_owned(),
            )
        );
        assert_eq!(
            migrated_turn,
            ("Task".to_owned(), "Reply".to_owned(), "[]".to_owned())
        );
        assert!(!sqlite_table_exists(&migrated, "codex_sessions")?);
        assert!(!sqlite_table_exists(&migrated, "codex_turns")?);

        Ok(())
    }

    #[test]
    fn open_index_database_prefers_existing_normalized_rows_over_legacy_tables() -> Result<()>
    {
        let path = unique_db_path("index-db-prefer-normalized");
        let connection = open_index_database(&path)?;
        connection.execute(
            "
            INSERT INTO sessions (
                project_id,
                provider,
                session_id,
                parent_session_id,
                session_kind,
                archive_path,
                cwd,
                cli_version,
                schema_id,
                determinism,
                source_size,
                source_mtime_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ",
            (
                "project",
                "claude",
                "claude-session",
                Option::<String>::None,
                "primary",
                "claude/claude-session/claude-session.jsonl",
                "/tmp/repo",
                Some("2.1.87"),
                "claude.primary_transcript",
                "exact",
                Some(10_i64),
                Some(20_i64),
            ),
        )?;
        drop(connection);

        let legacy = Connection::open(&path)?;
        legacy.execute_batch(
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
            VALUES ('project', 'stale-session', 'codex/stale.jsonl', '/tmp/repo');
            ",
        )?;
        drop(legacy);

        let reopened = open_index_database(&path)?;
        let normalized_count: i64 =
            reopened.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        let stale_count: i64 = reopened.query_row(
            "
            SELECT COUNT(*)
            FROM sessions
            WHERE project_id = 'project' AND provider = 'codex' AND session_id = 'stale-session'
            ",
            [],
            |row| row.get(0),
        )?;

        assert_eq!(normalized_count, 1);
        assert_eq!(stale_count, 0);
        assert!(!sqlite_table_exists(&reopened, "codex_sessions")?);

        Ok(())
    }
}
