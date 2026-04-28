use std::{collections::BTreeSet, ops::Range, path::Path};

use anyhow::{Context, Result, bail};
use darc_paths::SourceKind;
use regex::{Regex, RegexBuilder};
use rusqlite::{Connection, params};

use super::{
    SearchMode, SearchTurnHit, SearchTurnMatch, SearchTurnsQueryData, SearchTurnsRequest,
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
        AND (?4 IS NULL OR julianday(turns.started_at) >= julianday(?4))
        AND (?5 IS NULL OR julianday(turns.started_at) < julianday(?5))
        AND turn_search_fts MATCH ?6
    ORDER BY
        bm25(turn_search_fts) ASC,
        turns.started_at DESC,
        turn_search.provider ASC,
        turn_search.session_id ASC,
        turn_search.turn_ordinal ASC
    LIMIT ?7 OFFSET ?8
";

const EVIDENCE_SEARCH_TURNS_SQL: &str = "
    SELECT
        provider,
        session_id,
        turn_ordinal,
        started_at,
        completed_at,
        status,
        user_message
    FROM turns
    WHERE project_id = ?1
        AND (?2 IS NULL OR provider = ?2)
        AND (?3 IS NULL OR session_id = ?3)
        AND (?4 IS NULL OR julianday(started_at) >= julianday(?4))
        AND (?5 IS NULL OR julianday(started_at) < julianday(?5))
        AND (
            ?6 IS NULL
            OR started_at < ?6
            OR (
                started_at = ?6
                AND (
                    provider > ?7
                    OR (
                        provider = ?7
                        AND session_id > ?8
                    )
                    OR (
                        provider = ?7
                        AND session_id = ?8
                        AND turn_ordinal > ?9
                    )
                )
            )
        )
        AND (
            ?10
            OR EXISTS (
                SELECT 1
                FROM turn_evidence
                WHERE turn_evidence.project_id = turns.project_id
                    AND turn_evidence.provider = turns.provider
                    AND turn_evidence.session_id = turns.session_id
                    AND turn_evidence.turn_ordinal = turns.turn_ordinal
                    AND turn_evidence.field <> 'tool_output'
            )
        )
    ORDER BY
        started_at DESC,
        provider ASC,
        session_id ASC,
        turn_ordinal ASC
    LIMIT ?11
";

const LITERAL_EVIDENCE_SEARCH_TURNS_SQL: &str = "
    SELECT
        provider,
        session_id,
        turn_ordinal,
        started_at,
        completed_at,
        status,
        user_message
    FROM turns
    WHERE project_id = ?1
        AND (?2 IS NULL OR provider = ?2)
        AND (?3 IS NULL OR session_id = ?3)
        AND (?4 IS NULL OR julianday(started_at) >= julianday(?4))
        AND (?5 IS NULL OR julianday(started_at) < julianday(?5))
        AND (
            ?6 IS NULL
            OR started_at < ?6
            OR (
                started_at = ?6
                AND (
                    provider > ?7
                    OR (
                        provider = ?7
                        AND session_id > ?8
                    )
                    OR (
                        provider = ?7
                        AND session_id = ?8
                        AND turn_ordinal > ?9
                    )
                )
            )
        )
        AND EXISTS (
            SELECT 1
            FROM turn_evidence
            WHERE turn_evidence.project_id = turns.project_id
                AND turn_evidence.provider = turns.provider
                AND turn_evidence.session_id = turns.session_id
                AND turn_evidence.turn_ordinal = turns.turn_ordinal
                AND (?10 OR turn_evidence.field <> 'tool_output')
                AND instr(turn_evidence.text, ?11) > 0
        )
    ORDER BY
        started_at DESC,
        provider ASC,
        session_id ASC,
        turn_ordinal ASC
    LIMIT ?12
";

const TURN_EVIDENCE_ROWS_SQL: &str = "
    SELECT
        field,
        text
    FROM turn_evidence
    WHERE project_id = ?1
        AND provider = ?2
        AND session_id = ?3
        AND turn_ordinal = ?4
        AND (?5 OR field <> 'tool_output')
    ORDER BY evidence_ordinal ASC
";

const LITERAL_TURN_EVIDENCE_ROWS_SQL: &str = "
    SELECT
        field,
        text
    FROM turn_evidence
    WHERE project_id = ?1
        AND provider = ?2
        AND session_id = ?3
        AND turn_ordinal = ?4
        AND (?5 OR field <> 'tool_output')
        AND instr(text, ?6) > 0
    ORDER BY evidence_ordinal ASC
    LIMIT ?7
";

const EVIDENCE_SEARCH_TURN_BATCH_ROWS: usize = 1_000;
const MAX_EVIDENCE_MATCHES_PER_TURN: usize = 20;
const MAX_REGEX_QUERY_CHARS: usize = 1_024;
const REGEX_SIZE_LIMIT_BYTES: usize = 1_000_000;
const REGEX_DFA_SIZE_LIMIT_BYTES: usize = 1_000_000;
const SEARCH_SNIPPET_CONTEXT_CHARS: usize = 80;

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

/// Stores the last turn scanned by keyset pagination.
#[derive(Debug, Clone)]
struct EvidenceTurnCursor {
    started_at: String,
    provider: String,
    session_id: String,
    turn_ordinal: i64,
}

/// Stores shared filters applied by every turn-search mode.
#[derive(Debug, Clone, Copy)]
struct SearchScope<'a> {
    project_id: &'a str,
    provider: Option<&'a str>,
    session_id: Option<&'a str>,
    since: Option<&'a str>,
    until: Option<&'a str>,
}

/// Stores one keyword search request after CLI/project resolution.
#[derive(Debug, Clone, Copy)]
struct KeywordSearchRequest<'a> {
    scope: SearchScope<'a>,
    query: &'a str,
    limit: usize,
    offset: usize,
}

/// Stores one exact evidence search request after CLI/project resolution.
#[derive(Debug, Clone, Copy)]
struct EvidenceSearchRequest<'a> {
    scope: SearchScope<'a>,
    mode: SearchMode,
    query: &'a str,
    include_tool_output: bool,
    limit: usize,
    offset: usize,
}

/// Stores one file search request after CLI/project resolution.
#[derive(Debug, Clone, Copy)]
struct FileSearchRequest<'a> {
    scope: SearchScope<'a>,
    query: &'a str,
    kind: FileSearchKind,
    desired_hit_count: usize,
}

/// Stores one candidate turn before scanning its evidence rows.
#[derive(Debug, Clone)]
struct EvidenceSearchTurn {
    provider: SourceKind,
    provider_key: String,
    session_id: String,
    turn_ordinal: u64,
    turn_ordinal_key: i64,
    started_at: String,
    completed_at: Option<String>,
    status: darc_rollout::model::NormalizedTurnStatus,
    user_preview: String,
}

/// Stores one evidence fragment for in-process exact matching.
#[derive(Debug, Clone)]
struct EvidenceTextRow {
    field: String,
    text: String,
}

/// Stores the exact text-matching strategy for one evidence search request.
enum EvidenceMatcher {
    Regex(Regex),
}

/// Stores one concrete staged file-search request.
#[derive(Debug, Clone, Copy)]
struct FileSearchStageRequest<'a> {
    scope: SearchScope<'a>,
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
    validate_tool_output_inclusion(mode, request.include_tool_output)?;
    let query = search_query_for_mode(mode, request.query)?;
    let response_provider = request.provider;
    let provider_filter = response_provider.map(SourceKind::directory_name);
    let session_id = request.session_id;
    let since = request.since;
    let until = request.until;
    let limit = request.limit;
    let offset = request.offset;
    let scope = SearchScope {
        project_id,
        provider: provider_filter,
        session_id,
        since,
        until,
    };

    let hits = match mode {
        SearchMode::Keyword => {
            let limit_plus_one = limit
                .checked_add(1)
                .context("search limit exceeds usize range")?;
            let has_more_hits = query_keyword_hits(
                connection,
                KeywordSearchRequest {
                    scope,
                    query,
                    limit: limit_plus_one,
                    offset,
                },
            )?;
            let has_more = has_more_hits.len() > limit;
            let hits = has_more_hits.into_iter().take(limit).collect::<Vec<_>>();
            return Ok(SearchTurnsQueryData {
                project_id: project_id.to_owned(),
                mode,
                query: query.to_owned(),
                include_tool_output: request.include_tool_output,
                provider: response_provider,
                session_id: session_id.map(str::to_owned),
                since: since.map(str::to_owned),
                until: until.map(str::to_owned),
                limit: u64::try_from(limit).context("search limit exceeds u64 range")?,
                offset: u64::try_from(offset).context("search offset exceeds u64 range")?,
                has_more,
                hits,
            });
        }
        SearchMode::Literal | SearchMode::Regex => query_evidence_hits(
            connection,
            EvidenceSearchRequest {
                scope,
                mode,
                query,
                include_tool_output: request.include_tool_output,
                limit,
                offset,
            },
        )?,
        SearchMode::FileName | SearchMode::FilePath => {
            let desired_hit_count = offset
                .checked_add(limit)
                .and_then(|value| value.checked_add(1))
                .context("search pagination exceeds usize range")?;
            query_file_hits(
                connection,
                FileSearchRequest {
                    scope,
                    query,
                    kind: match mode {
                        SearchMode::FileName => FileSearchKind::Name,
                        SearchMode::FilePath => FileSearchKind::Path,
                        SearchMode::Keyword | SearchMode::Literal | SearchMode::Regex => {
                            unreachable!("handled above")
                        }
                    },
                    desired_hit_count,
                },
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
        include_tool_output: request.include_tool_output,
        provider: response_provider,
        session_id: session_id.map(str::to_owned),
        since: since.map(str::to_owned),
        until: until.map(str::to_owned),
        limit: u64::try_from(limit).context("search limit exceeds u64 range")?,
        offset: u64::try_from(offset).context("search offset exceeds u64 range")?,
        has_more,
        hits,
    })
}

/// Rejects tool-output inclusion for search modes that never inspect turn evidence rows.
fn validate_tool_output_inclusion(mode: SearchMode, include_tool_output: bool) -> Result<()> {
    if include_tool_output && !matches!(mode, SearchMode::Literal | SearchMode::Regex) {
        bail!("--include-tool-output is only supported with --mode literal or --mode regex");
    }
    Ok(())
}

/// Returns the query text with mode-specific exactness rules applied.
fn search_query_for_mode(mode: SearchMode, query: &str) -> Result<&str> {
    match mode {
        SearchMode::Literal | SearchMode::Regex => {
            if query.is_empty() {
                bail!("search query must not be empty");
            }
            Ok(query)
        }
        SearchMode::Keyword | SearchMode::FileName | SearchMode::FilePath => {
            let query = query.trim();
            if query.is_empty() {
                bail!("search query must not be empty");
            }
            Ok(query)
        }
    }
}

/// Queries keyword search hits ordered by FTS relevance and latest activity.
fn query_keyword_hits(
    connection: &Connection,
    request: KeywordSearchRequest<'_>,
) -> Result<Vec<SearchTurnHit>> {
    let scope = request.scope;
    let fts_query = build_fts_query(request.query)?;
    let limit =
        i64::try_from(request.limit).context("search limit exceeds SQLite INTEGER range")?;
    let offset =
        i64::try_from(request.offset).context("search offset exceeds SQLite INTEGER range")?;
    let mut statement = connection
        .prepare(KEYWORD_SEARCH_SQL)
        .context("failed to prepare keyword search query")?;
    let rows = statement
        .query_map(
            params![
                scope.project_id,
                scope.provider,
                scope.session_id,
                scope.since,
                scope.until,
                fts_query,
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
                    matches: Vec::new(),
                    matches_truncated: false,
                })
            },
        )
        .collect()
}

/// Queries exact evidence search hits and groups matching rows back into turns.
fn query_evidence_hits(
    connection: &Connection,
    request: EvidenceSearchRequest<'_>,
) -> Result<Vec<SearchTurnHit>> {
    match request.mode {
        SearchMode::Literal => query_literal_evidence_hits_by_turn(connection, request),
        SearchMode::Regex => {
            let matcher = build_regex_matcher(request.query)?;
            query_regex_evidence_hits_by_turn(connection, request, &matcher)
        }
        SearchMode::Keyword | SearchMode::FileName | SearchMode::FilePath => {
            unreachable!("only exact evidence modes use evidence search")
        }
    }
}

/// Queries regex evidence matches by scanning turn evidence in turn result order.
fn query_regex_evidence_hits_by_turn(
    connection: &Connection,
    request: EvidenceSearchRequest<'_>,
    matcher: &EvidenceMatcher,
) -> Result<Vec<SearchTurnHit>> {
    let scope = request.scope;
    let mut turn_statement = connection
        .prepare(EVIDENCE_SEARCH_TURNS_SQL)
        .context("failed to prepare evidence turn search query")?;
    let mut evidence_statement = connection
        .prepare(TURN_EVIDENCE_ROWS_SQL)
        .context("failed to prepare turn evidence row query")?;

    collect_evidence_hits_by_turn(
        request,
        |cursor, turn_limit| {
            query_regex_evidence_turn_batch(
                &mut turn_statement,
                scope,
                cursor,
                request.include_tool_output,
                turn_limit,
            )
        },
        |turn| {
            query_regex_evidence_hit_for_turn(
                &mut evidence_statement,
                scope.project_id,
                turn,
                matcher,
                request.include_tool_output,
            )
        },
    )
}

/// Queries literal evidence matches by letting SQLite discard nonmatching evidence rows.
fn query_literal_evidence_hits_by_turn(
    connection: &Connection,
    request: EvidenceSearchRequest<'_>,
) -> Result<Vec<SearchTurnHit>> {
    let scope = request.scope;
    let mut turn_statement = connection
        .prepare(LITERAL_EVIDENCE_SEARCH_TURNS_SQL)
        .context("failed to prepare literal evidence turn search query")?;
    let mut evidence_statement = connection
        .prepare(LITERAL_TURN_EVIDENCE_ROWS_SQL)
        .context("failed to prepare literal turn evidence row query")?;

    collect_evidence_hits_by_turn(
        request,
        |cursor, turn_limit| {
            query_literal_evidence_turn_batch(
                &mut turn_statement,
                scope,
                cursor,
                request.query,
                request.include_tool_output,
                turn_limit,
            )
        },
        |turn| {
            query_literal_evidence_hit_for_turn(
                &mut evidence_statement,
                scope.project_id,
                turn,
                request.query,
                request.include_tool_output,
            )
        },
    )
}

/// Collects turn hits from a turn-batch query and a per-turn evidence matcher.
fn collect_evidence_hits_by_turn<QueryTurns, QueryHit>(
    request: EvidenceSearchRequest<'_>,
    mut query_turns: QueryTurns,
    mut query_hit: QueryHit,
) -> Result<Vec<SearchTurnHit>>
where
    QueryTurns: FnMut(Option<&EvidenceTurnCursor>, i64) -> Result<Vec<EvidenceSearchTurn>>,
    QueryHit: FnMut(EvidenceSearchTurn) -> Result<Option<SearchTurnHit>>,
{
    let page_end = request
        .offset
        .checked_add(request.limit)
        .context("search pagination exceeds usize range")?;
    let turn_limit = i64::try_from(EVIDENCE_SEARCH_TURN_BATCH_ROWS)
        .context("evidence turn batch size exceeds SQLite INTEGER range")?;
    let mut hits = Vec::<SearchTurnHit>::new();
    let mut cursor = None::<EvidenceTurnCursor>;

    loop {
        let turns = query_turns(cursor.as_ref(), turn_limit)?;
        let batch_rows = turns.len();
        for turn in turns {
            cursor = Some(EvidenceTurnCursor::from_turn(&turn));
            if let Some(hit) = query_hit(turn)? {
                hits.push(hit);
                if hits.len() > page_end {
                    return Ok(hits);
                }
            }
        }

        if batch_rows < EVIDENCE_SEARCH_TURN_BATCH_ROWS {
            break;
        }
    }

    Ok(hits)
}

/// Queries one regex candidate-turn batch without filtering evidence text in SQLite.
fn query_regex_evidence_turn_batch(
    statement: &mut rusqlite::Statement<'_>,
    scope: SearchScope<'_>,
    cursor: Option<&EvidenceTurnCursor>,
    include_tool_output: bool,
    turn_limit: i64,
) -> Result<Vec<EvidenceSearchTurn>> {
    let cursor_started_at = cursor.map(|value| value.started_at.as_str());
    let cursor_provider = cursor.map(|value| value.provider.as_str());
    let cursor_session_id = cursor.map(|value| value.session_id.as_str());
    let cursor_turn_ordinal = cursor.map(|value| value.turn_ordinal);
    let mut rows = statement
        .query(params![
            scope.project_id,
            scope.provider,
            scope.session_id,
            scope.since,
            scope.until,
            cursor_started_at,
            cursor_provider,
            cursor_session_id,
            cursor_turn_ordinal,
            include_tool_output,
            turn_limit
        ])
        .context("failed to query evidence search turns")?;
    let mut turns = Vec::new();
    while let Some(row) = rows.next().context("failed to read evidence search turn")? {
        turns.push(read_evidence_search_turn(row)?);
    }
    Ok(turns)
}

/// Queries one literal candidate-turn batch with SQLite text prefiltering.
fn query_literal_evidence_turn_batch(
    statement: &mut rusqlite::Statement<'_>,
    scope: SearchScope<'_>,
    cursor: Option<&EvidenceTurnCursor>,
    query: &str,
    include_tool_output: bool,
    turn_limit: i64,
) -> Result<Vec<EvidenceSearchTurn>> {
    let cursor_started_at = cursor.map(|value| value.started_at.as_str());
    let cursor_provider = cursor.map(|value| value.provider.as_str());
    let cursor_session_id = cursor.map(|value| value.session_id.as_str());
    let cursor_turn_ordinal = cursor.map(|value| value.turn_ordinal);
    let mut rows = statement
        .query(params![
            scope.project_id,
            scope.provider,
            scope.session_id,
            scope.since,
            scope.until,
            cursor_started_at,
            cursor_provider,
            cursor_session_id,
            cursor_turn_ordinal,
            include_tool_output,
            query,
            turn_limit
        ])
        .context("failed to query literal evidence search turns")?;
    let mut turns = Vec::new();
    while let Some(row) = rows
        .next()
        .context("failed to read literal evidence search turn")?
    {
        turns.push(read_evidence_search_turn(row)?);
    }
    Ok(turns)
}

/// Queries matching literal evidence rows for one already-matched turn.
fn query_literal_evidence_hit_for_turn(
    statement: &mut rusqlite::Statement<'_>,
    project_id: &str,
    turn: EvidenceSearchTurn,
    query: &str,
    include_tool_output: bool,
) -> Result<Option<SearchTurnHit>> {
    let match_limit = i64::try_from(MAX_EVIDENCE_MATCHES_PER_TURN.saturating_add(1))
        .context("evidence match preview limit exceeds SQLite INTEGER range")?;
    let mut rows = statement
        .query(params![
            project_id,
            turn.provider_key.as_str(),
            turn.session_id.as_str(),
            turn.turn_ordinal_key,
            include_tool_output,
            query,
            match_limit
        ])
        .context("failed to query literal turn evidence rows")?;
    let mut matches = Vec::<SearchTurnMatch>::new();
    let mut matches_truncated = false;
    while let Some(row) = rows
        .next()
        .context("failed to read literal turn evidence row")?
    {
        let evidence = read_evidence_text_row(row)?;
        if let Some(range) = literal_match_range(&evidence.text, query) {
            if matches.len() >= MAX_EVIDENCE_MATCHES_PER_TURN {
                matches_truncated = true;
                break;
            }
            matches.push(SearchTurnMatch {
                field: evidence.field,
                snippet: evidence_snippet(&evidence.text, range),
            });
        }
    }
    if matches.is_empty() {
        return Ok(None);
    }

    Ok(Some(build_evidence_search_hit(
        turn,
        matches,
        matches_truncated,
    )))
}

/// Queries and matches all evidence rows for one regex candidate turn.
fn query_regex_evidence_hit_for_turn(
    statement: &mut rusqlite::Statement<'_>,
    project_id: &str,
    turn: EvidenceSearchTurn,
    matcher: &EvidenceMatcher,
    include_tool_output: bool,
) -> Result<Option<SearchTurnHit>> {
    let mut rows = statement
        .query(params![
            project_id,
            turn.provider_key.as_str(),
            turn.session_id.as_str(),
            turn.turn_ordinal_key,
            include_tool_output
        ])
        .context("failed to query turn evidence rows")?;
    let mut matches = Vec::<SearchTurnMatch>::new();
    let mut matches_truncated = false;
    while let Some(row) = rows.next().context("failed to read turn evidence row")? {
        let evidence = read_evidence_text_row(row)?;
        if let Some(range) = matcher.find_match(&evidence.text) {
            if matches.len() >= MAX_EVIDENCE_MATCHES_PER_TURN {
                matches_truncated = true;
                break;
            }
            matches.push(SearchTurnMatch {
                field: evidence.field,
                snippet: evidence_snippet(&evidence.text, range),
            });
        }
    }
    if matches.is_empty() {
        return Ok(None);
    }

    Ok(Some(build_evidence_search_hit(
        turn,
        matches,
        matches_truncated,
    )))
}

/// Builds one exact evidence-search hit from matched evidence previews.
fn build_evidence_search_hit(
    turn: EvidenceSearchTurn,
    matches: Vec<SearchTurnMatch>,
    matches_truncated: bool,
) -> SearchTurnHit {
    SearchTurnHit {
        provider: turn.provider,
        session_id: turn.session_id,
        turn_ordinal: turn.turn_ordinal,
        started_at: turn.started_at,
        completed_at: turn.completed_at,
        status: turn.status,
        user_preview: turn.user_preview,
        snippet: None,
        matched_paths: Vec::new(),
        matches,
        matches_truncated,
    }
}

impl EvidenceTurnCursor {
    /// Builds one keyset cursor from the last scanned turn.
    fn from_turn(turn: &EvidenceSearchTurn) -> Self {
        Self {
            started_at: turn.started_at.clone(),
            provider: turn.provider_key.clone(),
            session_id: turn.session_id.clone(),
            turn_ordinal: turn.turn_ordinal_key,
        }
    }
}

/// Builds the regex matcher used by exact evidence search.
fn build_regex_matcher(query: &str) -> Result<EvidenceMatcher> {
    if query.chars().count() > MAX_REGEX_QUERY_CHARS {
        bail!("regex query must be at most {MAX_REGEX_QUERY_CHARS} characters");
    }
    let regex = RegexBuilder::new(query)
        .size_limit(REGEX_SIZE_LIMIT_BYTES)
        .dfa_size_limit(REGEX_DFA_SIZE_LIMIT_BYTES)
        .build()
        .context("invalid regex search query")?;
    Ok(EvidenceMatcher::Regex(regex))
}

impl EvidenceMatcher {
    /// Returns the first matching byte range in one evidence string.
    fn find_match(&self, text: &str) -> Option<Range<usize>> {
        match self {
            Self::Regex(regex) => regex.find(text).map(|matched| matched.range()),
        }
    }
}

/// Returns the first literal matching byte range in one evidence string.
fn literal_match_range(text: &str, query: &str) -> Option<Range<usize>> {
    text.find(query).map(|start| start..start + query.len())
}

/// Reads one candidate turn for turn-ordered evidence search.
fn read_evidence_search_turn(row: &rusqlite::Row<'_>) -> Result<EvidenceSearchTurn> {
    let provider_key = row.get::<_, String>(0)?;
    let turn_ordinal_key = row.get::<_, i64>(2)?;
    Ok(EvidenceSearchTurn {
        provider: parse_provider(&provider_key)?,
        provider_key,
        session_id: row.get(1)?,
        turn_ordinal: sql_count_to_u64(turn_ordinal_key)?,
        turn_ordinal_key,
        started_at: row.get(3)?,
        completed_at: row.get(4)?,
        status: parse_turn_status(&row.get::<_, String>(5)?)?,
        user_preview: preview_text(&row.get::<_, String>(6)?),
    })
}

/// Reads one evidence text row for exact matching.
fn read_evidence_text_row(row: &rusqlite::Row<'_>) -> Result<EvidenceTextRow> {
    Ok(EvidenceTextRow {
        field: row.get(0)?,
        text: row.get(1)?,
    })
}

/// Builds one bounded evidence snippet around a matched byte range.
fn evidence_snippet(text: &str, matched: Range<usize>) -> String {
    let start_char = text[..matched.start].chars().count();
    let end_char = text[..matched.end].chars().count();
    let snippet_start = start_char.saturating_sub(SEARCH_SNIPPET_CONTEXT_CHARS);
    let snippet_end = end_char.saturating_add(SEARCH_SNIPPET_CONTEXT_CHARS);
    let total_chars = text.chars().count();

    let mut snippet = String::new();
    if snippet_start > 0 {
        snippet.push('…');
    }
    for (index, ch) in text.chars().enumerate() {
        if index < snippet_start {
            continue;
        }
        if index >= snippet_end {
            break;
        }
        snippet.push(ch);
    }
    if snippet_end < total_chars {
        snippet.push('…');
    }
    snippet
}

/// Queries staged file-search hits so exact and prefix matches rank before contains matches.
fn query_file_hits(
    connection: &Connection,
    request: FileSearchRequest<'_>,
) -> Result<Vec<SearchTurnHit>> {
    let scope = request.scope;
    let query = request.query;
    let desired_hit_count = request.desired_hit_count;
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

        // Query a full page from every stage because later stages can overlap earlier ones.
        // The shared dedupe pass below owns the final page boundary.
        let stage_hits = query_file_hits_stage(
            connection,
            FileSearchStageRequest {
                scope,
                kind: request.kind,
                stage,
                pattern: &pattern,
                limit: desired_hit_count,
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
    let scope = request.scope;
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
            params![
                scope.project_id,
                scope.provider,
                scope.session_id,
                pattern,
                scope.since,
                scope.until,
                limit
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
        matches: Vec::new(),
        matches_truncated: false,
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
                AND (?5 IS NULL OR julianday(turns.started_at) >= julianday(?5))
                AND (?6 IS NULL OR julianday(turns.started_at) < julianday(?6))
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
            LIMIT ?7
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
pub(crate) fn build_fts_query(query: &str) -> Result<String> {
    Ok(tokenize_fts_query(query)?
        .into_iter()
        .map(|token| format!("\"{token}\""))
        .collect::<Vec<_>>()
        .join(" "))
}

/// Tokenizes one free-form text query into the normalized FTS terms Darc indexes.
fn tokenize_fts_query(query: &str) -> Result<Vec<String>> {
    let tokens = query
        .split(|ch: char| !(ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.')))
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        bail!("search query must contain at least one keyword");
    }
    Ok(tokens.into_iter().map(str::to_owned).collect())
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
    connection
        .prepare(EVIDENCE_SEARCH_TURNS_SQL)
        .context("failed to prepare evidence turn search query")?;
    connection
        .prepare(LITERAL_EVIDENCE_SEARCH_TURNS_SQL)
        .context("failed to prepare literal evidence turn search query")?;
    connection
        .prepare(TURN_EVIDENCE_ROWS_SQL)
        .context("failed to prepare turn evidence row query")?;
    connection
        .prepare(LITERAL_TURN_EVIDENCE_ROWS_SQL)
        .context("failed to prepare literal turn evidence row query")?;
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
