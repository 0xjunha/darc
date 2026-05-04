mod migrations;
pub(crate) mod schema;

use std::{fs, path::Path, time::Duration};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};

use self::{
    migrations::{
        compat_backfill_missing_derived_analytics, ensure_legacy_compat_columns, has_table,
        index_db_schema_version, migrate_index_db_schema_version, migrate_legacy_codex_tables,
    },
    schema::{
        DERIVED_ANALYTICS_TABLES, SchemaTable, initialize_base_schema,
        initialize_supplemental_schema,
    },
};

/// Tracks one-shot SQLite migrations for derived analytics tables.
const INDEX_DB_SCHEMA_VERSION: i32 = 19;

/// Opens the index database and creates the current schema when missing.
pub fn open_index_database(path: &Path) -> Result<Connection> {
    create_parent_dir(path)?;

    let mut connection = Connection::open(path)
        .with_context(|| format!("failed to open index database {}", path.display()))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .context("failed to configure SQLite busy timeout")?;
    initialize_index_database(&mut connection)?;
    Ok(connection)
}

/// Counts one project's indexed rows without initializing or migrating SQLite.
pub fn count_project_index_rows_read_only(path: &Path, project_id: &str) -> Result<(usize, usize)> {
    if !path.exists() {
        return Ok((0, 0));
    }

    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open index database {} read-only", path.display()))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .context("failed to configure SQLite busy timeout")?;

    let mut session_count =
        count_project_rows_if_table_exists(&connection, SchemaTable::Sessions, project_id)?;
    let mut turn_count =
        count_project_rows_if_table_exists(&connection, SchemaTable::Turns, project_id)?;

    if !normalized_codex_index_has_rows(&connection)? {
        session_count += count_project_rows_if_table_exists(
            &connection,
            SchemaTable::CodexSessions,
            project_id,
        )?;
        turn_count +=
            count_project_rows_if_table_exists(&connection, SchemaTable::CodexTurns, project_id)?;
    }

    Ok((session_count, turn_count))
}

/// Ensures the index database file and schema exist.
pub fn ensure_index_database(path: &Path) -> Result<()> {
    let _connection = open_index_database(path)?;
    Ok(())
}

/// Counts rows for one project in a table when that table already exists.
fn count_project_rows_if_table_exists(
    connection: &Connection,
    table: SchemaTable,
    project_id: &str,
) -> Result<usize> {
    if !has_table(connection, table)? {
        return Ok(0);
    }

    let sql = format!(
        "SELECT COUNT(*) FROM {} WHERE project_id = ?1",
        table.sql_name()
    );
    let count: i64 = connection
        .query_row(&sql, params![project_id], |row| row.get(0))
        .with_context(|| {
            format!(
                "failed to count indexed {} rows for project `{project_id}`",
                table.sql_name()
            )
        })?;
    usize::try_from(count)
        .with_context(|| format!("indexed {} count exceeds usize range", table.sql_name()))
}

/// Returns whether normalized Codex rows already exist in the index.
fn normalized_codex_index_has_rows(connection: &Connection) -> Result<bool> {
    Ok(
        table_has_provider_rows(connection, SchemaTable::Sessions, "codex")?
            || table_has_provider_rows(connection, SchemaTable::Turns, "codex")?,
    )
}

/// Returns whether one table has rows for a stored provider value.
fn table_has_provider_rows(
    connection: &Connection,
    table: SchemaTable,
    provider: &str,
) -> Result<bool> {
    if !has_table(connection, table)? {
        return Ok(false);
    }

    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM {} WHERE provider = ?1 LIMIT 1)",
        table.sql_name()
    );
    let has_rows: bool = connection
        .query_row(&sql, params![provider], |row| row.get(0))
        .with_context(|| {
            format!(
                "failed to inspect indexed {} provider rows",
                table.sql_name()
            )
        })?;
    Ok(has_rows)
}

/// Creates the supported SQLite schema when missing.
fn initialize_index_database(connection: &mut Connection) -> Result<()> {
    let needs_derived_analytics_compat_backfill = index_db_schema_version(connection)?
        >= INDEX_DB_SCHEMA_VERSION
        && managed_tables_are_missing(connection, DERIVED_ANALYTICS_TABLES)?;
    initialize_base_schema(connection)?;
    ensure_legacy_compat_columns(connection)?;
    initialize_supplemental_schema(connection)?;
    migrate_legacy_codex_tables(connection)?;
    migrate_index_db_schema_version(connection, INDEX_DB_SCHEMA_VERSION)?;
    compat_backfill_missing_derived_analytics(connection, needs_derived_analytics_compat_backfill)?;
    Ok(())
}

/// Returns whether any managed table from one vetted list is missing.
fn managed_tables_are_missing(connection: &Connection, tables: &[SchemaTable]) -> Result<bool> {
    for &table in tables {
        if !has_table(connection, table)? {
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
    use darc_paths::SourceKind;
    use rusqlite::Connection;

    use super::{
        INDEX_DB_SCHEMA_VERSION, count_project_index_rows_read_only, migrations,
        open_index_database, schema,
    };
    use crate::test_support::{
        IndexedSessionFixture, IndexedTurnFixture, create_pre_analytics_index_schema,
        insert_indexed_session, insert_indexed_turn, insert_pre_analytics_turn,
        seed_legacy_codex_index,
    };

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

    /// Returns whether one named SQLite schema object currently exists.
    fn sqlite_object_exists(connection: &Connection, kind: &str, name: &str) -> Result<bool> {
        let object_count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
            (kind, name),
            |row| row.get(0),
        )?;
        Ok(object_count > 0)
    }

    /// Drops every managed secondary schema object so reopen must recreate them.
    fn drop_managed_secondary_schema_objects(connection: &Connection) -> Result<()> {
        for object in schema::SUPPLEMENTAL_SCHEMA_OBJECTS.iter().rev() {
            connection.execute_batch(&format!(
                "{} {};",
                object.kind.drop_statement_prefix(),
                object.name
            ))?;
        }
        Ok(())
    }

    /// Drops every registered compatibility column from any table that currently exists.
    fn drop_registered_compat_columns(connection: &Connection) -> Result<()> {
        for compat in schema::COMPAT_COLUMN_SETS {
            if !migrations::has_table(connection, compat.table)? {
                continue;
            }
            for column in compat.columns {
                if !schema::table_has_column(connection, compat.table, column.name)? {
                    continue;
                }
                connection.execute_batch(&format!(
                    "ALTER TABLE {} DROP COLUMN {};",
                    compat.table.sql_name(),
                    column.name
                ))?;
            }
        }
        Ok(())
    }

    /// Asserts that every registered compatibility column exists after reopen.
    fn assert_registered_compat_columns_present(connection: &Connection) -> Result<()> {
        for compat in schema::COMPAT_COLUMN_SETS {
            if !migrations::has_table(connection, compat.table)? {
                continue;
            }
            for column in compat.columns {
                assert!(
                    schema::table_has_column(connection, compat.table, column.name)?,
                    "expected {}.{} to exist after reopen",
                    compat.label,
                    column.name
                );
            }
        }
        Ok(())
    }

    /// Asserts that every managed secondary schema object exists after reopen.
    fn assert_managed_secondary_schema_objects_present(connection: &Connection) -> Result<()> {
        for object in schema::SUPPLEMENTAL_SCHEMA_OBJECTS {
            assert!(
                sqlite_object_exists(connection, object.kind.sqlite_master_type(), object.name)?,
                "expected managed {} `{}` to exist after reopen",
                object.kind.sqlite_master_type(),
                object.name
            );
        }
        Ok(())
    }

    #[test]
    fn open_index_database_smoke_tests_current_sql_statements() -> Result<()> {
        let path = unique_db_path("index-db-sql-smoke-current");
        let connection = open_index_database(&path)?;
        schema::smoke_test_sql(&connection)?;
        migrations::smoke_test_sql(&connection, INDEX_DB_SCHEMA_VERSION)?;
        assert!(sqlite_table_exists(&connection, "sessions")?);
        Ok(())
    }

    #[test]
    fn open_index_database_smoke_tests_current_sql_after_legacy_migration() -> Result<()> {
        let path = unique_db_path("index-db-sql-smoke-legacy");
        let connection = Connection::open(&path)?;
        seed_legacy_codex_index(&connection)?;
        drop(connection);

        let migrated = open_index_database(&path)?;
        schema::smoke_test_sql(&migrated)?;
        migrations::smoke_test_sql(&migrated, INDEX_DB_SCHEMA_VERSION)?;
        Ok(())
    }

    #[test]
    fn open_index_database_migrates_legacy_codex_rows_into_normalized_tables() -> Result<()> {
        let path = unique_db_path("index-db-migrate");
        let connection = Connection::open(&path)?;
        seed_legacy_codex_index(&connection)?;
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
    fn count_project_index_rows_read_only_counts_legacy_rows_without_migrating() -> Result<()> {
        let path = unique_db_path("read-only-count-legacy");
        let connection = Connection::open(&path)?;
        seed_legacy_codex_index(&connection)?;
        drop(connection);

        let (session_count, turn_count) = count_project_index_rows_read_only(&path, "project")?;

        assert_eq!(session_count, 1);
        assert_eq!(turn_count, 1);
        let reopened = Connection::open(&path)?;
        assert!(sqlite_table_exists(&reopened, "codex_sessions")?);
        assert!(sqlite_table_exists(&reopened, "codex_turns")?);
        assert!(!sqlite_table_exists(&reopened, "sessions")?);
        assert!(!sqlite_table_exists(&reopened, "turns")?);

        Ok(())
    }

    #[test]
    fn open_index_database_prefers_existing_normalized_codex_rows_over_legacy_tables() -> Result<()>
    {
        let path = unique_db_path("index-db-prefer-normalized");
        let connection = open_index_database(&path)?;
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new("project", SourceKind::Codex, "session-1", "/tmp/repo"),
        )?;
        drop(connection);

        let legacy = Connection::open(&path)?;
        seed_legacy_codex_index(&legacy)?;
        drop(legacy);

        let reopened = open_index_database(&path)?;
        let normalized_count: i64 =
            reopened.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        let codex_count: i64 = reopened.query_row(
            "SELECT COUNT(*) FROM sessions WHERE provider = 'codex'",
            [],
            |row| row.get(0),
        )?;

        assert_eq!(normalized_count, 1);
        assert_eq!(codex_count, 1);
        assert!(!sqlite_table_exists(&reopened, "codex_sessions")?);

        Ok(())
    }

    #[test]
    fn open_index_database_imports_legacy_codex_rows_when_only_other_provider_rows_exist()
    -> Result<()> {
        let path = unique_db_path("index-db-import-with-claude-only");
        let connection = open_index_database(&path)?;
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new(
                "project",
                SourceKind::Claude,
                "claude-session",
                "/tmp/repo",
            ),
        )?;
        drop(connection);

        let legacy = Connection::open(&path)?;
        seed_legacy_codex_index(&legacy)?;
        drop(legacy);

        let reopened = open_index_database(&path)?;
        let codex_count: i64 = reopened.query_row(
            "SELECT COUNT(*) FROM sessions WHERE provider = 'codex' AND session_id = 'session'",
            [],
            |row| row.get(0),
        )?;
        let codex_turn_count: i64 = reopened.query_row(
            "SELECT COUNT(*) FROM turns WHERE provider = 'codex' AND session_id = 'session'",
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
        create_pre_analytics_index_schema(&connection)?;
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new("project", SourceKind::Claude, "session", "/tmp/repo"),
        )?;
        insert_pre_analytics_turn(
            &connection,
            IndexedTurnFixture {
                turn_id: Some("turn-1"),
                completed_at: Some("2026-04-01T00:00:05Z"),
                status: "completed",
                user_message: "Inspect README",
                final_answer_at: Some("2026-04-01T00:00:05Z"),
                final_answer_text: Some("# Audit Fixture"),
                ..IndexedTurnFixture::new(
                    "project",
                    SourceKind::Claude,
                    "session",
                    0,
                    "2026-04-01T00:00:00Z",
                    "completed",
                    "[{\"type\":\"tool_call\",\"timestamp\":\"2026-04-01T00:00:01Z\",\"call_id\":\"tool-1\",\"name\":\"Read\",\"arguments\":\"{\\\"file_path\\\":\\\"README.md\\\"}\"},{\"type\":\"tool_call_output\",\"timestamp\":\"2026-04-01T00:00:02Z\",\"call_id\":\"tool-1\",\"output\":\"# Audit Fixture\"},{\"type\":\"delegation\",\"timestamp\":\"2026-04-01T00:00:03Z\",\"call_id\":\"tool-1\",\"task_id\":null,\"event\":\"completed\",\"agent_id\":\"agent-1\",\"agent_type\":\"general-purpose\",\"status\":\"completed\",\"summary\":\"done\",\"payload_json\":\"{}\"},{\"type\":\"attachment\",\"timestamp\":\"2026-04-01T00:00:04Z\",\"attachment_type\":\"deferred_tools_delta\",\"payload_json\":\"{}\"}]",
                )
            },
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
        let tool_call_row: (String, String, String, i64) = reopened.query_row(
            "
            SELECT call_id, tool_name, arguments_text, is_error
            FROM tool_calls
            WHERE project_id = 'project' AND provider = 'claude' AND session_id = 'session'
                AND turn_ordinal = 0 AND call_ordinal = 0
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let file_access_row: (String, String, String) = reopened.query_row(
            "
            SELECT tool_name, access_type, path
            FROM file_accesses
            WHERE project_id = 'project' AND provider = 'claude' AND session_id = 'session'
                AND turn_ordinal = 0 AND call_ordinal = 0
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let evidence_rows: Vec<(String, String)> = {
            let mut statement = reopened.prepare(
                "
                SELECT field, text
                FROM turn_evidence
                WHERE project_id = 'project' AND provider = 'claude' AND session_id = 'session'
                    AND turn_ordinal = 0
                ORDER BY evidence_ordinal ASC
                ",
            )?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let user_version: i32 = reopened.query_row("PRAGMA user_version", [], |row| row.get(0))?;

        assert_eq!(metrics, (4, 1, 1, 1, 1, 0, 1, 5_000));
        assert_eq!(
            tool_call_row,
            (
                "tool-1".to_owned(),
                "Read".to_owned(),
                "{\"file_path\":\"README.md\"}".to_owned(),
                0,
            )
        );
        assert_eq!(
            file_access_row,
            ("Read".to_owned(), "read".to_owned(), "README.md".to_owned())
        );
        assert!(evidence_rows.contains(&("user_message".to_owned(), "Inspect README".to_owned())));
        assert!(evidence_rows.contains(&("final_answer".to_owned(), "# Audit Fixture".to_owned())));
        assert!(evidence_rows.contains(&("tool_name".to_owned(), "Read".to_owned())));
        assert!(evidence_rows.contains(&(
            "tool_arguments".to_owned(),
            "{\"file_path\":\"README.md\"}".to_owned()
        )));
        assert!(evidence_rows.contains(&("tool_output".to_owned(), "# Audit Fixture".to_owned())));
        assert!(evidence_rows.contains(&("delegation_summary".to_owned(), "done".to_owned())));
        assert!(evidence_rows.iter().any(|(field, text)| {
            field == "delegation_metadata"
                && text.contains("\"agent_type\":\"general-purpose\"")
                && text.contains("\"status\":\"completed\"")
        }));
        assert!(evidence_rows.iter().any(|(field, text)| {
            field == "attachment_metadata"
                && text.contains("\"attachment_type\":\"deferred_tools_delta\"")
        }));
        assert_eq!(user_version, INDEX_DB_SCHEMA_VERSION);

        Ok(())
    }

    #[test]
    fn open_index_database_repairs_registered_columns_and_managed_schema_objects() -> Result<()> {
        let path = unique_db_path("index-db-repair-managed-schema");
        let connection = open_index_database(&path)?;
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new("project", SourceKind::Codex, "session", "/tmp/repo"),
        )?;
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture {
                turn_id: Some("turn-1"),
                completed_at: Some("2026-04-01T00:00:05Z"),
                status: "completed",
                user_message: "Inspect README",
                final_answer_at: Some("2026-04-01T00:00:05Z"),
                final_answer_text: Some("done"),
                step_count: 1,
                tool_call_count: 1,
                has_final_answer: true,
                duration_ms: 5_000,
                ..IndexedTurnFixture::new(
                    "project",
                    SourceKind::Codex,
                    "session",
                    0,
                    "2026-04-01T00:00:00Z",
                    "completed",
                    r#"[{"type":"tool_call","timestamp":"2026-04-01T00:00:01Z","call_id":"tool-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"}]"#,
                )
            },
        )?;
        drop_managed_secondary_schema_objects(&connection)?;
        drop_registered_compat_columns(&connection)?;
        connection.execute_batch("PRAGMA user_version = 0;")?;
        drop(connection);

        let reopened = open_index_database(&path)?;
        assert_registered_compat_columns_present(&reopened)?;
        assert_managed_secondary_schema_objects_present(&reopened)?;
        let metrics: (i64, i64, i64, i64) = reopened.query_row(
            "
            SELECT step_count, tool_call_count, has_final_answer, duration_ms
            FROM turns
            WHERE project_id = 'project' AND provider = 'codex' AND session_id = 'session'
                AND turn_ordinal = 0
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let file_access_row: (String, String, String) = reopened.query_row(
            "
            SELECT tool_name, access_type, path
            FROM file_accesses
            WHERE project_id = 'project' AND provider = 'codex' AND session_id = 'session'
                AND turn_ordinal = 0 AND call_ordinal = 0
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let user_version: i32 = reopened.query_row("PRAGMA user_version", [], |row| row.get(0))?;

        assert_eq!(metrics, (1, 1, 1, 5_000));
        assert_eq!(
            file_access_row,
            ("Read".to_owned(), "read".to_owned(), "README.md".to_owned())
        );
        assert_eq!(user_version, INDEX_DB_SCHEMA_VERSION);

        let reopened_again = open_index_database(&path)?;
        assert_registered_compat_columns_present(&reopened_again)?;
        assert_managed_secondary_schema_objects_present(&reopened_again)?;

        Ok(())
    }

    #[test]
    fn open_index_database_backfills_missing_derived_tables_for_version_one_indexes() -> Result<()>
    {
        let path = unique_db_path("index-db-compat-derived-backfill");
        let connection = open_index_database(&path)?;
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new("project", SourceKind::Codex, "session", "/tmp/repo"),
        )?;
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture {
                turn_id: Some("turn-1"),
                completed_at: Some("2026-04-01T00:00:05Z"),
                status: "completed",
                user_message: "Inspect README",
                final_answer_at: Some("2026-04-01T00:00:05Z"),
                final_answer_text: Some("done"),
                step_count: 1,
                tool_call_count: 1,
                has_final_answer: true,
                duration_ms: 5_000,
                ..IndexedTurnFixture::new(
                    "project",
                    SourceKind::Codex,
                    "session",
                    0,
                    "2026-04-01T00:00:00Z",
                    "completed",
                    r#"[{"type":"tool_call","timestamp":"2026-04-01T00:00:01Z","call_id":"tool-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"}]"#,
                )
            },
        )?;
        connection.execute_batch(
            "
            DROP TABLE turn_evidence;
            DROP TABLE file_accesses;
            DROP TABLE tool_calls;
            ",
        )?;
        drop(connection);

        let reopened = open_index_database(&path)?;
        let tool_call_row: (String, String, String, i64) = reopened.query_row(
            "
            SELECT call_id, tool_name, arguments_text, is_error
            FROM tool_calls
            WHERE project_id = 'project' AND provider = 'codex' AND session_id = 'session'
                AND turn_ordinal = 0 AND call_ordinal = 0
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let file_access_row: (String, String, String) = reopened.query_row(
            "
            SELECT tool_name, access_type, path
            FROM file_accesses
            WHERE project_id = 'project' AND provider = 'codex' AND session_id = 'session'
                AND turn_ordinal = 0 AND call_ordinal = 0
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let evidence_count: i64 = reopened.query_row(
            "
            SELECT COUNT(*)
            FROM turn_evidence
            WHERE project_id = 'project' AND provider = 'codex' AND session_id = 'session'
                AND turn_ordinal = 0
            ",
            [],
            |row| row.get(0),
        )?;
        let user_version: i32 = reopened.query_row("PRAGMA user_version", [], |row| row.get(0))?;

        assert_eq!(
            tool_call_row,
            (
                "tool-1".to_owned(),
                "Read".to_owned(),
                "{\"file_path\":\"README.md\"}".to_owned(),
                0,
            )
        );
        assert_eq!(
            file_access_row,
            ("Read".to_owned(), "read".to_owned(), "README.md".to_owned())
        );
        assert_eq!(evidence_count, 4);
        assert_eq!(user_version, INDEX_DB_SCHEMA_VERSION);

        Ok(())
    }

    #[test]
    fn open_index_database_rebuilds_file_accesses_for_policy_migrations() -> Result<()> {
        let path = unique_db_path("index-db-file-access-policy-rebuild");
        let connection = open_index_database(&path)?;
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new("project", SourceKind::Claude, "session", "/tmp/repo"),
        )?;
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture::new(
                "project",
                SourceKind::Claude,
                "session",
                0,
                "2026-04-01T00:00:00Z",
                "completed",
                r#"[{"type":"tool_call","timestamp":"2026-04-01T00:00:01Z","call_id":"tool-1","name":"Bash","arguments":"{\"command\":\"ls README.md 2>&1 && grep foo src/lib.rs 2> errors.log\"}"}]"#,
            ),
        )?;
        connection.execute(
            "
            INSERT INTO file_accesses (
                project_id,
                provider,
                session_id,
                turn_ordinal,
                call_ordinal,
                call_id,
                timestamp,
                tool_name,
                access_type,
                path,
                repo_relative_path,
                file_name
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ",
            rusqlite::params![
                "project",
                "claude",
                "session",
                0_i64,
                0_i64,
                "tool-1",
                "2026-04-01T00:00:02Z",
                "Bash",
                "read",
                "2>&1",
                "2>&1",
                "2>&1",
            ],
        )?;
        connection.execute_batch(&format!(
            "PRAGMA user_version = {};",
            INDEX_DB_SCHEMA_VERSION - 1
        ))?;
        drop(connection);

        let reopened = open_index_database(&path)?;
        let redirection_count: i64 = reopened.query_row(
            "SELECT COUNT(*) FROM file_accesses WHERE path = '2>&1'",
            [],
            |row| row.get(0),
        )?;
        let rebuilt_rows: Vec<(String, String)> = reopened
            .prepare(
                "
                SELECT access_type, path
                FROM file_accesses
                WHERE project_id = 'project' AND provider = 'claude' AND session_id = 'session'
                ORDER BY access_type ASC, path ASC
                ",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let user_version: i32 = reopened.query_row("PRAGMA user_version", [], |row| row.get(0))?;

        assert_eq!(redirection_count, 0);
        assert_eq!(
            rebuilt_rows,
            vec![
                ("list".to_owned(), "README.md".to_owned()),
                ("read".to_owned(), "src/lib.rs".to_owned()),
                ("write".to_owned(), "errors.log".to_owned()),
            ]
        );
        assert_eq!(user_version, INDEX_DB_SCHEMA_VERSION);

        Ok(())
    }

    #[test]
    fn open_index_database_rolls_back_derived_rebuild_failures() -> Result<()> {
        let path = unique_db_path("index-db-atomic-derived-rebuild");
        let connection = open_index_database(&path)?;
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new("project", SourceKind::Codex, "session", "/tmp/repo"),
        )?;
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture {
                turn_id: Some("turn-1"),
                completed_at: Some("2026-04-01T00:00:05Z"),
                status: "completed",
                user_message: "Inspect README",
                final_answer_at: Some("2026-04-01T00:00:05Z"),
                final_answer_text: Some("done"),
                step_count: 1,
                tool_call_count: 1,
                has_final_answer: true,
                duration_ms: 5_000,
                ..IndexedTurnFixture::new(
                    "project",
                    SourceKind::Codex,
                    "session",
                    0,
                    "2026-04-01T00:00:00Z",
                    "completed",
                    r#"[{"type":"tool_call","timestamp":"2026-04-01T00:00:01Z","call_id":"tool-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"}]"#,
                )
            },
        )?;
        connection.execute(
            "
            UPDATE turns
            SET steps_json = 'not json'
            WHERE project_id = 'project' AND provider = 'codex' AND session_id = 'session'
                AND turn_ordinal = 0
            ",
            [],
        )?;
        connection.execute_batch(&format!(
            "PRAGMA user_version = {};",
            INDEX_DB_SCHEMA_VERSION - 1
        ))?;
        drop(connection);

        let error = open_index_database(&path).expect_err("rebuild should fail");
        assert!(
            error
                .to_string()
                .contains("failed to parse stored steps_json")
        );

        let reopened = Connection::open(&path)?;
        let state: (i64, i64, i32) = reopened.query_row(
            "
            SELECT
                (SELECT COUNT(*) FROM tool_calls),
                (SELECT COUNT(*) FROM file_accesses),
                (SELECT user_version FROM pragma_user_version)
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

        assert_eq!(state, (1, 1, INDEX_DB_SCHEMA_VERSION - 1));

        Ok(())
    }
}
