use std::{fmt::Write, path::Path, sync::OnceLock};

use anyhow::{Context, Result};
use darc_paths::SourceKind;
use rusqlite::{Connection, OptionalExtension, params};

use super::files::filter_session_summaries_by_touched_path;
use super::turns::build_token_usage;
use super::{
    ProjectIndexAggregate, ResolveSessionQueryData, ResolveSessionQueryRequest,
    ResolvedSessionMatch, SessionSummary, SessionsQueryData, SessionsQueryRequest, TurnSummary,
    TurnsQueryData, TurnsQueryRequest, open_existing_index_database, optional_sql_count_to_u64,
    parse_provider, parse_session_kind, parse_turn_status, preview_first_line, preview_text,
    sql_count_to_u64,
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
            MIN(turn_ordinal) AS first_turn_ordinal,
            MAX(turn_ordinal) AS latest_turn_ordinal,
            MAX(started_at) AS latest_turn_at,
            SUM(CASE WHEN status = 'aborted' THEN 1 ELSE 0 END) AS aborted_turn_count,
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
            AND (?4 IS NULL OR provider = ?4)
            AND (?5 IS NULL OR session_id = ?5)
        GROUP BY project_id, provider, session_id
    ),
    filtered_sessions AS (
        SELECT
            s.project_id,
            s.provider,
            s.session_id,
            s.parent_session_id,
            s.session_kind,
            s.cwd,
            COALESCE(turn_stats.turn_count, 0) AS turn_count,
            turn_stats.latest_turn_at,
            latest.status AS latest_status,
            latest.primary_model,
            turn_stats.total_token_count,
            turn_stats.provider_total_token_count,
            turn_stats.input_uncached_token_count,
            turn_stats.cache_read_token_count,
            turn_stats.cache_write_token_count,
            turn_stats.output_token_count,
            turn_stats.reasoning_token_count,
            turn_stats.effective_agent_runtime_ms,
            COALESCE(turn_stats.changed_file_count, 0) AS changed_file_count,
            COALESCE(turn_stats.added_line_count, 0) AS added_line_count,
            COALESCE(turn_stats.removed_line_count, 0) AS removed_line_count,
            first_turn.started_at AS first_turn_at,
            first_turn.user_message AS first_user_prompt,
            COALESCE(turn_stats.aborted_turn_count, 0) AS aborted_turn_count
        FROM sessions AS s
        LEFT JOIN turn_stats
            ON turn_stats.project_id = s.project_id
            AND turn_stats.provider = s.provider
            AND turn_stats.session_id = s.session_id
        LEFT JOIN turns AS first_turn
            ON first_turn.project_id = turn_stats.project_id
            AND first_turn.provider = turn_stats.provider
            AND first_turn.session_id = turn_stats.session_id
            AND first_turn.turn_ordinal = turn_stats.first_turn_ordinal
        LEFT JOIN turns AS latest
            ON latest.project_id = turn_stats.project_id
            AND latest.provider = turn_stats.provider
            AND latest.session_id = turn_stats.session_id
            AND latest.turn_ordinal = turn_stats.latest_turn_ordinal
        WHERE s.project_id = ?1
            AND (?2 IS NULL OR julianday(turn_stats.latest_turn_at) >= julianday(?2))
            AND (?3 IS NULL OR julianday(turn_stats.latest_turn_at) < julianday(?3))
            AND (?4 IS NULL OR s.provider = ?4)
            AND (?5 IS NULL OR s.session_id = ?5)
    ),
    paged_sessions AS (
        SELECT *
        FROM filtered_sessions
        ORDER BY
            latest_turn_at IS NULL ASC,
            latest_turn_at DESC,
            provider ASC,
            session_id DESC
        LIMIT ?6 OFFSET ?7
    ),
    session_edited_files AS (
        SELECT
            project_id,
            provider,
            session_id,
            json_group_array(display_path) AS edited_files_json
        FROM (
            SELECT DISTINCT
                file_accesses.project_id,
                file_accesses.provider,
                file_accesses.session_id,
                TRIM(COALESCE(file_accesses.repo_relative_path, file_accesses.path)) AS display_path
            FROM file_accesses
            INNER JOIN paged_sessions
                ON paged_sessions.project_id = file_accesses.project_id
                AND paged_sessions.provider = file_accesses.provider
                AND paged_sessions.session_id = file_accesses.session_id
            WHERE file_accesses.access_type IN ('edit', 'write')
                AND NULLIF(TRIM(COALESCE(file_accesses.repo_relative_path, file_accesses.path)), '') IS NOT NULL
            ORDER BY
                file_accesses.project_id ASC,
                file_accesses.provider ASC,
                file_accesses.session_id ASC,
                display_path ASC
        )
        GROUP BY project_id, provider, session_id
    )
    SELECT
        paged_sessions.project_id,
        paged_sessions.provider,
        paged_sessions.session_id,
        paged_sessions.parent_session_id,
        paged_sessions.session_kind,
        paged_sessions.cwd,
        paged_sessions.turn_count,
        paged_sessions.latest_turn_at,
        paged_sessions.latest_status,
        paged_sessions.primary_model,
        paged_sessions.total_token_count,
        paged_sessions.provider_total_token_count,
        paged_sessions.input_uncached_token_count,
        paged_sessions.cache_read_token_count,
        paged_sessions.cache_write_token_count,
        paged_sessions.output_token_count,
        paged_sessions.reasoning_token_count,
        paged_sessions.effective_agent_runtime_ms,
        paged_sessions.changed_file_count,
        paged_sessions.added_line_count,
        paged_sessions.removed_line_count,
        paged_sessions.first_turn_at,
        paged_sessions.first_user_prompt,
        paged_sessions.aborted_turn_count,
        COALESCE(session_edited_files.edited_files_json, '[]')
    FROM paged_sessions
    LEFT JOIN session_edited_files
        ON session_edited_files.project_id = paged_sessions.project_id
        AND session_edited_files.provider = paged_sessions.provider
        AND session_edited_files.session_id = paged_sessions.session_id
    ORDER BY
        paged_sessions.latest_turn_at IS NULL ASC,
        paged_sessions.latest_turn_at DESC,
        paged_sessions.provider ASC,
        paged_sessions.session_id DESC
";

const TOUCHED_SESSION_CANDIDATE_BATCH_ROWS: usize = if cfg!(test) { 2 } else { 250 };

const RESOLVE_SESSIONS_SQL: &str = "
    SELECT DISTINCT
        provider,
        session_id
    FROM sessions
    WHERE (?1 IS NULL OR provider = ?1)
        AND session_id LIKE ?2 || '%' COLLATE NOCASE
    ORDER BY
        provider ASC,
        session_id ASC
    LIMIT ?3
";

const PROJECT_SESSION_ID_SQL: &str = "
    SELECT
        session_id
    FROM sessions
    WHERE project_id = ?1
        AND (?2 IS NULL OR provider = ?2)
        AND session_id = ?3 COLLATE NOCASE
    ORDER BY
        provider ASC,
        session_id ASC
    LIMIT 1
";

/// Collects the filters for one low-level session-summary SQL query.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionSummaryQuery<'a> {
    project_id: &'a str,
    since: Option<&'a str>,
    until: Option<&'a str>,
    provider: Option<SourceKind>,
    session_id: Option<&'a str>,
    limit: usize,
    offset: usize,
}

const TURN_SUMMARY_COLUMNS: &[&str] = &[
    "project_id",
    "provider",
    "session_id",
    "turn_ordinal",
    "turn_id",
    "started_at",
    "completed_at",
    "status",
    "user_message",
    "has_final_answer",
    "step_count",
    "tool_call_count",
    "primary_model",
    "total_token_count",
    "provider_total_token_count",
    "input_uncached_token_count",
    "cache_read_token_count",
    "cache_write_token_count",
    "output_token_count",
    "reasoning_token_count",
    "effective_agent_runtime_ms",
    "changed_file_count",
    "added_line_count",
    "removed_line_count",
];

type RawTurnSummaryRow = (
    String,
    String,
    String,
    i64,
    Option<String>,
    String,
    Option<String>,
    String,
    String,
    i64,
    i64,
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    i64,
    i64,
    i64,
);

/// Returns the shared session-turn summary SQL used by every session-scoped list query.
fn session_turns_sql() -> &'static str {
    static SQL: OnceLock<String> = OnceLock::new();
    SQL.get_or_init(build_session_turns_sql).as_str()
}

/// Queries the indexed project aggregates for one workspace database.
pub fn list_project_index_aggregates(index_db_path: &Path) -> Result<Vec<ProjectIndexAggregate>> {
    let connection = open_existing_index_database(index_db_path)?;
    query_project_index_aggregates(&connection)
}

/// Queries the indexed session list for one project.
pub fn query_project_sessions(
    index_db_path: &Path,
    request: SessionsQueryRequest<'_>,
) -> Result<SessionsQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    let (sessions, has_more) = if let Some(touched_path) = request.touched_path {
        query_touched_path_session_page(&connection, request, touched_path)?
    } else {
        query_session_page(&connection, request)?
    };
    Ok(SessionsQueryData {
        project_id: request.project_id.to_owned(),
        since: request.since.map(str::to_owned),
        until: request.until.map(str::to_owned),
        touched_path: request.touched_path.map(str::to_owned),
        limit: u64::try_from(request.limit).context("query limit exceeds u64 range")?,
        offset: u64::try_from(request.offset).context("query offset exceeds u64 range")?,
        has_more,
        sessions,
    })
}

/// Queries one session page without touched-path post-filtering.
fn query_session_page(
    connection: &Connection,
    request: SessionsQueryRequest<'_>,
) -> Result<(Vec<SessionSummary>, bool)> {
    let page_limit = request
        .limit
        .checked_add(1)
        .context("query limit exceeds usize range")?;
    let mut sessions = query_sessions(
        connection,
        SessionSummaryQuery {
            project_id: request.project_id,
            since: request.since,
            until: request.until,
            provider: None,
            session_id: None,
            limit: page_limit,
            offset: request.offset,
        },
    )?;
    let has_more = sessions.len() > request.limit;
    sessions.truncate(request.limit);
    Ok((sessions, has_more))
}

/// Queries one touched-path session page by filtering bounded session candidate batches.
fn query_touched_path_session_page(
    connection: &Connection,
    request: SessionsQueryRequest<'_>,
    touched_path: &str,
) -> Result<(Vec<SessionSummary>, bool)> {
    let desired_match_count = request
        .offset
        .checked_add(request.limit)
        .and_then(|value| value.checked_add(1))
        .context("query pagination exceeds usize range")?;
    let mut matching_sessions = Vec::<SessionSummary>::new();
    let mut candidate_offset = 0usize;

    loop {
        let candidates = query_sessions(
            connection,
            SessionSummaryQuery {
                project_id: request.project_id,
                since: request.since,
                until: request.until,
                provider: None,
                session_id: None,
                limit: TOUCHED_SESSION_CANDIDATE_BATCH_ROWS,
                offset: candidate_offset,
            },
        )?;
        let candidate_count = candidates.len();
        if candidate_count == 0 {
            break;
        }

        matching_sessions.extend(filter_session_summaries_by_touched_path(
            connection,
            request.project_id,
            request.project_root,
            candidates,
            touched_path,
        )?);
        if matching_sessions.len() >= desired_match_count {
            break;
        }
        if candidate_count < TOUCHED_SESSION_CANDIDATE_BATCH_ROWS {
            break;
        }
        candidate_offset = candidate_offset
            .checked_add(candidate_count)
            .context("query candidate offset exceeds usize range")?;
    }

    let page_end = request
        .offset
        .checked_add(request.limit)
        .context("query pagination exceeds usize range")?;
    let has_more = matching_sessions.len() > page_end;
    let sessions = matching_sessions
        .into_iter()
        .skip(request.offset)
        .take(request.limit)
        .collect();
    Ok((sessions, has_more))
}

/// Queries the indexed turn list for one provider session.
pub fn query_project_turns(
    index_db_path: &Path,
    request: TurnsQueryRequest<'_>,
) -> Result<TurnsQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    build_turns_query(&connection, request)
}

/// Resolves one full session id or prefix into deterministic provider/session matches.
pub fn query_resolve_sessions(
    index_db_path: &Path,
    request: ResolveSessionQueryRequest<'_>,
) -> Result<ResolveSessionQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    build_resolve_sessions_query(&connection, request)
}

/// Looks up one canonical project-scoped session id using exact case-insensitive matching.
pub fn lookup_project_session_id(
    index_db_path: &Path,
    project_id: &str,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<Option<String>> {
    let connection = open_existing_index_database(index_db_path)?;
    query_project_session_id(&connection, project_id, provider, session_id)
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

/// Builds one session-resolution response from the indexed session table.
fn build_resolve_sessions_query(
    connection: &Connection,
    request: ResolveSessionQueryRequest<'_>,
) -> Result<ResolveSessionQueryData> {
    let limit = request
        .limit
        .checked_add(1)
        .context("resolve-session limit exceeds usize range")?;
    let limit = i64::try_from(limit).context("resolve-session limit exceeds SQLite range")?;
    let provider = request.provider.map(SourceKind::directory_name);
    let mut statement = connection
        .prepare(RESOLVE_SESSIONS_SQL)
        .context("failed to prepare resolve-session query")?;
    let rows = statement
        .query_map(params![provider, request.query, limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("failed to query session resolution matches")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read session resolution rows")?;
    let truncated = rows.len() > request.limit;
    let matches = rows
        .into_iter()
        .take(request.limit)
        .map(|(provider, session_id)| -> Result<_> {
            Ok(ResolvedSessionMatch {
                provider: parse_provider(&provider)?,
                session_id,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ResolveSessionQueryData {
        query: request.query.to_owned(),
        total: u64::try_from(matches.len()).context("resolve-session match count exceeds u64")?,
        truncated,
        matches,
    })
}

/// Builds one session-scoped turn-list response.
fn build_turns_query(
    connection: &Connection,
    request: TurnsQueryRequest<'_>,
) -> Result<TurnsQueryData> {
    let project_id = request.project_id;
    Ok(TurnsQueryData {
        project_id: project_id.to_owned(),
        provider: request.provider,
        session_id: request.session_id.to_owned(),
        since: request.since.map(str::to_owned),
        until: request.until.map(str::to_owned),
        view: request.view,
        turns: query_session_turn_summaries(
            connection,
            project_id,
            request.provider,
            request.session_id,
            request.since,
            request.until,
        )?,
    })
}

/// Queries the indexed sessions for one configured project.
pub(crate) fn query_sessions(
    connection: &Connection,
    request: SessionSummaryQuery<'_>,
) -> Result<Vec<SessionSummary>> {
    let provider = request.provider.map(SourceKind::directory_name);
    let limit = i64::try_from(request.limit).context("query limit exceeds SQLite INTEGER range")?;
    let offset =
        i64::try_from(request.offset).context("query offset exceeds SQLite INTEGER range")?;
    let mut statement = connection
        .prepare(PROJECT_SESSIONS_SQL)
        .context("failed to prepare indexed session query")?;
    let rows = statement
        .query_map(
            params![
                request.project_id,
                request.since,
                request.until,
                provider,
                request.session_id,
                limit,
                offset
            ],
            |row| {
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
                    row.get::<_, Option<String>>(21)?,
                    row.get::<_, Option<String>>(22)?,
                    row.get::<_, i64>(23)?,
                    row.get::<_, String>(24)?,
                ))
            },
        )
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
                first_turn_at,
                first_user_prompt,
                aborted_turn_count,
                edited_files_json,
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
                    first_turn_at,
                    first_user_prompt,
                    aborted_turn_count: sql_count_to_u64(aborted_turn_count)?,
                    edited_files: parse_edited_files_json(&edited_files_json)?,
                })
            },
        )
        .collect()
}

/// Queries one indexed session summary for one configured project session.
pub(crate) fn query_session_summary(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
) -> Result<SessionSummary> {
    let session_ref = format!("{}:{session_id}", provider.directory_name());
    query_sessions(
        connection,
        SessionSummaryQuery {
            project_id,
            since: None,
            until: None,
            provider: Some(provider),
            session_id: Some(session_id),
            limit: 1,
            offset: 0,
        },
    )?
    .into_iter()
    .next()
    .with_context(|| format!("session `{session_ref}` was not found in project `{project_id}`"))
}

/// Queries one canonical project-scoped session id using exact case-insensitive matching.
pub(crate) fn query_project_session_id(
    connection: &Connection,
    project_id: &str,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<Option<String>> {
    let provider = provider.map(SourceKind::directory_name);
    let mut statement = connection
        .prepare(PROJECT_SESSION_ID_SQL)
        .context("failed to prepare project session id query")?;
    let session_id = statement
        .query_row(params![project_id, provider, session_id], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .context("failed to query project session id")?;
    Ok(session_id)
}

/// Parses one JSON array of edited session file paths from SQLite aggregation output.
fn parse_edited_files_json(value: &str) -> Result<Vec<String>> {
    serde_json::from_str(value)
        .with_context(|| format!("failed to parse session edited-files JSON `{value}`"))
}

/// Queries the indexed turns for one provider session.
fn query_session_turn_summaries(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<TurnSummary>> {
    let mut statement = connection
        .prepare(session_turns_sql())
        .context("failed to prepare indexed turn query")?;
    let rows = statement
        .query_map(
            (
                project_id,
                provider.directory_name(),
                session_id,
                since,
                until,
            ),
            read_turn_summary_row,
        )
        .context("failed to query indexed turns")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read indexed turn rows")?;
    rows.into_iter().map(build_turn_summary).collect()
}

/// Builds the shared turn-summary select list for one optional table alias.
fn build_turn_summary_select_list(table_alias: &str) -> String {
    let mut sql = String::new();
    for (index, column) in TURN_SUMMARY_COLUMNS.iter().enumerate() {
        if index > 0 {
            sql.push_str(",\n");
        }
        sql.push_str("        ");
        if table_alias.is_empty() {
            sql.push_str(column);
        } else {
            write!(&mut sql, "{table_alias}.{column}")
                .expect("formatting turn-summary column should not fail");
        }
    }
    sql
}

/// Builds the shared session-scoped turn-summary query SQL.
fn build_session_turns_sql() -> String {
    format!(
        "
    SELECT
{select_list}
    FROM turns
    WHERE project_id = ?1
        AND provider = ?2
        AND session_id = ?3
        AND (?4 IS NULL OR julianday(started_at) >= julianday(?4))
        AND (?5 IS NULL OR julianday(started_at) < julianday(?5))
    ORDER BY turn_ordinal ASC
",
        select_list = build_turn_summary_select_list(""),
    )
}

/// Reads one raw turn-summary row from SQLite before type normalization.
fn read_turn_summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawTurnSummaryRow> {
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
        row.get::<_, i64>(11)?,
        row.get::<_, Option<String>>(12)?,
        row.get::<_, Option<i64>>(13)?,
        row.get::<_, Option<i64>>(14)?,
        row.get::<_, Option<i64>>(15)?,
        row.get::<_, Option<i64>>(16)?,
        row.get::<_, Option<i64>>(17)?,
        row.get::<_, Option<i64>>(18)?,
        row.get::<_, Option<i64>>(19)?,
        row.get::<_, Option<i64>>(20)?,
        row.get::<_, i64>(21)?,
        row.get::<_, i64>(22)?,
        row.get::<_, i64>(23)?,
    ))
}

/// Converts one raw SQLite turn-summary row into the public turn-summary payload.
fn build_turn_summary(row: RawTurnSummaryRow) -> Result<TurnSummary> {
    Ok(TurnSummary {
        project_id: row.0,
        provider: parse_provider(&row.1)?,
        session_id: row.2,
        turn_ordinal: sql_count_to_u64(row.3)?,
        turn_id: row.4,
        started_at: row.5,
        completed_at: row.6,
        status: parse_turn_status(&row.7)?,
        user_preview: preview_text(&row.8),
        oneline_user_preview: preview_first_line(&row.8),
        has_final_answer: row.9 != 0,
        step_count: sql_count_to_u64(row.10)?,
        tool_call_count: sql_count_to_u64(row.11)?,
        primary_model: row.12,
        token_usage: build_token_usage(row.14, row.15, row.16, row.17, row.18, row.19, row.13)?,
        total_token_count: optional_sql_count_to_u64(row.13)?,
        effective_agent_runtime_ms: optional_sql_count_to_u64(row.20)?,
        changed_file_count: sql_count_to_u64(row.21)?,
        added_line_count: sql_count_to_u64(row.22)?,
        removed_line_count: sql_count_to_u64(row.23)?,
    })
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
        ("resolve sessions query", RESOLVE_SESSIONS_SQL),
        ("project session id query", PROJECT_SESSION_ID_SQL),
        ("session turns query", session_turns_sql()),
    ] {
        connection
            .prepare(sql)
            .with_context(|| format!("failed to prepare {label}"))?;
    }
    Ok(())
}
