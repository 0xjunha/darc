use std::path::Path;

use anyhow::{Context, Result};
use darc_paths::SourceKind;
use darc_rollout::{ParseDeterminism, model::NormalizedTurn};
use rusqlite::{Connection, params};

use crate::{
    derived_data::{TurnDerivedContext, insert_turn_derived_records},
    index_db::schema::{INSERT_SESSION_SQL, INSERT_TURN_SQL},
    turn_metrics::summarize_turn_metrics,
};

/// Identifies the normalized session shape stored in SQLite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredSessionKind {
    Primary,
    Subagent,
}

impl StoredSessionKind {
    /// Returns the stable SQLite string value for one stored session kind.
    fn as_sql_text(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Subagent => "subagent",
        }
    }
}

/// Carries one normalized session row into the store-owned SQLite writer.
pub struct StoredSessionRecord<'a> {
    pub project_id: &'a str,
    pub provider: SourceKind,
    pub session_id: &'a str,
    pub parent_session_id: Option<&'a str>,
    pub session_kind: StoredSessionKind,
    pub archive_path: &'a str,
    pub cwd: &'a Path,
    pub cli_version: Option<&'a str>,
    pub schema_id: &'a str,
    pub determinism: ParseDeterminism,
    pub source_size: u64,
    pub source_mtime_ms: u64,
}

/// Carries one normalized turn into the store-owned SQLite writer.
pub struct StoredTurnRecord<'a> {
    pub project_id: &'a str,
    pub provider: SourceKind,
    pub session_id: &'a str,
    pub turn_ordinal: i64,
    pub turn: NormalizedTurn,
}

/// Inserts one normalized session row into SQLite.
pub fn insert_session_record(
    connection: &Connection,
    record: &StoredSessionRecord<'_>,
) -> Result<()> {
    let source_size =
        i64::try_from(record.source_size).context("source_size exceeds SQLite INTEGER range")?;
    let source_mtime_ms = i64::try_from(record.source_mtime_ms)
        .context("source_mtime_ms exceeds SQLite INTEGER range")?;

    connection
        .execute(
            INSERT_SESSION_SQL,
            params![
                record.project_id,
                record.provider.directory_name(),
                record.session_id,
                record.parent_session_id,
                record.session_kind.as_sql_text(),
                record.archive_path,
                record.cwd.to_string_lossy(),
                record.cli_version,
                record.schema_id,
                record.determinism.as_sql_text(),
                source_size,
                source_mtime_ms,
            ],
        )
        .with_context(|| {
            format!(
                "failed to insert {} session {}",
                record.provider.title(),
                record.session_id
            )
        })?;
    Ok(())
}

/// Inserts one normalized turn row plus its derived analytics rows into SQLite.
pub fn insert_turn_record(connection: &Connection, record: StoredTurnRecord<'_>) -> Result<()> {
    u64::try_from(record.turn_ordinal).context("turn ordinal is negative while inserting turn")?;
    let metrics = summarize_turn_metrics(&record.turn);
    let NormalizedTurn {
        turn_id,
        user_message,
        final_answer,
        started_at,
        completed_at,
        status,
        primary_model,
        token_usage: _token_usage,
        steps,
    } = record.turn;
    let steps_json = serde_json::to_string(&steps).context("failed to serialize turn steps")?;
    let final_answer_at = final_answer.as_ref().map(|message| &message.timestamp);
    let final_answer_text = final_answer.as_ref().map(|message| &message.text);

    connection
        .execute(
            INSERT_TURN_SQL,
            params![
                record.project_id,
                record.provider.directory_name(),
                record.session_id,
                record.turn_ordinal,
                turn_id,
                started_at,
                completed_at,
                status.as_sql_text(),
                user_message,
                final_answer_at,
                final_answer_text,
                steps_json,
                metrics.step_count,
                metrics.tool_call_count,
                metrics.tool_output_count,
                metrics.attachment_count,
                metrics.delegation_count,
                metrics.hook_summary_count,
                metrics.has_final_answer,
                metrics.duration_ms,
                metrics.effective_agent_runtime_ms,
                metrics.provider_total_token_count,
                metrics.input_uncached_token_count,
                metrics.cache_read_token_count,
                metrics.cache_write_token_count,
                metrics.output_token_count,
                metrics.reasoning_token_count,
                metrics.total_token_count,
                primary_model.as_deref(),
                metrics.changed_file_count,
                metrics.added_line_count,
                metrics.removed_line_count,
            ],
        )
        .with_context(|| {
            format!(
                "failed to insert {} turn {} for session {}",
                record.provider.title(),
                record.turn_ordinal,
                record.session_id
            )
        })?;

    insert_turn_derived_records(
        connection,
        &TurnDerivedContext {
            project_id: record.project_id,
            provider: record.provider,
            session_id: record.session_id,
            turn_ordinal: record.turn_ordinal,
            user_message: &user_message,
            final_answer_text: final_answer_text.map(String::as_str),
        },
        &steps,
    )
    .with_context(|| {
        format!(
            "failed to insert derived analytics for {} turn {} in session {}",
            record.provider.title(),
            record.turn_ordinal,
            record.session_id
        )
    })?;

    Ok(())
}
