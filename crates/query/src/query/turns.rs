use std::path::Path;

use anyhow::{Context, Result};
use darc_paths::SourceKind;
use darc_rollout::model::NormalizedTokenUsage;
use rusqlite::Connection;

use super::{
    FileUsageScope, IndexedTurnRow, ToolUsageScope, TurnDetail, TurnDetailInsights,
    TurnDetailOptions, TurnInsights, open_existing_index_database, optional_sql_count_to_u64,
    parse_provider, parse_turn_status, sort_tool_usage_stats, sort_turn_file_usage_stats,
    sql_count_to_u64,
};
use crate::query::insights::{
    query_file_usage_stats, query_tool_usage_stats, query_turn_shell_commands,
};

const INDEXED_TURN_SQL: &str = "
    SELECT
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
        steps_json,
        primary_model,
        COALESCE(duration_ms, 0),
        effective_agent_runtime_ms,
        provider_total_token_count,
        input_uncached_token_count,
        cache_read_token_count,
        cache_write_token_count,
        output_token_count,
        reasoning_token_count,
        total_token_count,
        COALESCE(changed_file_count, 0),
        COALESCE(added_line_count, 0),
        COALESCE(removed_line_count, 0),
        COALESCE(step_count, 0),
        COALESCE(tool_call_count, 0),
        COALESCE(tool_output_count, 0),
        COALESCE(attachment_count, 0),
        COALESCE(delegation_count, 0),
        COALESCE(hook_summary_count, 0),
        has_final_answer
    FROM turns
    WHERE project_id = ?1 AND provider = ?2 AND session_id = ?3 AND turn_ordinal = ?4
";

const INDEXED_SESSION_TURNS_SQL: &str = "
    SELECT
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
        steps_json,
        primary_model,
        COALESCE(duration_ms, 0),
        effective_agent_runtime_ms,
        provider_total_token_count,
        input_uncached_token_count,
        cache_read_token_count,
        cache_write_token_count,
        output_token_count,
        reasoning_token_count,
        total_token_count,
        COALESCE(changed_file_count, 0),
        COALESCE(added_line_count, 0),
        COALESCE(removed_line_count, 0),
        COALESCE(step_count, 0),
        COALESCE(tool_call_count, 0),
        COALESCE(tool_output_count, 0),
        COALESCE(attachment_count, 0),
        COALESCE(delegation_count, 0),
        COALESCE(hook_summary_count, 0),
        has_final_answer
    FROM turns
    WHERE project_id = ?1 AND provider = ?2 AND session_id = ?3
    ORDER BY turn_ordinal ASC
";

/// Queries one full normalized turn detail payload.
pub fn query_turn_detail(
    index_db_path: &Path,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
    options: TurnDetailOptions,
) -> Result<TurnDetail> {
    let connection = open_existing_index_database(index_db_path)?;
    build_turn_detail(
        &connection,
        project_id,
        provider,
        session_id,
        turn_ordinal,
        options,
    )
}

/// Queries every full normalized turn detail payload for one indexed provider session.
pub fn query_session_turn_details(
    index_db_path: &Path,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    options: TurnDetailOptions,
) -> Result<Vec<TurnDetail>> {
    let connection = open_existing_index_database(index_db_path)?;
    build_session_turn_details(&connection, project_id, provider, session_id, options)
}

/// Queries one turn insights payload for one indexed provider session turn.
pub fn query_turn_insights(
    index_db_path: &Path,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
) -> Result<TurnInsights> {
    let connection = open_existing_index_database(index_db_path)?;
    build_turn_insights(&connection, project_id, provider, session_id, turn_ordinal)
}

/// Builds one normalized turn detail row from the index.
fn build_turn_detail(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
    options: TurnDetailOptions,
) -> Result<TurnDetail> {
    let row = query_indexed_turn_row(connection, project_id, provider, session_id, turn_ordinal)?;
    let insights = options
        .include_insights
        .then(|| build_turn_detail_insights(connection, &row))
        .transpose()?;
    row.into_turn_detail(options, insights)
}

/// Builds every normalized turn detail row for one indexed session.
fn build_session_turn_details(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    options: TurnDetailOptions,
) -> Result<Vec<TurnDetail>> {
    let mut statement = connection
        .prepare(INDEXED_SESSION_TURNS_SQL)
        .context("failed to prepare indexed session turn detail query")?;
    let rows = statement
        .query_map((project_id, provider.directory_name(), session_id), |row| {
            read_indexed_turn_row(row)
        })
        .with_context(|| {
            format!(
                "failed to query indexed turn details for session {session_id} and provider {}",
                provider.directory_name()
            )
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read indexed turn detail rows")?;

    rows.into_iter()
        .map(|row| {
            let insights = options
                .include_insights
                .then(|| build_turn_detail_insights(connection, &row))
                .transpose()?;
            row.into_turn_detail(options, insights)
        })
        .collect()
}

/// Builds one turn insights report from indexed turn, tool, and file rows.
pub(crate) fn build_turn_insights(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
) -> Result<TurnInsights> {
    let row = query_indexed_turn_row(connection, project_id, provider, session_id, turn_ordinal)?;
    let insights = build_turn_detail_insights(connection, &row)?;
    let shell_commands =
        query_turn_shell_commands(connection, project_id, provider, session_id, turn_ordinal)?;
    Ok(row.into_turn_insights(insights, shell_commands))
}

/// Queries one indexed turn row used by turn detail and turn insights.
fn query_indexed_turn_row(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
) -> Result<IndexedTurnRow> {
    let turn_ordinal =
        i64::try_from(turn_ordinal).context("turn ordinal exceeds SQLite INTEGER range")?;
    let row = connection
        .query_row(
            INDEXED_TURN_SQL,
            (
                project_id,
                provider.directory_name(),
                session_id,
                turn_ordinal,
            ),
            read_indexed_turn_row,
        )
        .with_context(|| {
            format!(
                "turn {turn_ordinal} was not found in session {session_id} for provider {}",
                provider.directory_name()
            )
        })?;
    Ok(row)
}

/// Reads one indexed turn row from one SQLite result row.
fn read_indexed_turn_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedTurnRow> {
    Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, String>(2)?,
        row.get::<_, i64>(3)?,
        row.get::<_, Option<String>>(4)?,
        row.get::<_, String>(5)?,
        row.get::<_, Option<String>>(6)?,
        row.get::<_, String>(7)?,
        row.get::<_, String>(8)?,
        row.get::<_, Option<String>>(9)?,
        row.get::<_, Option<String>>(10)?,
        row.get::<_, String>(11)?,
        row.get::<_, Option<String>>(12)?,
        row.get::<_, i64>(13)?,
        row.get::<_, Option<i64>>(14)?,
        row.get::<_, Option<i64>>(15)?,
        row.get::<_, Option<i64>>(16)?,
        row.get::<_, Option<i64>>(17)?,
        row.get::<_, Option<i64>>(18)?,
        row.get::<_, Option<i64>>(19)?,
        row.get::<_, Option<i64>>(20)?,
        row.get::<_, Option<i64>>(21)?,
        row.get::<_, i64>(22)?,
        row.get::<_, i64>(23)?,
        row.get::<_, i64>(24)?,
        row.get::<_, i64>(25)?,
        row.get::<_, i64>(26)?,
        row.get::<_, i64>(27)?,
        row.get::<_, i64>(28)?,
        row.get::<_, i64>(29)?,
        row.get::<_, i64>(30)?,
        row.get::<_, i64>(31)?,
    ))
    .and_then(|row| {
        Ok(IndexedTurnRow {
            project_id: row.0,
            provider: parse_provider(&row.1)
                .map_err(|error| row_conversion_error(1, rusqlite::types::Type::Text, error))?,
            session_id: row.2,
            turn_ordinal: sql_count_to_u64(row.3)
                .map_err(|error| row_conversion_error(3, rusqlite::types::Type::Integer, error))?,
            turn_id: row.4,
            started_at: row.5,
            completed_at: row.6,
            status: parse_turn_status(&row.7)
                .map_err(|error| row_conversion_error(7, rusqlite::types::Type::Text, error))?,
            user_message: row.8,
            final_answer_at: row.9,
            final_answer_text: row.10,
            steps_json: row.11,
            primary_model: row.12,
            duration_ms: sql_count_to_u64(row.13)
                .map_err(|error| row_conversion_error(13, rusqlite::types::Type::Integer, error))?,
            effective_agent_runtime_ms: optional_sql_count_to_u64(row.14)
                .map_err(|error| row_conversion_error(14, rusqlite::types::Type::Integer, error))?,
            token_usage: build_token_usage(row.15, row.16, row.17, row.18, row.19, row.20, row.21)
                .map_err(|error| row_conversion_error(15, rusqlite::types::Type::Integer, error))?,
            total_token_count: optional_sql_count_to_u64(row.21)
                .map_err(|error| row_conversion_error(21, rusqlite::types::Type::Integer, error))?,
            changed_file_count: sql_count_to_u64(row.22)
                .map_err(|error| row_conversion_error(22, rusqlite::types::Type::Integer, error))?,
            added_line_count: sql_count_to_u64(row.23)
                .map_err(|error| row_conversion_error(23, rusqlite::types::Type::Integer, error))?,
            removed_line_count: sql_count_to_u64(row.24)
                .map_err(|error| row_conversion_error(24, rusqlite::types::Type::Integer, error))?,
            step_count: sql_count_to_u64(row.25)
                .map_err(|error| row_conversion_error(25, rusqlite::types::Type::Integer, error))?,
            tool_call_count: sql_count_to_u64(row.26)
                .map_err(|error| row_conversion_error(26, rusqlite::types::Type::Integer, error))?,
            tool_output_count: sql_count_to_u64(row.27)
                .map_err(|error| row_conversion_error(27, rusqlite::types::Type::Integer, error))?,
            attachment_count: sql_count_to_u64(row.28)
                .map_err(|error| row_conversion_error(28, rusqlite::types::Type::Integer, error))?,
            delegation_count: sql_count_to_u64(row.29)
                .map_err(|error| row_conversion_error(29, rusqlite::types::Type::Integer, error))?,
            hook_summary_count: sql_count_to_u64(row.30)
                .map_err(|error| row_conversion_error(30, rusqlite::types::Type::Integer, error))?,
            has_final_answer: row.31 != 0,
        })
    })
}

/// Converts one row-mapping failure into a SQLite row conversion error.
fn row_conversion_error(
    column: usize,
    value_type: rusqlite::types::Type,
    error: anyhow::Error,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        value_type,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error.to_string(),
        )),
    )
}

/// Builds one optional token-usage summary from one indexed turn or session row.
pub(crate) fn build_token_usage(
    provider_total_token_count: Option<i64>,
    input_uncached_token_count: Option<i64>,
    cache_read_token_count: Option<i64>,
    cache_write_token_count: Option<i64>,
    output_token_count: Option<i64>,
    reasoning_token_count: Option<i64>,
    normalized_total_token_count: Option<i64>,
) -> Result<Option<NormalizedTokenUsage>> {
    let token_usage = NormalizedTokenUsage {
        input_uncached_token_count: optional_sql_count_to_u64(input_uncached_token_count)?,
        cache_read_token_count: optional_sql_count_to_u64(cache_read_token_count)?,
        cache_write_token_count: optional_sql_count_to_u64(cache_write_token_count)?,
        output_token_count: optional_sql_count_to_u64(output_token_count)?,
        reasoning_token_count: optional_sql_count_to_u64(reasoning_token_count)?,
        provider_total_token_count: optional_sql_count_to_u64(provider_total_token_count)?,
        normalized_total_token_count: optional_sql_count_to_u64(normalized_total_token_count)?,
    };
    Ok(token_usage.has_any_value().then_some(token_usage))
}

/// Builds one derived insights block for a turn detail payload.
fn build_turn_detail_insights(
    connection: &Connection,
    turn: &IndexedTurnRow,
) -> Result<TurnDetailInsights> {
    let mut tools = query_tool_usage_stats(
        connection,
        ToolUsageScope::Turn {
            project_id: &turn.project_id,
            provider: turn.provider,
            session_id: &turn.session_id,
            turn_ordinal: turn.turn_ordinal,
        },
    )?;
    sort_tool_usage_stats(&mut tools);

    let mut files = query_file_usage_stats(
        connection,
        FileUsageScope::Turn {
            project_id: &turn.project_id,
            provider: turn.provider,
            session_id: &turn.session_id,
            turn_ordinal: turn.turn_ordinal,
        },
    )?;
    sort_turn_file_usage_stats(&mut files);

    Ok(TurnDetailInsights {
        primary_model: turn.primary_model.clone(),
        token_usage: turn.token_usage,
        duration_ms: turn.duration_ms,
        effective_agent_runtime_ms: turn.effective_agent_runtime_ms,
        total_token_count: turn.total_token_count,
        changed_file_count: turn.changed_file_count,
        added_line_count: turn.added_line_count,
        removed_line_count: turn.removed_line_count,
        tool_call_count: turn.tool_call_count,
        tool_output_count: turn.tool_output_count,
        attachment_count: turn.attachment_count,
        delegation_count: turn.delegation_count,
        hook_summary_count: turn.hook_summary_count,
        has_final_answer: turn.has_final_answer,
        tools,
        files,
    })
}

#[cfg(test)]
/// Prepares the turn-detail SQL statements against one live schema.
pub(super) fn smoke_test_sql(connection: &Connection) -> Result<()> {
    connection
        .prepare(INDEXED_TURN_SQL)
        .context("failed to prepare indexed turn query")?;
    Ok(())
}
