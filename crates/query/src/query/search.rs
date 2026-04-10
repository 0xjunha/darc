use std::path::Path;

use anyhow::{Context, Result, bail};
use darc_paths::SourceKind;
use rusqlite::{Connection, params};

use super::{
    SearchMode, SearchTurnHit, SearchTurnsQueryData, SearchTurnsRequest,
    open_existing_index_database, parse_provider, parse_turn_status, preview_text,
    sql_count_to_u64,
};

const KEYWORD_SEARCH_SQL: &str = "
    SELECT
        turn_search.provider,
        turn_search.session_id,
        turn_search.turn_ordinal,
        turns.started_at,
        turns.completed_at,
        turns.status,
        turns.user_message,
        turn_search.user_message_text,
        turn_search.final_answer_text,
        turn_search.tool_text
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
        AND turn_search_fts MATCH ?4
    ORDER BY
        bm25(turn_search_fts) ASC,
        turns.started_at DESC,
        turn_search.provider ASC,
        turn_search.session_id ASC,
        turn_search.turn_ordinal ASC
    LIMIT ?5 OFFSET ?6
";

const FILE_NAME_SEARCH_SQL: &str = "
    SELECT
        turns.provider,
        turns.session_id,
        turns.turn_ordinal,
        turns.started_at,
        turns.completed_at,
        turns.status,
        turns.user_message,
        MIN(
            CASE
                WHEN file_accesses.file_name = ?4 COLLATE NOCASE THEN 0
                WHEN LOWER(file_accesses.file_name) LIKE LOWER(?5) ESCAPE '!' THEN 1
                ELSE 2
            END
        ) AS match_rank,
        GROUP_CONCAT(DISTINCT COALESCE(file_accesses.repo_relative_path, file_accesses.path))
    FROM file_accesses
    INNER JOIN turns
        ON turns.project_id = file_accesses.project_id
        AND turns.provider = file_accesses.provider
        AND turns.session_id = file_accesses.session_id
        AND turns.turn_ordinal = file_accesses.turn_ordinal
    WHERE file_accesses.project_id = ?1
        AND (?2 IS NULL OR file_accesses.provider = ?2)
        AND (?3 IS NULL OR file_accesses.session_id = ?3)
        AND file_accesses.file_name IS NOT NULL
        AND LOWER(file_accesses.file_name) LIKE LOWER(?6) ESCAPE '!'
    GROUP BY
        turns.provider,
        turns.session_id,
        turns.turn_ordinal,
        turns.started_at,
        turns.completed_at,
        turns.status,
        turns.user_message
    ORDER BY
        match_rank ASC,
        turns.started_at DESC,
        turns.provider ASC,
        turns.session_id ASC,
        turns.turn_ordinal ASC
    LIMIT ?7 OFFSET ?8
";

const FILE_PATH_SEARCH_SQL: &str = "
    SELECT
        turns.provider,
        turns.session_id,
        turns.turn_ordinal,
        turns.started_at,
        turns.completed_at,
        turns.status,
        turns.user_message,
        MIN(
            CASE
                WHEN file_accesses.repo_relative_path = ?4 COLLATE NOCASE
                    OR file_accesses.path = ?4 COLLATE NOCASE THEN 0
                WHEN LOWER(COALESCE(file_accesses.repo_relative_path, '')) LIKE LOWER(?5) ESCAPE '!'
                    OR LOWER(file_accesses.path) LIKE LOWER(?5) ESCAPE '!' THEN 1
                ELSE 2
            END
        ) AS match_rank,
        GROUP_CONCAT(DISTINCT COALESCE(file_accesses.repo_relative_path, file_accesses.path))
    FROM file_accesses
    INNER JOIN turns
        ON turns.project_id = file_accesses.project_id
        AND turns.provider = file_accesses.provider
        AND turns.session_id = file_accesses.session_id
        AND turns.turn_ordinal = file_accesses.turn_ordinal
    WHERE file_accesses.project_id = ?1
        AND (?2 IS NULL OR file_accesses.provider = ?2)
        AND (?3 IS NULL OR file_accesses.session_id = ?3)
        AND (
            LOWER(COALESCE(file_accesses.repo_relative_path, '')) LIKE LOWER(?6) ESCAPE '!'
            OR LOWER(file_accesses.path) LIKE LOWER(?6) ESCAPE '!'
        )
    GROUP BY
        turns.provider,
        turns.session_id,
        turns.turn_ordinal,
        turns.started_at,
        turns.completed_at,
        turns.status,
        turns.user_message
    ORDER BY
        match_rank ASC,
        turns.started_at DESC,
        turns.provider ASC,
        turns.session_id ASC,
        turns.turn_ordinal ASC
    LIMIT ?7 OFFSET ?8
";

/// Queries one paginated turn-search payload from the indexed search tables.
pub fn query_search_turns(
    index_db_path: &Path,
    request: SearchTurnsRequest<'_>,
) -> Result<SearchTurnsQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    build_search_turns(&connection, request)
}

/// Builds one paginated turn-search response from the indexed search tables.
fn build_search_turns(
    connection: &Connection,
    request: SearchTurnsRequest<'_>,
) -> Result<SearchTurnsQueryData> {
    let project_id = request.project_id;
    let mode = request.mode;
    let query = request.query.trim();
    let provider = request.provider;
    let session_id = request.session_id;
    let limit = request.limit;
    let offset = request.offset;
    if query.is_empty() {
        bail!("search query must not be empty");
    }

    let limit_plus_one = limit
        .checked_add(1)
        .context("search limit exceeds usize range")?;
    let provider = provider.map(SourceKind::directory_name);
    let hits = match mode {
        SearchMode::Keyword => query_keyword_hits(
            connection,
            project_id,
            provider,
            session_id,
            query,
            limit_plus_one,
            offset,
        )?,
        SearchMode::FileName => query_file_name_hits(
            connection,
            project_id,
            provider,
            session_id,
            query,
            limit_plus_one,
            offset,
        )?,
        SearchMode::FilePath => query_file_path_hits(
            connection,
            project_id,
            provider,
            session_id,
            query,
            limit_plus_one,
            offset,
        )?,
    };
    let has_more = hits.len() > limit;
    let hits = hits.into_iter().take(limit).collect::<Vec<_>>();

    Ok(SearchTurnsQueryData {
        project_id: project_id.to_owned(),
        mode,
        query: query.to_owned(),
        provider: provider.and_then(|value| parse_provider(value).ok()),
        session_id: session_id.map(str::to_owned),
        limit: u64::try_from(limit).context("search limit exceeds u64 range")?,
        offset: u64::try_from(offset).context("search offset exceeds u64 range")?,
        has_more,
        hits,
    })
}

/// Queries keyword search hits ordered by FTS relevance and latest activity.
fn query_keyword_hits(
    connection: &Connection,
    project_id: &str,
    provider: Option<&str>,
    session_id: Option<&str>,
    query: &str,
    limit: usize,
    offset: usize,
) -> Result<Vec<SearchTurnHit>> {
    let fts_query = build_fts_query(query)?;
    let limit = i64::try_from(limit).context("search limit exceeds SQLite INTEGER range")?;
    let offset = i64::try_from(offset).context("search offset exceeds SQLite INTEGER range")?;
    let mut statement = connection
        .prepare(KEYWORD_SEARCH_SQL)
        .context("failed to prepare keyword search query")?;
    let rows = statement
        .query_map(
            params![project_id, provider, session_id, fts_query, limit, offset],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                ))
            },
        )
        .context("failed to query keyword search hits")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read keyword search rows")?;

    rows.into_iter()
        .map(
            |(
                provider,
                session_id,
                turn_ordinal,
                started_at,
                completed_at,
                status,
                user_message,
                user_message_text,
                final_answer_text,
                tool_text,
            )| {
                Ok(SearchTurnHit {
                    provider: parse_provider(&provider)?,
                    session_id,
                    turn_ordinal: sql_count_to_u64(turn_ordinal)?,
                    started_at,
                    completed_at,
                    status: parse_turn_status(&status)?,
                    user_preview: preview_text(&user_message),
                    snippet: build_keyword_snippet(
                        &user_message_text,
                        &final_answer_text,
                        &tool_text,
                        query,
                    ),
                    matched_paths: Vec::new(),
                })
            },
        )
        .collect()
}

/// Queries file-name search hits ordered by exactness and recency.
fn query_file_name_hits(
    connection: &Connection,
    project_id: &str,
    provider: Option<&str>,
    session_id: Option<&str>,
    query: &str,
    limit: usize,
    offset: usize,
) -> Result<Vec<SearchTurnHit>> {
    let prefix_pattern = prefix_like_pattern(query);
    let contains_pattern = contains_like_pattern(query);
    let limit = i64::try_from(limit).context("search limit exceeds SQLite INTEGER range")?;
    let offset = i64::try_from(offset).context("search offset exceeds SQLite INTEGER range")?;
    let mut statement = connection
        .prepare(FILE_NAME_SEARCH_SQL)
        .context("failed to prepare file-name search query")?;
    let rows = statement
        .query_map(
            params![
                project_id,
                provider,
                session_id,
                query,
                prefix_pattern,
                contains_pattern,
                limit,
                offset
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .context("failed to query file-name search hits")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read file-name search rows")?;

    rows.into_iter()
        .map(
            |(
                provider,
                session_id,
                turn_ordinal,
                started_at,
                completed_at,
                status,
                user_message,
                matched_paths,
            )| {
                Ok(SearchTurnHit {
                    provider: parse_provider(&provider)?,
                    session_id,
                    turn_ordinal: sql_count_to_u64(turn_ordinal)?,
                    started_at,
                    completed_at,
                    status: parse_turn_status(&status)?,
                    user_preview: preview_text(&user_message),
                    snippet: None,
                    matched_paths: parse_grouped_paths(&matched_paths),
                })
            },
        )
        .collect()
}

/// Queries file-path search hits ordered by exactness and recency.
fn query_file_path_hits(
    connection: &Connection,
    project_id: &str,
    provider: Option<&str>,
    session_id: Option<&str>,
    query: &str,
    limit: usize,
    offset: usize,
) -> Result<Vec<SearchTurnHit>> {
    let prefix_pattern = prefix_like_pattern(query);
    let contains_pattern = contains_like_pattern(query);
    let limit = i64::try_from(limit).context("search limit exceeds SQLite INTEGER range")?;
    let offset = i64::try_from(offset).context("search offset exceeds SQLite INTEGER range")?;
    let mut statement = connection
        .prepare(FILE_PATH_SEARCH_SQL)
        .context("failed to prepare file-path search query")?;
    let rows = statement
        .query_map(
            params![
                project_id,
                provider,
                session_id,
                query,
                prefix_pattern,
                contains_pattern,
                limit,
                offset
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .context("failed to query file-path search hits")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read file-path search rows")?;

    rows.into_iter()
        .map(
            |(
                provider,
                session_id,
                turn_ordinal,
                started_at,
                completed_at,
                status,
                user_message,
                matched_paths,
            )| {
                Ok(SearchTurnHit {
                    provider: parse_provider(&provider)?,
                    session_id,
                    turn_ordinal: sql_count_to_u64(turn_ordinal)?,
                    started_at,
                    completed_at,
                    status: parse_turn_status(&status)?,
                    user_preview: preview_text(&user_message),
                    snippet: None,
                    matched_paths: parse_grouped_paths(&matched_paths),
                })
            },
        )
        .collect()
}

/// Converts one free-form keyword query into a conservative FTS `MATCH` expression.
fn build_fts_query(query: &str) -> Result<String> {
    let tokens = query
        .split(|ch: char| !(ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.')))
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{token}\""))
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        bail!("search query must contain at least one keyword");
    }
    Ok(tokens.join(" "))
}

/// Builds one SQL `LIKE` prefix pattern with literal wildcard escaping.
fn prefix_like_pattern(query: &str) -> String {
    format!("{}%", escape_like_pattern(query))
}

/// Builds one SQL `LIKE` contains pattern with literal wildcard escaping.
fn contains_like_pattern(query: &str) -> String {
    format!("%{}%", escape_like_pattern(query))
}

/// Escapes literal `LIKE` wildcard characters for one user-provided query fragment.
fn escape_like_pattern(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len());
    for ch in query.chars() {
        match ch {
            '!' | '%' | '_' => {
                escaped.push('!');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Parses one grouped comma-delimited path list back into a deterministic path vector.
fn parse_grouped_paths(value: &str) -> Vec<String> {
    let mut paths = value
        .split(',')
        .filter_map(|path| {
            let path = path.trim();
            (!path.is_empty()).then(|| path.to_owned())
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

/// Builds one compact keyword snippet around the earliest matching query term.
fn build_keyword_snippet(
    user_message_text: &str,
    final_answer_text: &str,
    tool_text: &str,
    query: &str,
) -> Option<String> {
    let mut haystack = String::new();
    for text in [user_message_text, final_answer_text, tool_text] {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !haystack.is_empty() {
            haystack.push('\n');
        }
        haystack.push_str(trimmed);
    }

    let haystack = haystack.split_whitespace().collect::<Vec<_>>().join(" ");
    if haystack.is_empty() {
        return None;
    }

    let lowered = haystack.to_ascii_lowercase();
    let terms = query
        .split_whitespace()
        .map(|term| term.to_ascii_lowercase())
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>();
    let first_match = terms
        .iter()
        .filter_map(|term| lowered.find(term).map(|index| (index, term.len())))
        .min_by_key(|(index, _)| *index);
    let Some((match_start, match_len)) = first_match else {
        return Some(preview_text(&haystack));
    };

    let mut snippet_start = match_start.saturating_sub(48);
    while snippet_start > 0 && !haystack.is_char_boundary(snippet_start) {
        snippet_start -= 1;
    }
    let mut snippet_end = haystack
        .len()
        .min(match_start.saturating_add(match_len).saturating_add(64));
    while snippet_end < haystack.len() && !haystack.is_char_boundary(snippet_end) {
        snippet_end += 1;
    }
    let mut snippet = haystack[snippet_start..snippet_end].to_owned();
    if snippet_start > 0 {
        snippet.insert(0, '…');
    }
    if snippet_end < haystack.len() {
        snippet.push('…');
    }
    Some(snippet)
}

#[cfg(test)]
/// Prepares the turn-search SQL statements against one live schema.
pub(super) fn smoke_test_sql(connection: &Connection) -> Result<()> {
    for (label, sql) in [
        ("keyword search query", KEYWORD_SEARCH_SQL),
        ("file-name search query", FILE_NAME_SEARCH_SQL),
        ("file-path search query", FILE_PATH_SEARCH_SQL),
    ] {
        connection
            .prepare(sql)
            .with_context(|| format!("failed to prepare {label}"))?;
    }
    Ok(())
}
