use std::path::Path;

use anyhow::{Context, Result};
use darc_paths::SourceKind;
use rusqlite::{Connection, params};

use super::turns::build_token_usage;
use super::{
    ProjectIndexAggregate, SessionSummary, SessionsQueryData, TurnSummary, TurnsQueryData,
    open_existing_index_database, optional_sql_count_to_u64, parse_provider, parse_session_kind,
    parse_turn_status, preview_text, sql_count_to_u64,
};

const PROJECT_INDEX_AGGREGATES_SQL: &str = "
    WITH session_counts AS (
        SELECT
            project_id,
            COUNT(*) AS session_count
        FROM sessions
        GROUP BY project_id
    ),
    turn_counts AS (
        SELECT
            project_id,
            COUNT(*) AS turn_count,
            MAX(started_at) AS last_activity_at
        FROM turns
        GROUP BY project_id
    )
    SELECT
        session_counts.project_id,
        session_counts.session_count,
        COALESCE(turn_counts.turn_count, 0) AS turn_count,
        turn_counts.last_activity_at
    FROM session_counts
    LEFT JOIN turn_counts
        ON turn_counts.project_id = session_counts.project_id
    ORDER BY
        turn_counts.last_activity_at IS NULL ASC,
        turn_counts.last_activity_at DESC,
        session_counts.project_id ASC
";

const PROJECT_SESSIONS_SQL: &str = "
    WITH turn_stats AS (
        SELECT
            project_id,
            provider,
            session_id,
            COUNT(*) AS turn_count,
            MAX(turn_ordinal) AS latest_turn_ordinal,
            MAX(started_at) AS latest_turn_at,
            CASE
                WHEN COUNT(*) = COUNT(total_token_count) THEN SUM(COALESCE(total_token_count, 0))
                ELSE NULL
            END AS total_token_count,
            CASE
                WHEN COUNT(*) = COUNT(provider_total_token_count) THEN SUM(COALESCE(provider_total_token_count, 0))
                ELSE NULL
            END AS provider_total_token_count,
            CASE
                WHEN COUNT(*) = COUNT(input_uncached_token_count) THEN SUM(COALESCE(input_uncached_token_count, 0))
                ELSE NULL
            END AS input_uncached_token_count,
            CASE
                WHEN COUNT(*) = COUNT(cache_read_token_count) THEN SUM(COALESCE(cache_read_token_count, 0))
                ELSE NULL
            END AS cache_read_token_count,
            CASE
                WHEN COUNT(*) = COUNT(cache_write_token_count) THEN SUM(COALESCE(cache_write_token_count, 0))
                ELSE NULL
            END AS cache_write_token_count,
            CASE
                WHEN COUNT(*) = COUNT(output_token_count) THEN SUM(COALESCE(output_token_count, 0))
                ELSE NULL
            END AS output_token_count,
            CASE
                WHEN COUNT(*) = COUNT(reasoning_token_count) THEN SUM(COALESCE(reasoning_token_count, 0))
                ELSE NULL
            END AS reasoning_token_count,
            CASE
                WHEN COUNT(*) = COUNT(effective_agent_runtime_ms) THEN SUM(COALESCE(effective_agent_runtime_ms, 0))
                ELSE NULL
            END AS effective_agent_runtime_ms,
            SUM(COALESCE(changed_file_count, 0)) AS changed_file_count,
            SUM(COALESCE(added_line_count, 0)) AS added_line_count,
            SUM(COALESCE(removed_line_count, 0)) AS removed_line_count
        FROM turns
        WHERE project_id = ?1
        GROUP BY project_id, provider, session_id
    )
    SELECT
        s.project_id,
        s.provider,
        s.session_id,
        s.parent_session_id,
        s.session_kind,
        s.cwd,
        COALESCE(turn_stats.turn_count, 0) AS turn_count,
        turn_stats.latest_turn_at,
        latest.status,
        latest.primary_model,
        turn_stats.total_token_count,
        turn_stats.provider_total_token_count,
        turn_stats.input_uncached_token_count,
        turn_stats.cache_read_token_count,
        turn_stats.cache_write_token_count,
        turn_stats.output_token_count,
        turn_stats.reasoning_token_count,
        turn_stats.effective_agent_runtime_ms,
        COALESCE(turn_stats.changed_file_count, 0),
        COALESCE(turn_stats.added_line_count, 0),
        COALESCE(turn_stats.removed_line_count, 0)
    FROM sessions AS s
    LEFT JOIN turn_stats
        ON turn_stats.project_id = s.project_id
        AND turn_stats.provider = s.provider
        AND turn_stats.session_id = s.session_id
    LEFT JOIN turns AS latest
        ON latest.project_id = turn_stats.project_id
        AND latest.provider = turn_stats.provider
        AND latest.session_id = turn_stats.session_id
        AND latest.turn_ordinal = turn_stats.latest_turn_ordinal
    WHERE s.project_id = ?1
        AND (?2 IS NULL OR julianday(turn_stats.latest_turn_at) >= julianday(?2))
        AND (?3 IS NULL OR julianday(turn_stats.latest_turn_at) < julianday(?3))
    ORDER BY
        turn_stats.latest_turn_at IS NULL ASC,
        turn_stats.latest_turn_at DESC,
        s.provider ASC,
        s.session_id DESC
";

const SESSION_TURNS_SQL: &str = "
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
        has_final_answer,
        step_count,
        primary_model,
        total_token_count,
        provider_total_token_count,
        input_uncached_token_count,
        cache_read_token_count,
        cache_write_token_count,
        output_token_count,
        reasoning_token_count,
        effective_agent_runtime_ms,
        changed_file_count,
        added_line_count,
        removed_line_count
    FROM turns
    WHERE project_id = ?1 AND provider = ?2 AND session_id = ?3
    ORDER BY turn_ordinal ASC
";

/// Queries the indexed project aggregates for one workspace database.
pub fn list_project_index_aggregates(index_db_path: &Path) -> Result<Vec<ProjectIndexAggregate>> {
    let connection = open_existing_index_database(index_db_path)?;
    query_project_index_aggregates(&connection)
}

/// Queries the indexed session list for one project.
pub fn query_project_sessions(
    index_db_path: &Path,
    project_id: &str,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<SessionsQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    Ok(SessionsQueryData {
        project_id: project_id.to_owned(),
        sessions: query_sessions(&connection, project_id, since, until)?,
    })
}

/// Queries the indexed turn list for one session.
pub fn query_session_turns(
    index_db_path: &Path,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
) -> Result<TurnsQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    Ok(TurnsQueryData {
        project_id: project_id.to_owned(),
        provider,
        session_id: session_id.to_owned(),
        turns: query_turns(&connection, project_id, provider, session_id)?,
    })
}

/// Queries the stored project aggregates for every indexed project.
fn query_project_index_aggregates(connection: &Connection) -> Result<Vec<ProjectIndexAggregate>> {
    let mut statement = connection
        .prepare(PROJECT_INDEX_AGGREGATES_SQL)
        .context("failed to prepare project aggregate query")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .context("failed to query project aggregates")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read project aggregate rows")?;
    rows.into_iter()
        .map(
            |(project_id, session_count, turn_count, last_activity_at)| -> Result<_> {
                Ok(ProjectIndexAggregate {
                    project_id,
                    session_count: sql_count_to_u64(session_count)?,
                    turn_count: sql_count_to_u64(turn_count)?,
                    last_activity_at,
                })
            },
        )
        .collect()
}

/// Queries the indexed sessions for one configured project.
fn query_sessions(
    connection: &Connection,
    project_id: &str,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<SessionSummary>> {
    let mut statement = connection
        .prepare(PROJECT_SESSIONS_SQL)
        .context("failed to prepare indexed session query")?;
    let rows = statement
        .query_map(params![project_id, since, until], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<i64>>(10)?,
                row.get::<_, Option<i64>>(11)?,
                row.get::<_, Option<i64>>(12)?,
                row.get::<_, Option<i64>>(13)?,
                row.get::<_, Option<i64>>(14)?,
                row.get::<_, Option<i64>>(15)?,
                row.get::<_, Option<i64>>(16)?,
                row.get::<_, Option<i64>>(17)?,
                row.get::<_, i64>(18)?,
                row.get::<_, i64>(19)?,
                row.get::<_, i64>(20)?,
            ))
        })
        .context("failed to query indexed sessions")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read indexed session rows")?;
    rows.into_iter()
        .map(
            |(
                project_id,
                provider,
                session_id,
                parent_session_id,
                session_kind,
                cwd,
                turn_count,
                latest_turn_at,
                latest_status,
                primary_model,
                total_token_count,
                provider_total_token_count,
                input_uncached_token_count,
                cache_read_token_count,
                cache_write_token_count,
                output_token_count,
                reasoning_token_count,
                effective_agent_runtime_ms,
                changed_file_count,
                added_line_count,
                removed_line_count,
            )|
             -> Result<_> {
                Ok(SessionSummary {
                    project_id,
                    provider: parse_provider(&provider)?,
                    session_id,
                    parent_session_id,
                    session_kind: parse_session_kind(&session_kind)?,
                    cwd,
                    turn_count: sql_count_to_u64(turn_count)?,
                    latest_turn_at,
                    latest_status: latest_status
                        .as_deref()
                        .map(parse_turn_status)
                        .transpose()?,
                    primary_model,
                    token_usage: build_token_usage(
                        provider_total_token_count,
                        input_uncached_token_count,
                        cache_read_token_count,
                        cache_write_token_count,
                        output_token_count,
                        reasoning_token_count,
                        total_token_count,
                    )?,
                    total_token_count: optional_sql_count_to_u64(total_token_count)?,
                    effective_agent_runtime_ms: optional_sql_count_to_u64(
                        effective_agent_runtime_ms,
                    )?,
                    changed_file_count: sql_count_to_u64(changed_file_count)?,
                    added_line_count: sql_count_to_u64(added_line_count)?,
                    removed_line_count: sql_count_to_u64(removed_line_count)?,
                })
            },
        )
        .collect()
}

/// Queries the indexed turns for one provider session.
fn query_turns(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
) -> Result<Vec<TurnSummary>> {
    let mut statement = connection
        .prepare(SESSION_TURNS_SQL)
        .context("failed to prepare indexed turn query")?;
    let rows = statement
        .query_map((project_id, provider.directory_name(), session_id), |row| {
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
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<i64>>(12)?,
                row.get::<_, Option<i64>>(13)?,
                row.get::<_, Option<i64>>(14)?,
                row.get::<_, Option<i64>>(15)?,
                row.get::<_, Option<i64>>(16)?,
                row.get::<_, Option<i64>>(17)?,
                row.get::<_, Option<i64>>(18)?,
                row.get::<_, Option<i64>>(19)?,
                row.get::<_, i64>(20)?,
                row.get::<_, i64>(21)?,
                row.get::<_, i64>(22)?,
            ))
        })
        .context("failed to query indexed turns")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read indexed turn rows")?;
    rows.into_iter()
        .map(
            |(
                project_id,
                provider,
                session_id,
                turn_ordinal,
                turn_id,
                started_at,
                completed_at,
                status,
                user_message,
                has_final_answer,
                step_count,
                primary_model,
                total_token_count,
                provider_total_token_count,
                input_uncached_token_count,
                cache_read_token_count,
                cache_write_token_count,
                output_token_count,
                reasoning_token_count,
                effective_agent_runtime_ms,
                changed_file_count,
                added_line_count,
                removed_line_count,
            )|
             -> Result<_> {
                Ok(TurnSummary {
                    project_id,
                    provider: parse_provider(&provider)?,
                    session_id,
                    turn_ordinal: sql_count_to_u64(turn_ordinal)?,
                    turn_id,
                    started_at,
                    completed_at,
                    status: parse_turn_status(&status)?,
                    user_preview: preview_text(&user_message),
                    has_final_answer: has_final_answer != 0,
                    step_count: sql_count_to_u64(step_count)?,
                    primary_model,
                    token_usage: build_token_usage(
                        provider_total_token_count,
                        input_uncached_token_count,
                        cache_read_token_count,
                        cache_write_token_count,
                        output_token_count,
                        reasoning_token_count,
                        total_token_count,
                    )?,
                    total_token_count: optional_sql_count_to_u64(total_token_count)?,
                    effective_agent_runtime_ms: optional_sql_count_to_u64(
                        effective_agent_runtime_ms,
                    )?,
                    changed_file_count: sql_count_to_u64(changed_file_count)?,
                    added_line_count: sql_count_to_u64(added_line_count)?,
                    removed_line_count: sql_count_to_u64(removed_line_count)?,
                })
            },
        )
        .collect()
}

#[cfg(test)]
/// Prepares the session and project list SQL statements against one live schema.
pub(super) fn smoke_test_sql(connection: &Connection) -> Result<()> {
    for (label, sql) in [
        (
            "project index aggregate query",
            PROJECT_INDEX_AGGREGATES_SQL,
        ),
        ("project sessions query", PROJECT_SESSIONS_SQL),
        ("session turns query", SESSION_TURNS_SQL),
    ] {
        connection
            .prepare(sql)
            .with_context(|| format!("failed to prepare {label}"))?;
    }
    Ok(())
}
