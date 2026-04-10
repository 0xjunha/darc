use std::{collections::BTreeSet, path::Path};

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
        NULLIF(snippet(turn_search_fts, -1, '', '', '…', 16), '')
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

/// Identifies one file-search mode that shares the staged query pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileSearchKind {
    Name,
    Path,
}

/// Identifies one staged file-search predicate bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileSearchStage {
    Exact,
    Prefix,
    Contains,
}

/// Stores one grouped file-search row while reconstructing turn hits in Rust.
#[derive(Debug, Clone)]
struct FileSearchRow {
    provider: SourceKind,
    session_id: String,
    turn_ordinal: u64,
    started_at: String,
    completed_at: Option<String>,
    status: darc_rollout::model::NormalizedTurnStatus,
    user_preview: String,
    matched_path: String,
}

/// Stores one in-progress file-search hit while grouping SQL rows.
#[derive(Debug, Clone)]
struct FileSearchHitAccumulator {
    provider: SourceKind,
    session_id: String,
    turn_ordinal: u64,
    started_at: String,
    completed_at: Option<String>,
    status: darc_rollout::model::NormalizedTurnStatus,
    user_preview: String,
    matched_paths: BTreeSet<String>,
}

type SearchTurnKey = (SourceKind, String, u64);

/// Stores one concrete staged file-search request.
#[derive(Debug, Clone, Copy)]
struct FileSearchStageRequest<'a> {
    project_id: &'a str,
    provider: Option<&'a str>,
    session_id: Option<&'a str>,
    kind: FileSearchKind,
    stage: FileSearchStage,
    pattern: &'a str,
    limit: usize,
}

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
    let response_provider = request.provider;
    let provider_filter = response_provider.map(SourceKind::directory_name);
    let session_id = request.session_id;
    let limit = request.limit;
    let offset = request.offset;
    if query.is_empty() {
        bail!("search query must not be empty");
    }

    let hits = match mode {
        SearchMode::Keyword => {
            let limit_plus_one = limit
                .checked_add(1)
                .context("search limit exceeds usize range")?;
            let has_more_hits = query_keyword_hits(
                connection,
                project_id,
                provider_filter,
                session_id,
                query,
                limit_plus_one,
                offset,
            )?;
            let has_more = has_more_hits.len() > limit;
            let hits = has_more_hits.into_iter().take(limit).collect::<Vec<_>>();
            return Ok(SearchTurnsQueryData {
                project_id: project_id.to_owned(),
                mode,
                query: query.to_owned(),
                provider: response_provider,
                session_id: session_id.map(str::to_owned),
                limit: u64::try_from(limit).context("search limit exceeds u64 range")?,
                offset: u64::try_from(offset).context("search offset exceeds u64 range")?,
                has_more,
                hits,
            });
        }
        SearchMode::FileName | SearchMode::FilePath => {
            let desired_hit_count = offset
                .checked_add(limit)
                .and_then(|value| value.checked_add(1))
                .context("search pagination exceeds usize range")?;
            query_file_hits(
                connection,
                project_id,
                provider_filter,
                session_id,
                query,
                match mode {
                    SearchMode::FileName => FileSearchKind::Name,
                    SearchMode::FilePath => FileSearchKind::Path,
                    SearchMode::Keyword => unreachable!("handled above"),
                },
                desired_hit_count,
            )?
        }
    };

    let page_end = offset
        .checked_add(limit)
        .context("search pagination exceeds usize range")?;
    let has_more = hits.len() > page_end;
    let hits = hits
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();

    Ok(SearchTurnsQueryData {
        project_id: project_id.to_owned(),
        mode,
        query: query.to_owned(),
        provider: response_provider,
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
                    row.get::<_, Option<String>>(7)?,
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
                snippet,
            )| {
                Ok(SearchTurnHit {
                    provider: parse_provider(&provider)?,
                    session_id,
                    turn_ordinal: sql_count_to_u64(turn_ordinal)?,
                    started_at,
                    completed_at,
                    status: parse_turn_status(&status)?,
                    user_preview: preview_text(&user_message),
                    snippet: snippet
                        .filter(|value| !value.trim().is_empty())
                        .or_else(|| Some(preview_text(&user_message))),
                    matched_paths: Vec::new(),
                })
            },
        )
        .collect()
}

/// Queries staged file-search hits so exact and prefix matches can use dedicated indexes first.
fn query_file_hits(
    connection: &Connection,
    project_id: &str,
    provider: Option<&str>,
    session_id: Option<&str>,
    query: &str,
    kind: FileSearchKind,
    desired_hit_count: usize,
) -> Result<Vec<SearchTurnHit>> {
    if desired_hit_count == 0 {
        return Ok(Vec::new());
    }

    let mut hits = Vec::<SearchTurnHit>::new();
    let mut seen = BTreeSet::<SearchTurnKey>::new();
    for (stage, pattern) in [
        (FileSearchStage::Exact, query.to_owned()),
        (FileSearchStage::Prefix, prefix_like_pattern(query)),
        (FileSearchStage::Contains, contains_like_pattern(query)),
    ] {
        if hits.len() >= desired_hit_count {
            break;
        }

        let remaining = desired_hit_count.saturating_sub(hits.len());
        let stage_hits = query_file_hits_stage(
            connection,
            FileSearchStageRequest {
                project_id,
                provider,
                session_id,
                kind,
                stage,
                pattern: &pattern,
                limit: remaining,
            },
        )?;
        for hit in stage_hits {
            let key = (hit.provider, hit.session_id.clone(), hit.turn_ordinal);
            if !seen.insert(key) {
                continue;
            }
            hits.push(hit);
            if hits.len() >= desired_hit_count {
                break;
            }
        }
    }

    Ok(hits)
}

/// Queries one staged file-search bucket and groups the matching paths per turn in Rust.
fn query_file_hits_stage(
    connection: &Connection,
    request: FileSearchStageRequest<'_>,
) -> Result<Vec<SearchTurnHit>> {
    let project_id = request.project_id;
    let provider = request.provider;
    let session_id = request.session_id;
    let kind = request.kind;
    let stage = request.stage;
    let pattern = request.pattern;
    let limit = request.limit;
    if limit == 0 {
        return Ok(Vec::new());
    }

    let sql = build_file_search_stage_sql(kind, stage);
    let limit = i64::try_from(limit).context("search limit exceeds SQLite INTEGER range")?;
    let mut statement = connection
        .prepare(&sql)
        .with_context(|| format!("failed to prepare {kind:?} {stage:?} search query"))?;
    let rows = statement
        .query_map(
            params![project_id, provider, session_id, pattern, limit],
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
                ))
            },
        )
        .with_context(|| format!("failed to query {kind:?} {stage:?} search hits"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("failed to read {kind:?} {stage:?} search rows"))?;
    let rows = rows
        .into_iter()
        .map(
            |(
                provider,
                session_id,
                turn_ordinal,
                started_at,
                completed_at,
                status,
                user_message,
                matched_path,
            )| {
                Ok(FileSearchRow {
                    provider: parse_provider(&provider)?,
                    session_id,
                    turn_ordinal: sql_count_to_u64(turn_ordinal)?,
                    started_at,
                    completed_at,
                    status: parse_turn_status(&status)?,
                    user_preview: preview_text(&user_message),
                    matched_path,
                })
            },
        )
        .collect::<Result<Vec<_>>>()?;

    group_file_search_rows(rows)
}

/// Groups one ordered file-search row stream back into turn hits with lossless matched paths.
fn group_file_search_rows(rows: Vec<FileSearchRow>) -> Result<Vec<SearchTurnHit>> {
    let mut grouped = Vec::<SearchTurnHit>::new();
    let mut current = None::<FileSearchHitAccumulator>;

    for row in rows {
        match current.as_mut() {
            Some(accumulator)
                if accumulator.provider == row.provider
                    && accumulator.session_id == row.session_id
                    && accumulator.turn_ordinal == row.turn_ordinal =>
            {
                accumulator.matched_paths.insert(row.matched_path);
            }
            Some(_) => {
                let finished = current
                    .take()
                    .context("missing grouped file-search accumulator")?;
                grouped.push(finalize_file_search_hit(finished));
                current = Some(start_file_search_hit(row));
            }
            None => current = Some(start_file_search_hit(row)),
        }
    }

    if let Some(accumulator) = current {
        grouped.push(finalize_file_search_hit(accumulator));
    }

    Ok(grouped)
}

/// Starts one grouped file-search hit from the first row for a turn.
fn start_file_search_hit(row: FileSearchRow) -> FileSearchHitAccumulator {
    let mut matched_paths = BTreeSet::new();
    matched_paths.insert(row.matched_path);
    FileSearchHitAccumulator {
        provider: row.provider,
        session_id: row.session_id,
        turn_ordinal: row.turn_ordinal,
        started_at: row.started_at,
        completed_at: row.completed_at,
        status: row.status,
        user_preview: row.user_preview,
        matched_paths,
    }
}

/// Finalizes one grouped file-search hit after every matching path has been collected.
fn finalize_file_search_hit(accumulator: FileSearchHitAccumulator) -> SearchTurnHit {
    SearchTurnHit {
        provider: accumulator.provider,
        session_id: accumulator.session_id,
        turn_ordinal: accumulator.turn_ordinal,
        started_at: accumulator.started_at,
        completed_at: accumulator.completed_at,
        status: accumulator.status,
        user_preview: accumulator.user_preview,
        snippet: None,
        matched_paths: accumulator.matched_paths.into_iter().collect(),
    }
}

/// Builds the SQL for one staged file-search query using only vetted internal fragments.
fn build_file_search_stage_sql(kind: FileSearchKind, stage: FileSearchStage) -> String {
    let guard = file_search_guard_sql(kind);
    let predicate = file_search_predicate_sql(kind, stage);
    let matched_path = "COALESCE(file_accesses.repo_relative_path, file_accesses.path)";

    format!(
        "
        WITH matched_turns AS (
            SELECT
                turns.provider,
                turns.session_id,
                turns.turn_ordinal,
                turns.started_at,
                turns.completed_at,
                turns.status,
                turns.user_message
            FROM file_accesses
            INNER JOIN turns
                ON turns.project_id = file_accesses.project_id
                AND turns.provider = file_accesses.provider
                AND turns.session_id = file_accesses.session_id
                AND turns.turn_ordinal = file_accesses.turn_ordinal
            WHERE file_accesses.project_id = ?1
                AND (?2 IS NULL OR file_accesses.provider = ?2)
                AND (?3 IS NULL OR file_accesses.session_id = ?3)
                AND {guard}
                AND {predicate}
            GROUP BY
                turns.provider,
                turns.session_id,
                turns.turn_ordinal,
                turns.started_at,
                turns.completed_at,
                turns.status,
                turns.user_message
            ORDER BY
                turns.started_at DESC,
                turns.provider ASC,
                turns.session_id ASC,
                turns.turn_ordinal ASC
            LIMIT ?5
        )
        SELECT
            matched_turns.provider,
            matched_turns.session_id,
            matched_turns.turn_ordinal,
            matched_turns.started_at,
            matched_turns.completed_at,
            matched_turns.status,
            matched_turns.user_message,
            {matched_path} AS matched_path
        FROM matched_turns
        INNER JOIN file_accesses
            ON file_accesses.project_id = ?1
            AND file_accesses.provider = matched_turns.provider
            AND file_accesses.session_id = matched_turns.session_id
            AND file_accesses.turn_ordinal = matched_turns.turn_ordinal
        WHERE {guard}
            AND {predicate}
        ORDER BY
            matched_turns.started_at DESC,
            matched_turns.provider ASC,
            matched_turns.session_id ASC,
            matched_turns.turn_ordinal ASC,
            matched_path COLLATE NOCASE ASC
        "
    )
}

/// Returns any additional non-null guards required for one file-search kind.
fn file_search_guard_sql(kind: FileSearchKind) -> &'static str {
    match kind {
        FileSearchKind::Name => "file_accesses.file_name IS NOT NULL",
        FileSearchKind::Path => "1 = 1",
    }
}

/// Returns the predicate SQL for one vetted file-search kind and stage.
fn file_search_predicate_sql(kind: FileSearchKind, stage: FileSearchStage) -> &'static str {
    match (kind, stage) {
        (FileSearchKind::Name, FileSearchStage::Exact) => {
            "file_accesses.file_name = ?4 COLLATE NOCASE"
        }
        (FileSearchKind::Name, FileSearchStage::Prefix)
        | (FileSearchKind::Name, FileSearchStage::Contains) => {
            "file_accesses.file_name LIKE ?4 ESCAPE '!' COLLATE NOCASE"
        }
        (FileSearchKind::Path, FileSearchStage::Exact) => {
            "(file_accesses.repo_relative_path = ?4 COLLATE NOCASE OR file_accesses.path = ?4 COLLATE NOCASE)"
        }
        (FileSearchKind::Path, FileSearchStage::Prefix)
        | (FileSearchKind::Path, FileSearchStage::Contains) => {
            "(file_accesses.repo_relative_path LIKE ?4 ESCAPE '!' COLLATE NOCASE OR file_accesses.path LIKE ?4 ESCAPE '!' COLLATE NOCASE)"
        }
    }
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

#[cfg(test)]
/// Prepares the turn-search SQL statements against one live schema.
pub(super) fn smoke_test_sql(connection: &Connection) -> Result<()> {
    connection
        .prepare(KEYWORD_SEARCH_SQL)
        .context("failed to prepare keyword search query")?;
    for kind in [FileSearchKind::Name, FileSearchKind::Path] {
        for stage in [
            FileSearchStage::Exact,
            FileSearchStage::Prefix,
            FileSearchStage::Contains,
        ] {
            connection
                .prepare(&build_file_search_stage_sql(kind, stage))
                .with_context(|| format!("failed to prepare {kind:?} {stage:?} search query"))?;
        }
    }
    Ok(())
}
