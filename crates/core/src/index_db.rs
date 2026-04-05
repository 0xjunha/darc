use std::{fs, path::Path, time::Duration};

use anyhow::{Context, Result};
use darc_rollout::model::NormalizedTurnStep as CodexTurnStep;
use rusqlite::Connection;
use serde_json::from_str;

use crate::turn_metrics::summarize_stored_turn_metrics;

const INDEX_DB_SCHEMA_VERSION: i32 = 1;

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
                step_count INTEGER NOT NULL DEFAULT 0,
                tool_call_count INTEGER NOT NULL DEFAULT 0,
                tool_output_count INTEGER NOT NULL DEFAULT 0,
                attachment_count INTEGER NOT NULL DEFAULT 0,
                delegation_count INTEGER NOT NULL DEFAULT 0,
                hook_summary_count INTEGER NOT NULL DEFAULT 0,
                has_final_answer INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER,
                PRIMARY KEY (project_id, provider, session_id, turn_ordinal),
                FOREIGN KEY (project_id, provider, session_id)
                    REFERENCES sessions(project_id, provider, session_id)
                    ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS turns_project_provider_started_idx
                ON turns (project_id, provider, started_at);
            CREATE INDEX IF NOT EXISTS sessions_project_provider_schema_idx
                ON sessions (project_id, provider, schema_id, determinism);
            ",
        )
        .context("failed to initialize index database schema")?;
    ensure_turn_columns(connection)?;
    if has_table(connection, "codex_sessions")? {
        ensure_codex_session_columns(connection)?;
    }
    migrate_legacy_codex_tables(connection)?;
    migrate_index_db_schema_version(connection)?;
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

/// Ensures `turns` contains all derived analytics columns required by the current parser schema.
fn ensure_turn_columns(connection: &Connection) -> Result<()> {
    for (column, sql_type) in [
        ("step_count", "INTEGER NOT NULL DEFAULT 0"),
        ("tool_call_count", "INTEGER NOT NULL DEFAULT 0"),
        ("tool_output_count", "INTEGER NOT NULL DEFAULT 0"),
        ("attachment_count", "INTEGER NOT NULL DEFAULT 0"),
        ("delegation_count", "INTEGER NOT NULL DEFAULT 0"),
        ("hook_summary_count", "INTEGER NOT NULL DEFAULT 0"),
        ("has_final_answer", "INTEGER NOT NULL DEFAULT 0"),
        ("duration_ms", "INTEGER"),
    ] {
        if has_table_column(connection, "turns", column)? {
            continue;
        }
        connection
            .execute(
                &format!("ALTER TABLE turns ADD COLUMN {column} {sql_type}"),
                [],
            )
            .with_context(|| format!("failed to add `{column}` column to turns"))?;
    }
    Ok(())
}

/// Backfills derived turn analytics for rows created before these columns existed.
fn backfill_turn_metrics(connection: &Connection) -> Result<()> {
    let mut statement = connection
        .prepare(
            "
            SELECT
                project_id,
                provider,
                session_id,
                turn_ordinal,
                started_at,
                completed_at,
                final_answer_at,
                final_answer_text,
                steps_json
            FROM turns
            WHERE duration_ms IS NULL
                OR (has_final_answer = 0 AND (final_answer_at IS NOT NULL OR final_answer_text IS NOT NULL))
                OR (
                    step_count = 0
                    AND tool_call_count = 0
                    AND tool_output_count = 0
                    AND attachment_count = 0
                    AND delegation_count = 0
                    AND hook_summary_count = 0
                    AND steps_json <> '[]'
                )
            ",
        )
        .context("failed to prepare turn analytics backfill query")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, String>(8)?,
            ))
        })
        .context("failed to query turns that need analytics backfill")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read turn analytics backfill rows")?;
    drop(statement);

    for (
        project_id,
        provider,
        session_id,
        turn_ordinal,
        started_at,
        completed_at,
        final_answer_at,
        final_answer_text,
        steps_json,
    ) in rows
    {
        let steps: Vec<CodexTurnStep> = from_str(&steps_json).with_context(|| {
            format!(
                "failed to parse stored steps_json while backfilling turn analytics for {provider}/{session_id}#{turn_ordinal}"
            )
        })?;
        let metrics = summarize_stored_turn_metrics(
            &started_at,
            completed_at.as_deref(),
            final_answer_at.as_deref(),
            final_answer_text.as_deref(),
            &steps,
        );
        connection
            .execute(
                "
                UPDATE turns
                SET
                    step_count = ?1,
                    tool_call_count = ?2,
                    tool_output_count = ?3,
                    attachment_count = ?4,
                    delegation_count = ?5,
                    hook_summary_count = ?6,
                    has_final_answer = ?7,
                    duration_ms = ?8
                WHERE project_id = ?9 AND provider = ?10 AND session_id = ?11 AND turn_ordinal = ?12
                ",
                (
                    metrics.step_count,
                    metrics.tool_call_count,
                    metrics.tool_output_count,
                    metrics.attachment_count,
                    metrics.delegation_count,
                    metrics.hook_summary_count,
                    metrics.has_final_answer,
                    metrics.duration_ms,
                    project_id.as_str(),
                    provider.as_str(),
                    session_id.as_str(),
                    turn_ordinal,
                ),
            )
            .with_context(|| {
                format!(
                    "failed to update backfilled turn analytics for {provider}/{session_id}#{turn_ordinal}"
                )
            })?;
    }

    Ok(())
}

/// Applies one-shot schema-version migrations that should not rerun on every DB open.
fn migrate_index_db_schema_version(connection: &Connection) -> Result<()> {
    let current_version = index_db_schema_version(connection)?;
    if current_version >= INDEX_DB_SCHEMA_VERSION {
        return Ok(());
    }

    backfill_turn_metrics(connection)?;
    set_index_db_schema_version(connection, INDEX_DB_SCHEMA_VERSION)?;
    Ok(())
}

/// Migrates any legacy Codex-only index rows into the provider-neutral tables once.
fn migrate_legacy_codex_tables(connection: &mut Connection) -> Result<()> {
    let has_legacy_sessions = has_table(connection, "codex_sessions")?;
    let has_legacy_turns = has_table(connection, "codex_turns")?;
    if !has_legacy_sessions && !has_legacy_turns {
        return Ok(());
    }

    let should_import_legacy_rows = normalized_codex_index_is_empty(connection)?;
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

/// Returns whether the normalized index still has no cached Codex rows.
fn normalized_codex_index_is_empty(connection: &Connection) -> Result<bool> {
    let has_rows: i64 = connection
        .query_row(
            "
            SELECT
                EXISTS(SELECT 1 FROM sessions WHERE provider = 'codex' LIMIT 1)
                OR EXISTS(SELECT 1 FROM turns WHERE provider = 'codex' LIMIT 1)
            ",
            [],
            |row| row.get(0),
        )
        .context("failed to inspect normalized Codex index rows")?;
    Ok(has_rows == 0)
}

/// Returns the current SQLite user-version for the normalized index schema.
fn index_db_schema_version(connection: &Connection) -> Result<i32> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("failed to read SQLite user_version")
}

/// Persists the current normalized index schema version into SQLite.
fn set_index_db_schema_version(connection: &Connection, version: i32) -> Result<()> {
    connection
        .execute_batch(&format!("PRAGMA user_version = {version}"))
        .with_context(|| format!("failed to set SQLite user_version to {version}"))
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

    use super::{INDEX_DB_SCHEMA_VERSION, open_index_database};

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
        let migrated_turn: (String, String, String, i64, i64, i64) = migrated.query_row(
            "
            SELECT
                user_message,
                final_answer_text,
                steps_json,
                step_count,
                has_final_answer,
                duration_ms
            FROM turns
            WHERE project_id = 'project' AND provider = 'codex' AND session_id = 'session'
                AND turn_ordinal = 0
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
        let user_version: i32 = migrated.query_row("PRAGMA user_version", [], |row| row.get(0))?;

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
            (
                "Task".to_owned(),
                "Reply".to_owned(),
                "[]".to_owned(),
                0,
                1,
                1_000,
            )
        );
        assert_eq!(user_version, INDEX_DB_SCHEMA_VERSION);
        assert!(!sqlite_table_exists(&migrated, "codex_sessions")?);
        assert!(!sqlite_table_exists(&migrated, "codex_turns")?);

        Ok(())
    }

    #[test]
    fn open_index_database_prefers_existing_normalized_codex_rows_over_legacy_tables() -> Result<()>
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
                "codex",
                "session-1",
                Option::<String>::None,
                "primary",
                "codex/session-1.jsonl",
                "/tmp/repo",
                Some("0.118.0"),
                "codex.turn_lifecycle",
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

    #[test]
    fn open_index_database_imports_legacy_codex_rows_when_only_other_provider_rows_exist()
    -> Result<()> {
        let path = unique_db_path("index-db-import-with-claude-only");
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
                Some(11_i64),
                Some(21_i64),
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
            VALUES ('project', 'legacy-codex', 'codex/legacy-codex.jsonl', '/tmp/repo');

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
                'legacy-codex',
                0,
                'turn-1',
                '2026-04-01T00:00:00Z',
                '2026-04-01T00:00:01Z',
                'completed',
                'Legacy task',
                '2026-04-01T00:00:01Z',
                'Legacy reply',
                '[]'
            );
            ",
        )?;
        drop(legacy);

        let reopened = open_index_database(&path)?;
        let codex_count: i64 = reopened.query_row(
            "SELECT COUNT(*) FROM sessions WHERE provider = 'codex' AND session_id = 'legacy-codex'",
            [],
            |row| row.get(0),
        )?;
        let codex_turn_count: i64 = reopened.query_row(
            "SELECT COUNT(*) FROM turns WHERE provider = 'codex' AND session_id = 'legacy-codex'",
            [],
            |row| row.get(0),
        )?;

        assert_eq!(codex_count, 1);
        assert_eq!(codex_turn_count, 1);
        assert!(!sqlite_table_exists(&reopened, "codex_sessions")?);
        assert!(!sqlite_table_exists(&reopened, "codex_turns")?);

        Ok(())
    }

    #[test]
    fn open_index_database_backfills_derived_turn_metrics_for_existing_rows() -> Result<()> {
        let path = unique_db_path("index-db-backfill-turn-metrics");
        let connection = Connection::open(&path)?;
        connection.execute_batch(
            "
            CREATE TABLE sessions (
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

            CREATE TABLE turns (
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
            )
            VALUES (
                'project',
                'claude',
                'session',
                NULL,
                'primary',
                'claude/session/session.jsonl',
                '/tmp/repo',
                '2.1.90',
                'claude.primary_transcript.2_1_90_to_latest',
                'best_effort_forward',
                10,
                20
            );

            INSERT INTO turns (
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
            VALUES (
                'project',
                'claude',
                'session',
                0,
                'turn-1',
                '2026-04-01T00:00:00Z',
                '2026-04-01T00:00:05Z',
                'completed',
                'Inspect README',
                '2026-04-01T00:00:05Z',
                '# Audit Fixture',
                '[{\"type\":\"tool_call\",\"timestamp\":\"2026-04-01T00:00:01Z\",\"call_id\":\"tool-1\",\"name\":\"Read\",\"arguments\":\"{\\\"file_path\\\":\\\"README.md\\\"}\"},{\"type\":\"tool_call_output\",\"timestamp\":\"2026-04-01T00:00:02Z\",\"call_id\":\"tool-1\",\"output\":\"# Audit Fixture\"},{\"type\":\"delegation\",\"timestamp\":\"2026-04-01T00:00:03Z\",\"call_id\":\"tool-1\",\"task_id\":null,\"event\":\"completed\",\"agent_id\":\"agent-1\",\"agent_type\":\"general-purpose\",\"status\":\"completed\",\"summary\":\"done\",\"payload_json\":\"{}\"},{\"type\":\"attachment\",\"timestamp\":\"2026-04-01T00:00:04Z\",\"attachment_type\":\"deferred_tools_delta\",\"payload_json\":\"{}\"}]'
            );
            ",
        )?;
        drop(connection);

        let reopened = open_index_database(&path)?;
        let metrics: (i64, i64, i64, i64, i64, i64, i64, i64) = reopened.query_row(
            "
            SELECT
                step_count,
                tool_call_count,
                tool_output_count,
                attachment_count,
                delegation_count,
                hook_summary_count,
                has_final_answer,
                duration_ms
            FROM turns
            WHERE project_id = 'project' AND provider = 'claude' AND session_id = 'session' AND turn_ordinal = 0
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
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )?;
        let user_version: i32 = reopened.query_row("PRAGMA user_version", [], |row| row.get(0))?;

        assert_eq!(metrics, (4, 1, 1, 1, 1, 0, 1, 5_000));
        assert_eq!(user_version, INDEX_DB_SCHEMA_VERSION);

        Ok(())
    }
}
