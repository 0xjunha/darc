use anyhow::{Context, Result};
use darc_rollout::model::NormalizedTurnStep as CodexTurnStep;
use rusqlite::Connection;
use serde_json::from_str;

use super::schema::{
    COMPAT_COLUMN_SETS, SchemaTable, TableColumn, alter_table_add_column_sql, table_has_column,
};
use crate::{
    derived_data::rebuild_derived_analytics_tables, turn_metrics::summarize_stored_turn_metrics,
};

const SELECT_TURNS_REQUIRING_METRICS_BACKFILL_SQL: &str = "
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
";

const UPDATE_TURN_METRICS_SQL: &str = "
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
";

const INSERT_LEGACY_CODEX_SESSIONS_SQL: &str = "
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
";

const INSERT_LEGACY_CODEX_TURNS_SQL: &str = "
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
";

const DROP_LEGACY_CODEX_TURNS_SQL: &str = "DROP TABLE codex_turns";
const DROP_LEGACY_CODEX_SESSIONS_SQL: &str = "DROP TABLE codex_sessions";
const NORMALIZED_CODEX_INDEX_EMPTY_SQL: &str = "
    SELECT
        EXISTS(SELECT 1 FROM sessions WHERE provider = 'codex' LIMIT 1)
        OR EXISTS(SELECT 1 FROM turns WHERE provider = 'codex' LIMIT 1)
";
const READ_USER_VERSION_SQL: &str = "PRAGMA user_version";
const HAS_TABLE_SQL: &str = "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1";

/// Ensures compatibility columns exist on any legacy or pre-derived tables.
pub(super) fn ensure_legacy_compat_columns(connection: &Connection) -> Result<()> {
    for &compat in COMPAT_COLUMN_SETS {
        if !has_table(connection, compat.table)? {
            continue;
        }
        ensure_columns(connection, compat.table, compat.columns, compat.label)?;
    }
    Ok(())
}

/// Applies one-shot schema-version migrations that should not rerun on every DB open.
pub(super) fn migrate_index_db_schema_version(
    connection: &mut Connection,
    schema_version: i32,
) -> Result<()> {
    let current_version = index_db_schema_version(connection)?;
    if current_version >= schema_version {
        return Ok(());
    }

    let transaction = connection
        .transaction()
        .context("failed to begin schema-version migration transaction")?;
    backfill_turn_metrics(&transaction)?;
    rebuild_derived_analytics_tables(&transaction)?;
    set_index_db_schema_version(&transaction, schema_version)?;
    transaction
        .commit()
        .context("failed to commit schema-version migration transaction")?;
    Ok(())
}

/// Rebuilds derived analytics once for normalized databases missing those derived tables.
pub(super) fn compat_backfill_missing_derived_analytics(
    connection: &mut Connection,
    needs_compat_backfill: bool,
) -> Result<()> {
    if !needs_compat_backfill {
        return Ok(());
    }

    let transaction = connection
        .transaction()
        .context("failed to begin compatibility derived-analytics backfill transaction")?;
    rebuild_derived_analytics_tables(&transaction)?;
    transaction
        .commit()
        .context("failed to commit compatibility derived-analytics backfill transaction")?;
    Ok(())
}

/// Migrates any legacy Codex-only index rows into the provider-neutral tables once.
pub(super) fn migrate_legacy_codex_tables(connection: &mut Connection) -> Result<()> {
    let has_legacy_sessions = has_table(connection, SchemaTable::CodexSessions)?;
    let has_legacy_turns = has_table(connection, SchemaTable::CodexTurns)?;
    if !has_legacy_sessions && !has_legacy_turns {
        return Ok(());
    }

    let should_import_legacy_rows = normalized_codex_index_is_empty(connection)?;
    let transaction = connection
        .transaction()
        .context("failed to begin legacy Codex migration transaction")?;

    if should_import_legacy_rows && has_legacy_sessions {
        transaction
            .execute(INSERT_LEGACY_CODEX_SESSIONS_SQL, [])
            .context("failed to migrate legacy Codex sessions into normalized index")?;
    }

    if should_import_legacy_rows && has_legacy_sessions && has_legacy_turns {
        transaction
            .execute(INSERT_LEGACY_CODEX_TURNS_SQL, [])
            .context("failed to migrate legacy Codex turns into normalized index")?;
    }

    if has_legacy_turns {
        transaction
            .execute(DROP_LEGACY_CODEX_TURNS_SQL, [])
            .context("failed to drop legacy codex_turns table")?;
    }
    if has_legacy_sessions {
        transaction
            .execute(DROP_LEGACY_CODEX_SESSIONS_SQL, [])
            .context("failed to drop legacy codex_sessions table")?;
    }
    transaction
        .commit()
        .context("failed to commit legacy Codex migration transaction")?;

    Ok(())
}

/// Returns the current SQLite user-version for the normalized index schema.
pub(super) fn index_db_schema_version(connection: &Connection) -> Result<i32> {
    connection
        .query_row(READ_USER_VERSION_SQL, [], |row| row.get(0))
        .context("failed to read SQLite user_version")
}

/// Returns whether one vetted SQLite table already exists.
pub(super) fn has_table(connection: &Connection, table: SchemaTable) -> Result<bool> {
    let count: i64 = connection
        .query_row(HAS_TABLE_SQL, [table.sql_name()], |row| row.get(0))
        .with_context(|| format!("failed to inspect SQLite table `{}`", table.sql_name()))?;
    Ok(count > 0)
}

/// Persists the current normalized index schema version into SQLite.
fn set_index_db_schema_version(connection: &Connection, version: i32) -> Result<()> {
    connection
        .execute_batch(&format!("PRAGMA user_version = {version}"))
        .with_context(|| format!("failed to set SQLite user_version to {version}"))
}

/// Ensures a vetted table contains every required compatibility column.
fn ensure_columns(
    connection: &Connection,
    table: SchemaTable,
    columns: &[TableColumn],
    label: &str,
) -> Result<()> {
    for &column in columns {
        if table_has_column(connection, table, column.name)? {
            continue;
        }
        connection
            .execute(&alter_table_add_column_sql(table, column), [])
            .with_context(|| format!("failed to add `{}` column to {label}", column.name))?;
    }
    Ok(())
}

/// Backfills derived turn analytics for rows created before these columns existed.
fn backfill_turn_metrics(connection: &Connection) -> Result<()> {
    let mut statement = connection
        .prepare(SELECT_TURNS_REQUIRING_METRICS_BACKFILL_SQL)
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
                UPDATE_TURN_METRICS_SQL,
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

/// Returns whether the normalized index still has no cached Codex rows.
fn normalized_codex_index_is_empty(connection: &Connection) -> Result<bool> {
    let has_rows: i64 = connection
        .query_row(NORMALIZED_CODEX_INDEX_EMPTY_SQL, [], |row| row.get(0))
        .context("failed to inspect normalized Codex index rows")?;
    Ok(has_rows == 0)
}

#[cfg(test)]
/// Prepares migration SQL and executes lightweight smoke checks against the current schema.
pub(super) fn smoke_test_sql(connection: &Connection, schema_version: i32) -> Result<()> {
    connection
        .execute_batch(
            "
            CREATE TEMP TABLE IF NOT EXISTS codex_sessions (
                project_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                archive_path TEXT NOT NULL,
                cwd TEXT NOT NULL,
                cli_version TEXT,
                schema_id TEXT,
                determinism TEXT,
                source_size INTEGER,
                source_mtime_ms INTEGER
            );

            CREATE TEMP TABLE IF NOT EXISTS codex_turns (
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
                steps_json TEXT NOT NULL
            );
            ",
        )
        .context("failed to create temporary legacy tables for SQL smoke testing")?;

    for (label, sql) in [
        (
            "turn metrics backfill select",
            SELECT_TURNS_REQUIRING_METRICS_BACKFILL_SQL,
        ),
        ("turn metrics update", UPDATE_TURN_METRICS_SQL),
        (
            "legacy codex session import",
            INSERT_LEGACY_CODEX_SESSIONS_SQL,
        ),
        ("legacy codex turn import", INSERT_LEGACY_CODEX_TURNS_SQL),
        ("legacy codex turn drop", DROP_LEGACY_CODEX_TURNS_SQL),
        ("legacy codex session drop", DROP_LEGACY_CODEX_SESSIONS_SQL),
        (
            "normalized codex emptiness check",
            NORMALIZED_CODEX_INDEX_EMPTY_SQL,
        ),
        ("read user version", READ_USER_VERSION_SQL),
        ("has table query", HAS_TABLE_SQL),
    ] {
        connection
            .prepare(sql)
            .with_context(|| format!("failed to prepare {label}"))?;
    }

    let scratch = Connection::open_in_memory()
        .context("failed to open scratch SQLite connection for alter-table smoke tests")?;
    scratch
        .execute_batch(
            "
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
                PRIMARY KEY (project_id, provider, session_id, turn_ordinal)
            );

            CREATE TABLE file_accesses (
                project_id TEXT NOT NULL,
                provider TEXT NOT NULL,
                session_id TEXT NOT NULL,
                turn_ordinal INTEGER NOT NULL,
                call_ordinal INTEGER NOT NULL,
                call_id TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                access_type TEXT NOT NULL,
                path TEXT NOT NULL,
                repo_relative_path TEXT
            );

            CREATE TABLE codex_sessions (
                project_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                archive_path TEXT NOT NULL,
                cwd TEXT NOT NULL,
                PRIMARY KEY (project_id, session_id),
                UNIQUE (project_id, archive_path)
            );
            ",
        )
        .context("failed to create scratch pre-migration tables for SQL smoke testing")?;

    for &compat in COMPAT_COLUMN_SETS {
        if !has_table(&scratch, compat.table)? {
            continue;
        }
        for &column in compat.columns {
            scratch
                .execute(&alter_table_add_column_sql(compat.table, column), [])
                .with_context(|| {
                    format!(
                        "failed to execute {} alter smoke test for `{}`",
                        compat.label, column.name
                    )
                })?;
        }
    }

    set_index_db_schema_version(connection, schema_version)?;
    Ok(())
}
