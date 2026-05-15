use std::{collections::BTreeSet, fmt::Write, path::Path, sync::OnceLock};

use anyhow::{Context, Result};
use darc_paths::{SourceKind, normalize_access_path_candidate};
use darc_store::{SessionProvenance, parse_origin_kind};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value};

use super::files::{
    TouchedSessionKey, TouchedSessionPageRequest, filter_session_summaries_by_touched_path,
    query_exact_touched_session_page,
};
use super::turns::build_token_usage;
use super::{
    ProjectIndexAggregate, ResolveSessionQueryData, ResolveSessionQueryRequest,
    ResolvedSessionMatch, SessionOriginScope, SessionSummary, SessionsQueryData,
    SessionsQueryRequest, SessionsView, TurnSummary, TurnsQueryData, TurnsQueryRequest,
    open_existing_index_database, optional_sql_count_to_u64, parse_provider, parse_session_kind,
    parse_turn_status, preview_first_line, preview_text, sql_count_to_u64,
};

const PROJECT_INDEX_AGGREGATES_SQL: &str = "
    WITH session_counts AS (
        SELECT
            project_id,
            COUNT(*) AS session_count
        FROM sessions
        WHERE origin_kind = 'local'
        GROUP BY project_id
    ),
    turn_counts AS (
        SELECT
            turns.project_id,
            COUNT(*) AS turn_count,
            MAX(turns.started_at) AS last_activity_at
        FROM turns
        JOIN sessions
            ON sessions.project_id = turns.project_id
            AND sessions.provider = turns.provider
            AND sessions.session_id = turns.session_id
        WHERE sessions.origin_kind = 'local'
        GROUP BY turns.project_id
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
    WITH latest_session_turns AS (
        SELECT
            project_id,
            provider,
            session_id,
            MAX(started_at) AS latest_turn_at
        FROM turns
        WHERE project_id = ?1
            AND (?4 IS NULL OR provider = ?4)
            AND (?5 IS NULL OR session_id = ?5)
        GROUP BY project_id, provider, session_id
    ),
    filtered_session_keys AS (
        SELECT
            s.project_id,
            s.provider,
            s.session_id,
            s.parent_session_id,
            s.session_kind,
            s.cwd,
            s.origin_kind,
            s.origin_user_id,
            users.display_name AS origin_user_name,
            users.email AS origin_user_email,
            s.origin_remote,
            s.imported_at,
            latest_session_turns.latest_turn_at
        FROM sessions AS s
        LEFT JOIN users
            ON users.user_id = s.origin_user_id
        LEFT JOIN latest_session_turns
            ON latest_session_turns.project_id = s.project_id
            AND latest_session_turns.provider = s.provider
            AND latest_session_turns.session_id = s.session_id
        WHERE s.project_id = ?1
            AND (?2 IS NULL OR julianday(latest_session_turns.latest_turn_at) >= julianday(?2))
            AND (?3 IS NULL OR julianday(latest_session_turns.latest_turn_at) < julianday(?3))
            AND (?4 IS NULL OR s.provider = ?4)
            AND (?5 IS NULL OR s.session_id = ?5)
            AND (?8 = 'all' OR s.origin_kind = ?8)
            AND (?9 IS NULL OR s.origin_user_id = ?9 OR users.user_id = ?9 OR users.email = ?9 OR users.display_name = ?9)
    ),
    paged_session_keys AS (
        SELECT *
        FROM filtered_session_keys
        ORDER BY
            latest_turn_at IS NULL ASC,
            latest_turn_at DESC,
            provider ASC,
            session_id DESC
        LIMIT ?6 OFFSET ?7
    ),
    turn_stats AS (
        SELECT
            turns.project_id,
            turns.provider,
            turns.session_id,
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
        INNER JOIN paged_session_keys
            ON paged_session_keys.project_id = turns.project_id
            AND paged_session_keys.provider = turns.provider
            AND paged_session_keys.session_id = turns.session_id
        GROUP BY turns.project_id, turns.provider, turns.session_id
    ),
    paged_sessions AS (
        SELECT
            paged_session_keys.project_id,
            paged_session_keys.provider,
            paged_session_keys.session_id,
            paged_session_keys.parent_session_id,
            paged_session_keys.session_kind,
            paged_session_keys.cwd,
            paged_session_keys.origin_kind,
            paged_session_keys.origin_user_id,
            paged_session_keys.origin_user_name,
            paged_session_keys.origin_user_email,
            paged_session_keys.origin_remote,
            paged_session_keys.imported_at,
            COALESCE(turn_stats.turn_count, 0) AS turn_count,
            paged_session_keys.latest_turn_at,
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
            latest.final_answer_text AS final_agent_message,
            COALESCE(turn_stats.aborted_turn_count, 0) AS aborted_turn_count
        FROM paged_session_keys
        LEFT JOIN turn_stats
            ON turn_stats.project_id = paged_session_keys.project_id
            AND turn_stats.provider = paged_session_keys.provider
            AND turn_stats.session_id = paged_session_keys.session_id
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
        paged_sessions.origin_kind,
        paged_sessions.origin_user_id,
        paged_sessions.origin_user_name,
        paged_sessions.origin_user_email,
        paged_sessions.origin_remote,
        paged_sessions.imported_at,
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
        paged_sessions.final_agent_message,
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
const COMPACT_SESSION_PROMPT_CHARS: usize = 500;
const RESOLVE_SESSIONS_SQL: &str = "
    SELECT DISTINCT
        project_id,
        provider,
        session_id
    FROM sessions
    WHERE (?1 IS NULL OR project_id = ?1)
        AND (?2 IS NULL OR provider = ?2)
        AND session_id LIKE ?3 || '%' COLLATE NOCASE
        AND origin_kind = 'local'
    ORDER BY
        project_id ASC,
        provider ASC,
        session_id ASC
    LIMIT ?4
";

const RESOLVE_SESSIONS_COUNT_SQL: &str = "
    SELECT COUNT(*)
    FROM (
        SELECT DISTINCT
            project_id,
            provider,
            session_id
        FROM sessions
        WHERE (?1 IS NULL OR project_id = ?1)
            AND (?2 IS NULL OR provider = ?2)
            AND session_id LIKE ?3 || '%' COLLATE NOCASE
            AND origin_kind = 'local'
    )
";

const PROJECT_SESSION_ID_SQL: &str = "
    SELECT
        session_id
    FROM sessions
    WHERE project_id = ?1
        AND (?2 IS NULL OR provider = ?2)
        AND session_id = ?3 COLLATE NOCASE
        AND (?4 = 'all' OR origin_kind = ?4)
    ORDER BY
        provider ASC,
        session_id ASC
    LIMIT 1
";

const PROJECT_SESSION_MATCHES_SQL: &str = "
    SELECT DISTINCT
        project_id,
        provider,
        session_id
    FROM sessions
    WHERE project_id = ?1
        AND (?2 IS NULL OR provider = ?2)
        AND session_id LIKE ?3 || '%' COLLATE NOCASE
        AND (?4 = 'all' OR origin_kind = ?4)
    ORDER BY
        provider ASC,
        session_id ASC
    LIMIT ?5
";

/// Collects the filters for one low-level session-summary SQL query.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionSummaryQuery<'a> {
    project_id: &'a str,
    since: Option<&'a str>,
    until: Option<&'a str>,
    provider: Option<SourceKind>,
    session_id: Option<&'a str>,
    origin_scope: SessionOriginScope,
    author: Option<&'a str>,
    project_root: Option<&'a Path>,
    limit: usize,
    offset: usize,
}

type RawSessionSummaryRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
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
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    String,
);

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
    "final_answer_text",
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
    Option<String>,
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

/// Collects the filters for one low-level session-turn summary SQL query.
#[derive(Debug, Clone, Copy)]
struct TurnSummaryQuery<'a> {
    project_id: &'a str,
    provider: SourceKind,
    session_id: &'a str,
    since: Option<&'a str>,
    until: Option<&'a str>,
    limit: usize,
    offset: usize,
}

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
    let sessions = apply_sessions_view(sessions, request.view);
    Ok(SessionsQueryData {
        project_id: request.project_id.to_owned(),
        provider: request.provider,
        since: request.since.map(str::to_owned),
        until: request.until.map(str::to_owned),
        touched_path: request.touched_path.map(str::to_owned),
        origin_scope: request.origin_scope,
        author: request.author.map(str::to_owned),
        view: request.view,
        limit: u64::try_from(request.limit).context("query limit exceeds u64 range")?,
        offset: u64::try_from(request.offset).context("query offset exceeds u64 range")?,
        has_more,
        sessions,
    })
}

/// Applies the requested session-list projection to already paginated rows.
fn apply_sessions_view(sessions: Vec<SessionSummary>, view: SessionsView) -> Vec<SessionSummary> {
    match view {
        SessionsView::Full => sessions,
        SessionsView::Compact => sessions.into_iter().map(compact_session_summary).collect(),
    }
}

/// Projects one session summary into the compact browse shape.
pub(crate) fn compact_session_summary(mut session: SessionSummary) -> SessionSummary {
    if let Some(prompt) = session.first_user_prompt.take() {
        let total_chars = count_chars_u64(&prompt);
        let (prompt, truncated) = truncate_chars(prompt, COMPACT_SESSION_PROMPT_CHARS);
        session.first_user_prompt_chars = Some(count_chars_u64(&prompt));
        session.first_user_prompt_total_chars = Some(total_chars);
        session.first_user_prompt = Some(prompt);
        session.first_user_prompt_truncated = truncated;
    }
    if let Some(message) = session.final_agent_message.take() {
        let total_chars = count_chars_u64(&message);
        let (message, truncated) = truncate_chars(message, COMPACT_SESSION_PROMPT_CHARS);
        session.final_agent_message_chars = Some(count_chars_u64(&message));
        session.final_agent_message_total_chars = Some(total_chars);
        session.final_agent_message = Some(message);
        session.final_agent_message_truncated = truncated;
    }
    session
}

/// Counts Unicode scalar values in one string for preview-size metadata.
fn count_chars_u64(value: &str) -> u64 {
    u64::try_from(value.chars().count()).unwrap_or(u64::MAX)
}

/// Truncates one string by character count without adding marker text.
fn truncate_chars(value: String, limit: usize) -> (String, bool) {
    if value.chars().count() <= limit {
        return (value, false);
    }
    (value.chars().take(limit).collect(), true)
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
            provider: request.provider,
            session_id: None,
            origin_scope: request.origin_scope,
            author: request.author,
            project_root: request.project_root,
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
    if let Some((session_keys, has_more)) = query_exact_touched_session_page(
        connection,
        TouchedSessionPageRequest {
            project_id: request.project_id,
            project_root: request.project_root,
            provider: request.provider,
            since: request.since,
            until: request.until,
            origin_scope: request.origin_scope,
            author: request.author,
            touched_path,
            limit: request.limit,
            offset: request.offset,
        },
    )? {
        let sessions = query_sessions_by_keys(connection, request, &session_keys)?;
        return Ok((sessions, has_more));
    }

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
                provider: request.provider,
                session_id: None,
                origin_scope: request.origin_scope,
                author: request.author,
                project_root: request.project_root,
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

/// Builds session summaries for already ordered touched-path session keys.
fn query_sessions_by_keys(
    connection: &Connection,
    request: SessionsQueryRequest<'_>,
    session_keys: &[TouchedSessionKey],
) -> Result<Vec<SessionSummary>> {
    if session_keys.is_empty() {
        return Ok(Vec::new());
    }
    let sql = build_selected_sessions_sql(session_keys.len());
    let mut params = Vec::with_capacity(
        session_keys
            .len()
            .checked_mul(3)
            .and_then(|value| value.checked_add(3))
            .context("session key parameter count exceeds usize range")?,
    );
    for (index, session_key) in session_keys.iter().enumerate() {
        params.push(Value::Text(
            session_key.provider.directory_name().to_owned(),
        ));
        params.push(Value::Text(session_key.session_id.clone()));
        params.push(Value::Integer(
            i64::try_from(index).context("session key index exceeds SQLite INTEGER range")?,
        ));
    }
    params.push(Value::Text(request.project_id.to_owned()));
    params.push(Value::Text(
        request.origin_scope.sql_filter_value().to_owned(),
    ));
    params.push(optional_text_value(request.author));
    query_sessions_with_params(
        connection,
        &sql,
        params_from_iter(params),
        request.project_root,
    )
}

/// Builds one keyed session-summary query preserving the supplied session order.
fn build_selected_sessions_sql(session_count: usize) -> String {
    let values = (0..session_count)
        .map(|index| {
            let base = index * 3;
            format!("(?{}, ?{}, ?{})", base + 1, base + 2, base + 3)
        })
        .collect::<Vec<_>>()
        .join(", ");
    let project_id_param = session_count * 3 + 1;
    let origin_scope_param = session_count * 3 + 2;
    let author_param = session_count * 3 + 3;
    format!(
        "
    WITH selected_session_keys(provider, session_id, sort_index) AS (
        VALUES {values}
    ),
    turn_stats AS (
        SELECT
            turns.project_id,
            turns.provider,
            turns.session_id,
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
        INNER JOIN selected_session_keys
            ON selected_session_keys.provider = turns.provider
            AND selected_session_keys.session_id = turns.session_id
        WHERE turns.project_id = ?{project_id_param}
        GROUP BY turns.project_id, turns.provider, turns.session_id
    ),
    paged_sessions AS (
        SELECT
            sessions.project_id,
            sessions.provider,
            sessions.session_id,
            sessions.parent_session_id,
            sessions.session_kind,
            sessions.cwd,
            sessions.origin_kind,
            sessions.origin_user_id,
            users.display_name AS origin_user_name,
            users.email AS origin_user_email,
            sessions.origin_remote,
            sessions.imported_at,
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
            latest.final_answer_text AS final_agent_message,
            COALESCE(turn_stats.aborted_turn_count, 0) AS aborted_turn_count,
            selected_session_keys.sort_index
        FROM selected_session_keys
        INNER JOIN sessions
            ON sessions.project_id = ?{project_id_param}
            AND sessions.provider = selected_session_keys.provider
            AND sessions.session_id = selected_session_keys.session_id
        LEFT JOIN users
            ON users.user_id = sessions.origin_user_id
        LEFT JOIN turn_stats
            ON turn_stats.project_id = sessions.project_id
            AND turn_stats.provider = sessions.provider
            AND turn_stats.session_id = sessions.session_id
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
        WHERE (?{origin_scope_param} = 'all' OR sessions.origin_kind = ?{origin_scope_param})
            AND (?{author_param} IS NULL OR sessions.origin_user_id = ?{author_param} OR users.user_id = ?{author_param} OR users.email = ?{author_param} OR users.display_name = ?{author_param})
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
        paged_sessions.origin_kind,
        paged_sessions.origin_user_id,
        paged_sessions.origin_user_name,
        paged_sessions.origin_user_email,
        paged_sessions.origin_remote,
        paged_sessions.imported_at,
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
        paged_sessions.final_agent_message,
        paged_sessions.aborted_turn_count,
        COALESCE(session_edited_files.edited_files_json, '[]')
    FROM paged_sessions
    LEFT JOIN session_edited_files
        ON session_edited_files.project_id = paged_sessions.project_id
        AND session_edited_files.provider = paged_sessions.provider
        AND session_edited_files.session_id = paged_sessions.session_id
    ORDER BY paged_sessions.sort_index ASC
"
    )
}

/// Queries the indexed turn list for one provider session.
pub fn query_project_turns(
    index_db_path: &Path,
    request: TurnsQueryRequest<'_>,
) -> Result<TurnsQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    build_turns_query(&connection, request)
}

/// Resolves one full session id or prefix into deterministic project/provider/session matches.
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
    origin_scope: SessionOriginScope,
) -> Result<Option<String>> {
    let connection = open_existing_index_database(index_db_path)?;
    query_project_session_id(&connection, project_id, provider, session_id, origin_scope)
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
    let limit =
        i64::try_from(request.limit).context("resolve-session limit exceeds SQLite range")?;
    let provider = request.provider.map(SourceKind::directory_name);
    let total =
        query_resolve_sessions_total(connection, request.project_id, provider, request.query)?;
    let mut statement = connection
        .prepare(RESOLVE_SESSIONS_SQL)
        .context("failed to prepare resolve-session query")?;
    let rows = statement
        .query_map(
            params![request.project_id, provider, request.query, limit],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .context("failed to query session resolution matches")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read session resolution rows")?;
    let matches = rows
        .into_iter()
        .map(|(project_id, provider, session_id)| -> Result<_> {
            Ok(ResolvedSessionMatch {
                project_id,
                provider: parse_provider(&provider)?,
                session_id,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let returned_count =
        u64::try_from(request.limit).context("resolve-session limit exceeds u64 range")?;
    Ok(ResolveSessionQueryData {
        query: request.query.to_owned(),
        total,
        truncated: total > returned_count,
        matches,
    })
}

/// Queries the true total match count for one session-resolution request.
fn query_resolve_sessions_total(
    connection: &Connection,
    project_id: Option<&str>,
    provider: Option<&str>,
    query: &str,
) -> Result<u64> {
    let total = connection
        .query_row(
            RESOLVE_SESSIONS_COUNT_SQL,
            params![project_id, provider, query],
            |row| row.get::<_, i64>(0),
        )
        .context("failed to query session resolution match count")?;
    sql_count_to_u64(total)
}

/// Builds one session-scoped turn-list response.
fn build_turns_query(
    connection: &Connection,
    request: TurnsQueryRequest<'_>,
) -> Result<TurnsQueryData> {
    let project_id = request.project_id;
    let page_limit = request
        .limit
        .checked_add(1)
        .context("query limit exceeds usize range")?;
    let mut turns = query_session_turn_summaries(
        connection,
        TurnSummaryQuery {
            project_id,
            provider: request.provider,
            session_id: request.session_id,
            since: request.since,
            until: request.until,
            limit: page_limit,
            offset: request.offset,
        },
    )?;
    let has_more = turns.len() > request.limit;
    turns.truncate(request.limit);
    Ok(TurnsQueryData {
        project_id: project_id.to_owned(),
        provider: request.provider,
        session_id: request.session_id.to_owned(),
        since: request.since.map(str::to_owned),
        until: request.until.map(str::to_owned),
        view: request.view,
        limit: u64::try_from(request.limit).context("query limit exceeds u64 range")?,
        offset: u64::try_from(request.offset).context("query offset exceeds u64 range")?,
        has_more,
        turns,
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
    query_sessions_with_params(
        connection,
        PROJECT_SESSIONS_SQL,
        params![
            request.project_id,
            request.since,
            request.until,
            provider,
            request.session_id,
            limit,
            offset,
            request.origin_scope.sql_filter_value(),
            request.author
        ],
        request.project_root,
    )
}

/// Queries indexed sessions with a supplied session-summary SQL statement.
fn query_sessions_with_params<P>(
    connection: &Connection,
    sql: &str,
    params: P,
    project_root: Option<&Path>,
) -> Result<Vec<SessionSummary>>
where
    P: rusqlite::Params,
{
    let mut statement = connection
        .prepare(sql)
        .context("failed to prepare indexed session query")?;
    let rows = statement
        .query_map(params, read_raw_session_summary_row)
        .context("failed to query indexed sessions")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read indexed session rows")?;
    rows.into_iter()
        .map(|row| build_session_summary(row, project_root))
        .collect()
}

/// Converts an optional string into one owned SQLite dynamic value.
fn optional_text_value(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |value| Value::Text(value.to_owned()))
}

/// Reads one raw session-summary row from SQLite before type normalization.
fn read_raw_session_summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawSessionSummaryRow> {
    Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, String>(2)?,
        row.get::<_, Option<String>>(3)?,
        row.get::<_, String>(4)?,
        row.get::<_, String>(5)?,
        row.get::<_, String>(6)?,
        row.get::<_, Option<String>>(7)?,
        row.get::<_, Option<String>>(8)?,
        row.get::<_, Option<String>>(9)?,
        row.get::<_, Option<String>>(10)?,
        row.get::<_, Option<String>>(11)?,
        row.get::<_, i64>(12)?,
        row.get::<_, Option<String>>(13)?,
        row.get::<_, Option<String>>(14)?,
        row.get::<_, Option<String>>(15)?,
        row.get::<_, Option<i64>>(16)?,
        row.get::<_, Option<i64>>(17)?,
        row.get::<_, Option<i64>>(18)?,
        row.get::<_, Option<i64>>(19)?,
        row.get::<_, Option<i64>>(20)?,
        row.get::<_, Option<i64>>(21)?,
        row.get::<_, Option<i64>>(22)?,
        row.get::<_, Option<i64>>(23)?,
        row.get::<_, i64>(24)?,
        row.get::<_, i64>(25)?,
        row.get::<_, i64>(26)?,
        row.get::<_, Option<String>>(27)?,
        row.get::<_, Option<String>>(28)?,
        row.get::<_, Option<String>>(29)?,
        row.get::<_, i64>(30)?,
        row.get::<_, String>(31)?,
    ))
}

/// Converts one raw SQLite session-summary row into the public payload shape.
fn build_session_summary(
    row: RawSessionSummaryRow,
    project_root: Option<&Path>,
) -> Result<SessionSummary> {
    let (
        project_id,
        provider,
        session_id,
        parent_session_id,
        session_kind,
        cwd,
        origin_kind,
        origin_user_id,
        origin_user_name,
        origin_user_email,
        origin_remote,
        imported_at,
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
        final_agent_message,
        aborted_turn_count,
        edited_files_json,
    ) = row;
    let first_user_prompt_chars = first_user_prompt.as_deref().map(count_chars_u64);
    let final_agent_message_chars = final_agent_message.as_deref().map(count_chars_u64);
    let origin_kind = parse_origin_kind(&origin_kind)?;
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
        effective_agent_runtime_ms: optional_sql_count_to_u64(effective_agent_runtime_ms)?,
        changed_file_count: sql_count_to_u64(changed_file_count)?,
        added_line_count: sql_count_to_u64(added_line_count)?,
        removed_line_count: sql_count_to_u64(removed_line_count)?,
        first_turn_at,
        first_user_prompt,
        first_user_prompt_truncated: false,
        first_user_prompt_chars,
        first_user_prompt_total_chars: first_user_prompt_chars,
        final_agent_message,
        final_agent_message_truncated: false,
        final_agent_message_chars,
        final_agent_message_total_chars: final_agent_message_chars,
        provenance: SessionProvenance {
            origin_kind,
            user_id: origin_user_id,
            user_name: origin_user_name,
            user_email: origin_user_email,
            origin_remote,
            imported_at,
        },
        aborted_turn_count: sql_count_to_u64(aborted_turn_count)?,
        edited_files: normalize_edited_files(
            parse_edited_files_json(&edited_files_json)?,
            project_root,
        ),
    })
}

/// Queries one indexed session summary for one configured project session.
pub(crate) fn query_session_summary(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    project_root: Option<&Path>,
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
            origin_scope: SessionOriginScope::All,
            author: None,
            project_root,
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
    origin_scope: SessionOriginScope,
) -> Result<Option<String>> {
    let provider = provider.map(SourceKind::directory_name);
    let mut statement = connection
        .prepare(PROJECT_SESSION_ID_SQL)
        .context("failed to prepare project session id query")?;
    let session_id = statement
        .query_row(
            params![
                project_id,
                provider,
                session_id,
                origin_scope.sql_filter_value()
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("failed to query project session id")?;
    Ok(session_id)
}

/// Queries project-scoped session matches for provider and prefix inference.
pub fn lookup_project_session_matches(
    index_db_path: &Path,
    project_id: &str,
    provider: Option<SourceKind>,
    session_id: &str,
    origin_scope: SessionOriginScope,
    limit: usize,
) -> Result<Vec<ResolvedSessionMatch>> {
    let connection = open_existing_index_database(index_db_path)?;
    query_project_session_matches(
        &connection,
        project_id,
        provider,
        session_id,
        origin_scope,
        limit,
    )
}

/// Queries project-scoped session matches from one open SQLite connection.
fn query_project_session_matches(
    connection: &Connection,
    project_id: &str,
    provider: Option<SourceKind>,
    session_id: &str,
    origin_scope: SessionOriginScope,
    limit: usize,
) -> Result<Vec<ResolvedSessionMatch>> {
    let provider = provider.map(SourceKind::directory_name);
    let limit = i64::try_from(limit).context("project session match limit exceeds SQLite range")?;
    let mut statement = connection
        .prepare(PROJECT_SESSION_MATCHES_SQL)
        .context("failed to prepare project session matches query")?;
    let rows = statement
        .query_map(
            params![
                project_id,
                provider,
                session_id,
                origin_scope.sql_filter_value(),
                limit
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .context("failed to query project session matches")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read project session match rows")?;
    rows.into_iter()
        .map(|(project_id, provider, session_id)| -> Result<_> {
            Ok(ResolvedSessionMatch {
                project_id,
                provider: parse_provider(&provider)?,
                session_id,
            })
        })
        .collect()
}

/// Parses one JSON array of edited session file paths from SQLite aggregation output.
fn parse_edited_files_json(value: &str) -> Result<Vec<String>> {
    serde_json::from_str(value)
        .with_context(|| format!("failed to parse session edited-files JSON `{value}`"))
}

/// Normalizes edited-file display paths and deduplicates absolute/project-relative twins.
fn normalize_edited_files(paths: Vec<String>, project_root: Option<&Path>) -> Vec<String> {
    paths
        .into_iter()
        .filter_map(|path| normalize_edited_file_path(&path, project_root))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Converts one in-project absolute display path to a project-relative path.
fn normalize_edited_file_path(path: &str, project_root: Option<&Path>) -> Option<String> {
    let trimmed = normalize_access_path_candidate(path)?;
    let Some(project_root) = project_root else {
        return Some(trimmed);
    };
    let candidate = Path::new(&trimmed);
    if !candidate.is_absolute() {
        return Some(trimmed);
    }
    Some(
        candidate
            .strip_prefix(project_root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(|relative| relative.to_string_lossy().into_owned())
            .unwrap_or(trimmed),
    )
}

/// Queries the indexed turns for one provider session.
fn query_session_turn_summaries(
    connection: &Connection,
    request: TurnSummaryQuery<'_>,
) -> Result<Vec<TurnSummary>> {
    let limit = i64::try_from(request.limit).context("query limit exceeds SQLite INTEGER range")?;
    let offset =
        i64::try_from(request.offset).context("query offset exceeds SQLite INTEGER range")?;
    let mut statement = connection
        .prepare(session_turns_sql())
        .context("failed to prepare indexed turn query")?;
    let rows = statement
        .query_map(
            (
                request.project_id,
                request.provider.directory_name(),
                request.session_id,
                request.since,
                request.until,
                limit,
                offset,
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
    LIMIT ?6 OFFSET ?7
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
        row.get::<_, Option<String>>(9)?,
        row.get::<_, i64>(10)?,
        row.get::<_, i64>(11)?,
        row.get::<_, i64>(12)?,
        row.get::<_, Option<String>>(13)?,
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
    ))
}

/// Converts one raw SQLite turn-summary row into the public turn-summary payload.
fn build_turn_summary(row: RawTurnSummaryRow) -> Result<TurnSummary> {
    let user_prompt_preview = preview_text(&row.8);
    let oneline_user_prompt_preview = preview_first_line(&row.8);
    let agent_answer_preview = row.9.as_deref().map(preview_text);
    let oneline_agent_answer_preview = row.9.as_deref().map(preview_first_line);
    Ok(TurnSummary {
        project_id: row.0,
        provider: parse_provider(&row.1)?,
        session_id: row.2,
        turn_ordinal: sql_count_to_u64(row.3)?,
        turn_id: row.4,
        started_at: row.5,
        completed_at: row.6,
        status: parse_turn_status(&row.7)?,
        user_prompt_preview: user_prompt_preview.text,
        user_prompt_preview_chars: user_prompt_preview.chars,
        user_prompt_total_chars: user_prompt_preview.total_chars,
        oneline_user_prompt_preview: oneline_user_prompt_preview.text,
        oneline_user_prompt_preview_chars: oneline_user_prompt_preview.chars,
        oneline_user_prompt_total_chars: oneline_user_prompt_preview.total_chars,
        oneline_agent_answer_preview: oneline_agent_answer_preview
            .as_ref()
            .map(|preview| preview.text.clone()),
        oneline_agent_answer_preview_chars: oneline_agent_answer_preview
            .as_ref()
            .map(|preview| preview.chars),
        oneline_agent_answer_total_chars: oneline_agent_answer_preview
            .as_ref()
            .map(|preview| preview.total_chars),
        agent_answer_preview: agent_answer_preview
            .as_ref()
            .map(|preview| preview.text.clone()),
        agent_answer_preview_chars: agent_answer_preview.as_ref().map(|preview| preview.chars),
        agent_answer_total_chars: agent_answer_preview
            .as_ref()
            .map(|preview| preview.total_chars),
        has_final_answer: row.10 != 0,
        step_count: sql_count_to_u64(row.11)?,
        tool_call_count: sql_count_to_u64(row.12)?,
        primary_model: row.13,
        token_usage: build_token_usage(row.15, row.16, row.17, row.18, row.19, row.20, row.14)?,
        total_token_count: optional_sql_count_to_u64(row.14)?,
        effective_agent_runtime_ms: optional_sql_count_to_u64(row.21)?,
        changed_file_count: sql_count_to_u64(row.22)?,
        added_line_count: sql_count_to_u64(row.23)?,
        removed_line_count: sql_count_to_u64(row.24)?,
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
        ("resolve sessions count query", RESOLVE_SESSIONS_COUNT_SQL),
        ("project session id query", PROJECT_SESSION_ID_SQL),
        ("project session matches query", PROJECT_SESSION_MATCHES_SQL),
        ("session turns query", session_turns_sql()),
    ] {
        connection
            .prepare(sql)
            .with_context(|| format!("failed to prepare {label}"))?;
    }
    Ok(())
}
