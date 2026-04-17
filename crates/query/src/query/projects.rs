use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt::Write,
    path::Path,
    sync::OnceLock,
};

use anyhow::{Context, Result, bail};
use darc_paths::SourceKind;
use glob::Pattern;
use rusqlite::{Connection, params, params_from_iter, types::Value};

use super::files::{
    filter_session_summaries_by_touched_path, glob_match_options, normalize_query_path_pattern,
    path_matches_glob,
};
use super::search::build_fts_phrase_query;
use super::turns::build_token_usage;
use super::{
    ProjectIndexAggregate, SessionSummary, SessionsQueryData, TurnMatchKind, TurnMatchesQueryData,
    TurnMatchesQueryRequest, TurnSearchRole, TurnSummary, TurnsQueryData, TurnsQueryRequest,
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
            INNER JOIN filtered_sessions
                ON filtered_sessions.project_id = file_accesses.project_id
                AND filtered_sessions.provider = file_accesses.provider
                AND filtered_sessions.session_id = file_accesses.session_id
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
        filtered_sessions.project_id,
        filtered_sessions.provider,
        filtered_sessions.session_id,
        filtered_sessions.parent_session_id,
        filtered_sessions.session_kind,
        filtered_sessions.cwd,
        filtered_sessions.turn_count,
        filtered_sessions.latest_turn_at,
        filtered_sessions.latest_status,
        filtered_sessions.primary_model,
        filtered_sessions.total_token_count,
        filtered_sessions.provider_total_token_count,
        filtered_sessions.input_uncached_token_count,
        filtered_sessions.cache_read_token_count,
        filtered_sessions.cache_write_token_count,
        filtered_sessions.output_token_count,
        filtered_sessions.reasoning_token_count,
        filtered_sessions.effective_agent_runtime_ms,
        filtered_sessions.changed_file_count,
        filtered_sessions.added_line_count,
        filtered_sessions.removed_line_count,
        filtered_sessions.first_turn_at,
        filtered_sessions.first_user_prompt,
        filtered_sessions.aborted_turn_count,
        COALESCE(session_edited_files.edited_files_json, '[]')
    FROM filtered_sessions
    LEFT JOIN session_edited_files
        ON session_edited_files.project_id = filtered_sessions.project_id
        AND session_edited_files.provider = filtered_sessions.provider
        AND session_edited_files.session_id = filtered_sessions.session_id
    ORDER BY
        filtered_sessions.latest_turn_at IS NULL ASC,
        filtered_sessions.latest_turn_at DESC,
        filtered_sessions.provider ASC,
        filtered_sessions.session_id DESC
";

const TURN_MATCHES_SQL: &str = "
    SELECT
        turn_search.provider,
        turn_search.session_id,
        turn_search.turn_ordinal,
        NULLIF(snippet(turn_search_fts, -1, '[[', ']]', '…', 16), '')
    FROM turn_search_fts
    INNER JOIN turn_search
        ON turn_search.rowid = turn_search_fts.rowid
    INNER JOIN turns
        ON turns.project_id = turn_search.project_id
        AND turns.provider = turn_search.provider
        AND turns.session_id = turn_search.session_id
        AND turns.turn_ordinal = turn_search.turn_ordinal
    WHERE turn_search.project_id = ?1
        AND (?2 IS NULL OR turn_search.provider = ?2)
        AND (?3 IS NULL OR turn_search.session_id = ?3)
        AND (?4 IS NULL OR julianday(turns.started_at) >= julianday(?4))
        AND (?5 IS NULL OR julianday(turns.started_at) < julianday(?5))
        AND turn_search_fts MATCH ?6
    ORDER BY
        bm25(turn_search_fts) ASC,
        turns.started_at DESC,
        turn_search.provider ASC,
        turn_search.session_id ASC,
        turn_search.turn_ordinal ASC
";

const MATCH_SNIPPET_START: &str = "[[";
const MATCH_SNIPPET_END: &str = "]]";
const MAX_TURN_KEYS_PER_QUERY: usize = 250;
const MAX_TURN_MATCH_CONTEXT: usize = 50;
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

/// Stores one stable turn identity used while building match and context windows.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct TurnKey {
    provider: SourceKind,
    session_id: String,
    turn_ordinal: u64,
}

/// Stores one FTS-backed turn match before surrounding context is expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnMatchAnchor {
    key: TurnKey,
    match_snippet: Option<String>,
}

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
    project_id: &str,
    project_root: Option<&Path>,
    since: Option<&str>,
    until: Option<&str>,
    touched_path: Option<&str>,
) -> Result<SessionsQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    let sessions = query_sessions(&connection, project_id, since, until)?;
    let sessions = if let Some(touched_path) = touched_path {
        filter_session_summaries_by_touched_path(
            &connection,
            project_id,
            project_root,
            sessions,
            touched_path,
        )?
    } else {
        sessions
    };
    Ok(SessionsQueryData {
        project_id: project_id.to_owned(),
        since: since.map(str::to_owned),
        until: until.map(str::to_owned),
        touched_path: touched_path.map(str::to_owned),
        sessions,
    })
}

/// Queries the indexed turn list for one provider session.
pub fn query_project_turns(
    index_db_path: &Path,
    request: TurnsQueryRequest<'_>,
) -> Result<TurnsQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    build_turns_query(&connection, request)
}

/// Queries the grep-scoped turn-match list for one project.
pub fn query_project_turn_matches(
    index_db_path: &Path,
    request: TurnMatchesQueryRequest<'_>,
) -> Result<TurnMatchesQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    build_turn_matches_query(&connection, request)
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

/// Builds one grep-scoped turn-match response.
fn build_turn_matches_query(
    connection: &Connection,
    request: TurnMatchesQueryRequest<'_>,
) -> Result<TurnMatchesQueryData> {
    let grep = request.grep.trim();
    if grep.is_empty() {
        bail!("turn grep query must not be empty");
    }
    let context = validate_turn_match_context(request.context)?;
    let turns = query_grep_turns(connection, request, grep, context)?;
    Ok(TurnMatchesQueryData {
        project_id: request.project_id.to_owned(),
        provider: request.provider,
        session_id: request.session_id.map(str::to_owned),
        grep: grep.to_owned(),
        role: request.role,
        context,
        since: request.since.map(str::to_owned),
        until: request.until.map(str::to_owned),
        touched_path: request.touched_path.map(str::to_owned),
        turns,
    })
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
                row.get::<_, Option<String>>(21)?,
                row.get::<_, Option<String>>(22)?,
                row.get::<_, i64>(23)?,
                row.get::<_, String>(24)?,
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

/// Queries grep-matched turns plus any requested same-session context turns.
fn query_grep_turns(
    connection: &Connection,
    request: TurnMatchesQueryRequest<'_>,
    grep: &str,
    context_radius: u64,
) -> Result<Vec<TurnSummary>> {
    let mut matches = query_turn_match_anchors(connection, request, grep)?;
    if let Some(touched_path) = request.touched_path {
        matches = filter_turn_match_anchors_by_path(
            connection,
            request.project_id,
            request.project_root,
            matches,
            touched_path,
        )?;
    }
    if matches.is_empty() {
        return Ok(Vec::new());
    }

    let mut requested_turns_by_session = BTreeMap::<(SourceKind, String), BTreeSet<u64>>::new();
    let mut match_keys = HashSet::<TurnKey>::new();
    let mut match_snippets = BTreeMap::<TurnKey, String>::new();
    let mut session_order = BTreeMap::<(SourceKind, String), usize>::new();
    let mut next_session_order = 0_usize;
    for matched_turn in matches {
        let session_key = (
            matched_turn.key.provider,
            matched_turn.key.session_id.clone(),
        );
        if !session_order.contains_key(&session_key) {
            session_order.insert(session_key.clone(), next_session_order);
            next_session_order = next_session_order.saturating_add(1);
        }
        let ordinals = requested_turns_by_session.entry(session_key).or_default();
        let first_ordinal = matched_turn.key.turn_ordinal.saturating_sub(context_radius);
        let last_ordinal = matched_turn.key.turn_ordinal.saturating_add(context_radius);
        for turn_ordinal in first_ordinal..=last_ordinal {
            ordinals.insert(turn_ordinal);
        }
        if let Some(match_snippet) = matched_turn.match_snippet {
            match_snippets.insert(matched_turn.key.clone(), match_snippet);
        }
        match_keys.insert(matched_turn.key);
    }

    let requested_turn_keys = requested_turns_by_session
        .into_iter()
        .flat_map(|((provider, session_id), turn_ordinals)| {
            turn_ordinals.into_iter().map(move |turn_ordinal| TurnKey {
                provider,
                session_id: session_id.clone(),
                turn_ordinal,
            })
        })
        .collect::<Vec<_>>();
    let mut turns =
        query_turn_summaries_for_keys(connection, request.project_id, &requested_turn_keys)?;
    turns.iter_mut().for_each(|turn| {
        let turn_key = TurnKey {
            provider: turn.provider,
            session_id: turn.session_id.clone(),
            turn_ordinal: turn.turn_ordinal,
        };
        if match_keys.contains(&turn_key) {
            turn.match_kind = Some(TurnMatchKind::Match);
            turn.match_snippet = match_snippets.get(&turn_key).cloned();
        } else {
            turn.match_kind = Some(TurnMatchKind::Context);
            turn.match_snippet = None;
        }
    });
    turns.sort_by(|left, right| {
        let left_session_key = (left.provider, left.session_id.clone());
        let right_session_key = (right.provider, right.session_id.clone());
        session_order
            .get(&left_session_key)
            .cmp(&session_order.get(&right_session_key))
            .then_with(|| left.provider.cmp(&right.provider))
            .then_with(|| left.session_id.cmp(&right.session_id))
            .then_with(|| left.turn_ordinal.cmp(&right.turn_ordinal))
    });
    Ok(turns)
}

/// Queries the ordered FTS match anchors for one grep-scoped turn search.
fn query_turn_match_anchors(
    connection: &Connection,
    request: TurnMatchesQueryRequest<'_>,
    grep: &str,
) -> Result<Vec<TurnMatchAnchor>> {
    let provider = request.provider.map(SourceKind::directory_name);
    let fts_query = build_turn_match_fts_query(grep, request.role)?;
    let mut statement = connection
        .prepare(TURN_MATCHES_SQL)
        .context("failed to prepare grep turn query")?;
    let rows = statement
        .query_map(
            params![
                request.project_id,
                provider,
                request.session_id,
                request.since,
                request.until,
                fts_query
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .context("failed to query grep turn matches")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read grep turn rows")?;
    rows.into_iter()
        .map(
            |(provider, session_id, turn_ordinal, match_snippet)| -> Result<_> {
                Ok(TurnMatchAnchor {
                    key: TurnKey {
                        provider: parse_provider(&provider)?,
                        session_id,
                        turn_ordinal: sql_count_to_u64(turn_ordinal)?,
                    },
                    match_snippet: clean_match_snippet(match_snippet),
                })
            },
        )
        .collect()
}

/// Intersects one matched-turn set with file accesses that satisfy one glob pattern.
fn filter_turn_match_anchors_by_path(
    connection: &Connection,
    project_id: &str,
    project_root: Option<&Path>,
    matches: Vec<TurnMatchAnchor>,
    touched_path: &str,
) -> Result<Vec<TurnMatchAnchor>> {
    let touched_path = normalize_query_path_pattern(project_root, touched_path);
    let pattern = Pattern::new(&touched_path)
        .with_context(|| format!("invalid touched-path glob `{touched_path}`"))?;
    let mut matching_turns = HashSet::<TurnKey>::new();
    let match_options = glob_match_options();

    for match_chunk in matches.chunks(MAX_TURN_KEYS_PER_QUERY) {
        let sql = build_turn_key_values_query_sql(
            match_chunk.len(),
            "
            SELECT
                file_accesses.provider,
                file_accesses.session_id,
                file_accesses.turn_ordinal,
                file_accesses.repo_relative_path,
                file_accesses.path
            FROM requested
            INNER JOIN file_accesses
                ON file_accesses.project_id = ?1
                AND file_accesses.provider = requested.provider
                AND file_accesses.session_id = requested.session_id
                AND file_accesses.turn_ordinal = requested.turn_ordinal
            ORDER BY
                file_accesses.provider ASC,
                file_accesses.session_id ASC,
                file_accesses.turn_ordinal ASC
            ",
        );
        let mut statement = connection
            .prepare(&sql)
            .context("failed to prepare touched-path turn filter query")?;
        let params = build_turn_key_values_params(
            project_id,
            match_chunk.iter().map(|matched_turn| &matched_turn.key),
        )?;
        let rows = statement
            .query_map(params_from_iter(params), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .context("failed to query touched-path file accesses")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read touched-path file access rows")?;
        for (provider, session_id, turn_ordinal, repo_relative_path, path) in rows {
            if path_matches_glob(
                &pattern,
                &match_options,
                project_root,
                repo_relative_path.as_deref(),
                &path,
            ) {
                matching_turns.insert(TurnKey {
                    provider: parse_provider(&provider)?,
                    session_id,
                    turn_ordinal: sql_count_to_u64(turn_ordinal)?,
                });
            }
        }
    }

    Ok(matches
        .into_iter()
        .filter(|matched_turn| matching_turns.contains(&matched_turn.key))
        .collect())
}

/// Queries one deduplicated turn-summary set for the provided turn identities.
fn query_turn_summaries_for_keys(
    connection: &Connection,
    project_id: &str,
    turn_keys: &[TurnKey],
) -> Result<Vec<TurnSummary>> {
    let mut turns = Vec::new();
    for key_chunk in turn_keys.chunks(MAX_TURN_KEYS_PER_QUERY) {
        let sql = build_requested_turn_summaries_sql(key_chunk.len());
        let mut statement = connection
            .prepare(&sql)
            .context("failed to prepare exact turn-summary query")?;
        let params = build_turn_key_values_params(project_id, key_chunk.iter())?;
        let rows = statement
            .query_map(params_from_iter(params), read_turn_summary_row)
            .context("failed to query exact turn summaries")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read exact turn summaries")?;
        turns.extend(
            rows.into_iter()
                .map(build_turn_summary)
                .collect::<Result<Vec<_>>>()?,
        );
    }
    Ok(turns)
}

/// Builds one role-scoped FTS query from one free-form grep string.
fn build_turn_match_fts_query(grep: &str, role: TurnSearchRole) -> Result<String> {
    let query = build_fts_phrase_query(grep)?;
    Ok(match role {
        TurnSearchRole::User => format!("user_message_text : ({query})"),
        TurnSearchRole::Assistant => format!("{{final_answer_text tool_text}} : ({query})"),
        TurnSearchRole::Both => query,
    })
}

/// Converts one FTS snippet with temporary markers into protocol output text.
fn clean_match_snippet(snippet: Option<String>) -> Option<String> {
    snippet
        .map(|snippet| {
            snippet
                .replace(MATCH_SNIPPET_START, "")
                .replace(MATCH_SNIPPET_END, "")
        })
        .filter(|snippet| !snippet.trim().is_empty())
}

/// Validates one requested grep-context radius against the supported maximum.
fn validate_turn_match_context(context: usize) -> Result<u64> {
    if context > MAX_TURN_MATCH_CONTEXT {
        bail!("--context must be at most {MAX_TURN_MATCH_CONTEXT} turns for grep mode");
    }
    u64::try_from(context).context("turn context exceeds u64 range")
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

/// Builds one requested-turn summary SQL query that reuses the shared select list.
fn build_requested_turn_summaries_sql(row_count: usize) -> String {
    build_turn_key_values_query_sql(
        row_count,
        &format!(
            "
            SELECT
{select_list}
            FROM requested
            INNER JOIN turns
                ON turns.project_id = ?1
                AND turns.provider = requested.provider
                AND turns.session_id = requested.session_id
                AND turns.turn_ordinal = requested.turn_ordinal
            ORDER BY
                turns.provider ASC,
                turns.session_id ASC,
                turns.turn_ordinal ASC
            ",
            select_list = build_turn_summary_select_list("turns"),
        ),
    )
}

/// Builds one dynamic `WITH requested AS (VALUES ...)` SQL query for turn-key joins.
fn build_turn_key_values_query_sql(row_count: usize, select_sql: &str) -> String {
    let mut sql = String::from("WITH requested(provider, session_id, turn_ordinal) AS (VALUES ");
    for row_index in 0..row_count {
        if row_index > 0 {
            sql.push_str(", ");
        }
        let base = row_index
            .checked_mul(3)
            .and_then(|value| value.checked_add(2))
            .expect("placeholder index should stay within usize range");
        write!(&mut sql, "(?{base}, ?{}, ?{})", base + 1, base + 2)
            .expect("formatting SQL placeholders should not fail");
    }
    sql.push(')');
    sql.push('\n');
    sql.push_str(select_sql);
    sql
}

/// Builds one SQLite parameter list for a dynamic requested-turn-key query.
fn build_turn_key_values_params<'a>(
    project_id: &str,
    turn_keys: impl IntoIterator<Item = &'a TurnKey>,
) -> Result<Vec<Value>> {
    let mut params = vec![Value::Text(project_id.to_owned())];
    for turn_key in turn_keys {
        params.push(Value::Text(turn_key.provider.directory_name().to_owned()));
        params.push(Value::Text(turn_key.session_id.clone()));
        params.push(Value::Integer(
            i64::try_from(turn_key.turn_ordinal)
                .context("turn ordinal exceeds SQLite INTEGER range")?,
        ));
    }
    Ok(params)
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
        has_final_answer: row.9 != 0,
        step_count: sql_count_to_u64(row.10)?,
        primary_model: row.11,
        token_usage: build_token_usage(row.13, row.14, row.15, row.16, row.17, row.18, row.12)?,
        total_token_count: optional_sql_count_to_u64(row.12)?,
        effective_agent_runtime_ms: optional_sql_count_to_u64(row.19)?,
        changed_file_count: sql_count_to_u64(row.20)?,
        added_line_count: sql_count_to_u64(row.21)?,
        removed_line_count: sql_count_to_u64(row.22)?,
        match_kind: None,
        match_snippet: None,
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
        ("session turns query", session_turns_sql()),
        ("grep turns query", TURN_MATCHES_SQL),
    ] {
        connection
            .prepare(sql)
            .with_context(|| format!("failed to prepare {label}"))?;
    }
    for (label, sql) in [
        (
            "requested turn summaries query",
            build_requested_turn_summaries_sql(1),
        ),
        (
            "requested file accesses query",
            build_turn_key_values_query_sql(
                1,
                "
                SELECT
                    file_accesses.provider,
                    file_accesses.session_id,
                    file_accesses.turn_ordinal,
                    file_accesses.repo_relative_path,
                    file_accesses.path
                FROM requested
                INNER JOIN file_accesses
                    ON file_accesses.project_id = ?1
                    AND file_accesses.provider = requested.provider
                    AND file_accesses.session_id = requested.session_id
                    AND file_accesses.turn_ordinal = requested.turn_ordinal
                ",
            ),
        ),
    ] {
        connection
            .prepare(&sql)
            .with_context(|| format!("failed to prepare {label}"))?;
    }
    Ok(())
}
