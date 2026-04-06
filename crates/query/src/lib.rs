mod query;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

use anyhow::{Context, Result};
use darc_index::open_index_database;
pub use query::{
    DailyTimeStat, FileUsageStat, HardDebuggingTurn, ProjectIndexAggregate, ProjectInsights,
    ProjectSummary, ProjectTimeStat, RootAvailability, RootInfo, SessionKind, SessionRuntimeStat,
    SessionSummary, SessionsQueryData, ToolUsageStat, TurnDetail, TurnSummary, TurnsQueryData,
    WorkspaceDailyTimeStat, WorkspaceInsights, WorkspaceQueryData, list_project_index_aggregates,
    query_project_insights, query_project_sessions, query_session_turns, query_turn_detail,
    query_workspace_insights,
};
use rusqlite::Connection;
use serde::Serialize;

/// Stores the explicit project inputs required to report analytics from the index.
#[derive(Debug, Clone)]
pub struct ProjectAnalyticsRequest {
    pub project_id: String,
    pub project_name: String,
    pub project_root: PathBuf,
    pub index_db_path: PathBuf,
}

/// Reports aggregate Claude rollout analytics from the normalized SQLite index.
#[derive(Debug, Clone, Serialize)]
pub struct ClaudeRolloutAnalyticsReport {
    pub project_name: String,
    pub project_root: PathBuf,
    pub index_db_path: PathBuf,
    pub sessions_total: u64,
    pub primary_sessions: u64,
    pub subagent_sessions: u64,
    pub exact_sessions: u64,
    pub best_effort_sessions: u64,
    pub turns_total: u64,
    pub completed_turns: u64,
    pub incomplete_turns: u64,
    pub aborted_turns: u64,
    pub turns_with_final_answer: u64,
    pub turns_with_attachments: u64,
    pub turns_with_delegation: u64,
    pub total_step_count: u64,
    pub total_tool_calls: u64,
    pub total_tool_outputs: u64,
    pub total_attachments: u64,
    pub total_delegation_events: u64,
    pub total_hook_summaries: u64,
    pub total_duration_ms: u64,
    pub average_duration_ms: Option<f64>,
    pub schemas: Vec<ClaudeSchemaAnalytics>,
}

/// Stores one per-schema Claude analytics row.
#[derive(Debug, Clone, Serialize)]
pub struct ClaudeSchemaAnalytics {
    pub schema_id: String,
    pub session_count: u64,
    pub turn_count: u64,
    pub exact_session_count: u64,
    pub best_effort_session_count: u64,
}

/// Reports Claude rollout analytics for one explicit indexed project.
pub fn report_project_claude_rollout_analytics(
    request: &ProjectAnalyticsRequest,
) -> Result<ClaudeRolloutAnalyticsReport> {
    let connection = open_index_database(&request.index_db_path)?;

    let (sessions_total, primary_sessions, subagent_sessions, exact_sessions, best_effort_sessions) =
        query_claude_session_counts(&connection, &request.project_id)?;
    let turn_counts = query_claude_turn_counts(&connection, &request.project_id)?;
    let schemas = query_claude_schema_rows(&connection, &request.project_id)?;

    Ok(ClaudeRolloutAnalyticsReport {
        project_name: request.project_name.clone(),
        project_root: request.project_root.clone(),
        index_db_path: request.index_db_path.clone(),
        sessions_total,
        primary_sessions,
        subagent_sessions,
        exact_sessions,
        best_effort_sessions,
        turns_total: turn_counts.turns_total,
        completed_turns: turn_counts.completed_turns,
        incomplete_turns: turn_counts.incomplete_turns,
        aborted_turns: turn_counts.aborted_turns,
        turns_with_final_answer: turn_counts.turns_with_final_answer,
        turns_with_attachments: turn_counts.turns_with_attachments,
        turns_with_delegation: turn_counts.turns_with_delegation,
        total_step_count: turn_counts.total_step_count,
        total_tool_calls: turn_counts.total_tool_calls,
        total_tool_outputs: turn_counts.total_tool_outputs,
        total_attachments: turn_counts.total_attachments,
        total_delegation_events: turn_counts.total_delegation_events,
        total_hook_summaries: turn_counts.total_hook_summaries,
        total_duration_ms: turn_counts.total_duration_ms,
        average_duration_ms: turn_counts.average_duration_ms,
        schemas,
    })
}

/// Stores one grouped set of indexed Claude turn aggregates.
#[derive(Debug, Clone, Copy, Default)]
struct ClaudeTurnCounts {
    turns_total: u64,
    completed_turns: u64,
    incomplete_turns: u64,
    aborted_turns: u64,
    turns_with_final_answer: u64,
    turns_with_attachments: u64,
    turns_with_delegation: u64,
    total_step_count: u64,
    total_tool_calls: u64,
    total_tool_outputs: u64,
    total_attachments: u64,
    total_delegation_events: u64,
    total_hook_summaries: u64,
    total_duration_ms: u64,
    average_duration_ms: Option<f64>,
}

/// Stores one raw SQLite aggregate row for indexed Claude turn analytics.
type ClaudeTurnCountRow = (
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    Option<f64>,
);

/// Queries the indexed Claude session counts for one project.
fn query_claude_session_counts(
    connection: &Connection,
    project_id: &str,
) -> Result<(u64, u64, u64, u64, u64)> {
    let counts: (i64, i64, i64, i64, i64) = connection
        .query_row(
            "
            SELECT
                COUNT(*),
                COALESCE(SUM(CASE WHEN session_kind = 'primary' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN session_kind = 'subagent' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN determinism = 'exact' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN determinism = 'best_effort_forward' THEN 1 ELSE 0 END), 0)
            FROM sessions
            WHERE project_id = ?1 AND provider = 'claude'
            ",
            [project_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .context("failed to query indexed Claude session counts")?;
    Ok((
        sql_count_to_u64(counts.0, "sessions_total")?,
        sql_count_to_u64(counts.1, "primary_sessions")?,
        sql_count_to_u64(counts.2, "subagent_sessions")?,
        sql_count_to_u64(counts.3, "exact_sessions")?,
        sql_count_to_u64(counts.4, "best_effort_sessions")?,
    ))
}

/// Queries the indexed Claude turn counters for one project.
fn query_claude_turn_counts(connection: &Connection, project_id: &str) -> Result<ClaudeTurnCounts> {
    let counts: ClaudeTurnCountRow = connection
        .query_row(
            "
            SELECT
                COUNT(*),
                COALESCE(SUM(CASE WHEN status = 'completed' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'incomplete' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'aborted' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(has_final_answer), 0),
                COALESCE(SUM(CASE WHEN attachment_count > 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN delegation_count > 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(step_count), 0),
                COALESCE(SUM(tool_call_count), 0),
                COALESCE(SUM(tool_output_count), 0),
                COALESCE(SUM(attachment_count), 0),
                COALESCE(SUM(delegation_count), 0),
                COALESCE(SUM(hook_summary_count), 0),
                AVG(duration_ms)
            FROM turns
            WHERE project_id = ?1 AND provider = 'claude'
            ",
            [project_id],
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
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                ))
            },
        )
        .context("failed to query indexed Claude turn counts")?;

    let total_duration_ms: i64 = connection
        .query_row(
            "
            SELECT COALESCE(SUM(duration_ms), 0)
            FROM turns
            WHERE project_id = ?1 AND provider = 'claude'
            ",
            [project_id],
            |row| row.get(0),
        )
        .context("failed to query indexed Claude duration totals")?;

    Ok(ClaudeTurnCounts {
        turns_total: sql_count_to_u64(counts.0, "turns_total")?,
        completed_turns: sql_count_to_u64(counts.1, "completed_turns")?,
        incomplete_turns: sql_count_to_u64(counts.2, "incomplete_turns")?,
        aborted_turns: sql_count_to_u64(counts.3, "aborted_turns")?,
        turns_with_final_answer: sql_count_to_u64(counts.4, "turns_with_final_answer")?,
        turns_with_attachments: sql_count_to_u64(counts.5, "turns_with_attachments")?,
        turns_with_delegation: sql_count_to_u64(counts.6, "turns_with_delegation")?,
        total_step_count: sql_count_to_u64(counts.7, "total_step_count")?,
        total_tool_calls: sql_count_to_u64(counts.8, "total_tool_calls")?,
        total_tool_outputs: sql_count_to_u64(counts.9, "total_tool_outputs")?,
        total_attachments: sql_count_to_u64(counts.10, "total_attachments")?,
        total_delegation_events: sql_count_to_u64(counts.11, "total_delegation_events")?,
        total_hook_summaries: sql_count_to_u64(counts.12, "total_hook_summaries")?,
        total_duration_ms: sql_count_to_u64(total_duration_ms, "total_duration_ms")?,
        average_duration_ms: counts.13,
    })
}

/// Queries per-schema indexed Claude session and turn counts for one project.
fn query_claude_schema_rows(
    connection: &Connection,
    project_id: &str,
) -> Result<Vec<ClaudeSchemaAnalytics>> {
    let mut statement = connection
        .prepare(
            "
            SELECT
                s.schema_id,
                COUNT(*),
                COALESCE(SUM(CASE WHEN s.determinism = 'exact' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN s.determinism = 'best_effort_forward' THEN 1 ELSE 0 END), 0)
            FROM sessions s
            WHERE s.project_id = ?1 AND s.provider = 'claude'
            GROUP BY s.schema_id
            ORDER BY s.schema_id
            ",
        )
        .context("failed to prepare indexed Claude schema query")?;
    let mut rows = statement
        .query([project_id])
        .context("failed to query indexed Claude schema rows")?;
    let mut schemas = Vec::new();

    while let Some(row) = rows
        .next()
        .context("failed to read indexed Claude schema row")?
    {
        let schema_id: Option<String> = row.get(0).context("failed to read Claude schema id")?;
        let schema_id = schema_id.unwrap_or_else(|| "<unknown>".to_owned());
        let session_count = sql_count_to_u64(
            row.get::<_, i64>(1)
                .context("failed to read Claude schema session count")?,
            "schema session_count",
        )?;
        let exact_session_count = sql_count_to_u64(
            row.get::<_, i64>(2)
                .context("failed to read Claude schema exact session count")?,
            "schema exact_session_count",
        )?;
        let best_effort_session_count = sql_count_to_u64(
            row.get::<_, i64>(3)
                .context("failed to read Claude schema best-effort session count")?,
            "schema best_effort_session_count",
        )?;
        let turn_count = query_schema_turn_count(connection, project_id, &schema_id)?;
        schemas.push(ClaudeSchemaAnalytics {
            schema_id,
            session_count,
            turn_count,
            exact_session_count,
            best_effort_session_count,
        });
    }

    Ok(schemas)
}

/// Queries the indexed turn count for one Claude schema id.
fn query_schema_turn_count(
    connection: &Connection,
    project_id: &str,
    schema_id: &str,
) -> Result<u64> {
    let count: i64 = connection
        .query_row(
            "
            SELECT COUNT(*)
            FROM turns t
            JOIN sessions s
                ON s.project_id = t.project_id
                AND s.provider = t.provider
                AND s.session_id = t.session_id
            WHERE t.project_id = ?1 AND t.provider = 'claude' AND COALESCE(s.schema_id, '<unknown>') = ?2
            ",
            (project_id, schema_id),
            |row| row.get(0),
        )
        .with_context(|| format!("failed to count indexed Claude turns for schema `{schema_id}`"))?;
    sql_count_to_u64(count, "schema turn_count")
}

/// Converts one SQLite aggregate count into an unsigned Rust count.
fn sql_count_to_u64(value: i64, label: &str) -> Result<u64> {
    u64::try_from(value).with_context(|| format!("{label} exceeded u64 range"))
}
