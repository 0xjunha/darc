use std::{collections::BTreeSet, ops::Range, path::Path};

use anyhow::{Context, Result, bail};
use darc_paths::SourceKind;
use glob::{MatchOptions, Pattern};
use regex::{Regex, RegexBuilder};
use rusqlite::{Connection, params, params_from_iter, types::Value};

use super::{
    DEFAULT_SEARCH_MATCH_LIMIT, SearchEvidenceField, SearchMode, SearchTurnHit, SearchTurnMatch,
    SearchTurnsQueryData, SearchTurnsRequest, apply_matched_path_limit,
    open_existing_index_database, parse_provider, parse_turn_status, preview_text,
    sql_count_to_u64,
};
use crate::query::files::{
    PathQuerySelector, build_path_query_selector, glob_match_options, normalize_query_path_pattern,
    path_matches_glob,
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
        turns.final_answer_text,
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

const EVIDENCE_SEARCH_TURN_BATCH_ROWS: usize = 1_000;
const FILE_PATH_GLOB_TURN_BATCH_ROWS: usize = 1_000;
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
    user_prompt_preview: String,
    user_prompt_preview_chars: u64,
    user_prompt_total_chars: u64,
    agent_answer_preview: Option<String>,
    agent_answer_preview_chars: Option<u64>,
    agent_answer_total_chars: Option<u64>,
    matched_path: String,
}

/// Stores one file-access row that still needs glob verification in Rust.
#[derive(Debug, Clone)]
struct FilePathGlobRow {
    provider: SourceKind,
    session_id: String,
    turn_ordinal: u64,
    started_at: String,
    completed_at: Option<String>,
    status: darc_rollout::model::NormalizedTurnStatus,
    user_prompt_preview: String,
    user_prompt_preview_chars: u64,
    user_prompt_total_chars: u64,
    agent_answer_preview: Option<String>,
    agent_answer_preview_chars: Option<u64>,
    agent_answer_total_chars: Option<u64>,
    repo_relative_path: Option<String>,
    path: String,
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
    user_prompt_preview: String,
    user_prompt_preview_chars: u64,
    user_prompt_total_chars: u64,
    agent_answer_preview: Option<String>,
    agent_answer_preview_chars: Option<u64>,
    agent_answer_total_chars: Option<u64>,
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
    project_root: Option<&'a Path>,
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
    fields: &'a EvidenceFieldSelection,
    match_limit: usize,
    limit: usize,
    offset: usize,
}

/// Stores the resolved exact-search evidence fields allowed by request filters.
#[derive(Debug, Clone)]
struct EvidenceFieldSelection {
    allowed: Vec<SearchEvidenceField>,
    included: Vec<SearchEvidenceField>,
    excluded: Vec<SearchEvidenceField>,
}

/// Stores one file search request after CLI/project resolution.
#[derive(Debug, Clone, Copy)]
struct FileSearchRequest<'a> {
    scope: SearchScope<'a>,
    query: &'a str,
    kind: FileSearchKind,
    desired_hit_count: usize,
}

/// Stores one glob path-search request after CLI/project resolution.
#[derive(Debug, Clone, Copy)]
struct FilePathGlobSearchRequest<'a> {
    scope: SearchScope<'a>,
    query: &'a str,
    desired_hit_count: usize,
}

/// Stores one SQL candidate-turn page for glob path verification.
#[derive(Debug, Clone)]
struct FilePathGlobTurnBatch {
    rows: Vec<FilePathGlobRow>,
    candidate_turn_count: usize,
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
    user_prompt_preview: String,
    user_prompt_preview_chars: u64,
    user_prompt_total_chars: u64,
    agent_answer_preview: Option<String>,
    agent_answer_preview_chars: Option<u64>,
    agent_answer_total_chars: Option<u64>,
}

/// Stores one evidence fragment for in-process exact matching.
#[derive(Debug, Clone)]
struct EvidenceTextRow {
    evidence_ordinal: u64,
    field: String,
    text: String,
}

/// Stores the exact text-matching strategy for one evidence search request.
enum EvidenceMatcher {
    Regex(Regex),
}

/// Stores one reusable matcher for terminal search snippet presentation.
pub struct SearchSnippetMatcher {
    kind: SearchSnippetMatcherKind,
}

/// Stores the supported snippet presentation matching strategies.
enum SearchSnippetMatcherKind {
    Literal(String),
    Regex(EvidenceMatcher),
    Unsupported,
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

/// Returns the first matched byte range for terminal presentation of one search snippet.
pub fn search_snippet_match_range(
    mode: SearchMode,
    query: &str,
    snippet: &str,
) -> Result<Option<Range<usize>>> {
    Ok(SearchSnippetMatcher::new(mode, query)?.find(snippet))
}

impl SearchSnippetMatcher {
    /// Builds one reusable search snippet matcher.
    pub fn new(mode: SearchMode, query: &str) -> Result<Self> {
        let kind = match mode {
            SearchMode::Literal => SearchSnippetMatcherKind::Literal(query.to_owned()),
            SearchMode::Regex => SearchSnippetMatcherKind::Regex(build_regex_matcher(query)?),
            SearchMode::Keyword
            | SearchMode::FileName
            | SearchMode::FilePath
            | SearchMode::PathFragment => SearchSnippetMatcherKind::Unsupported,
        };
        Ok(Self { kind })
    }

    /// Returns the first matching byte range in one rendered snippet.
    pub fn find(&self, snippet: &str) -> Option<Range<usize>> {
        match &self.kind {
            SearchSnippetMatcherKind::Literal(query) => literal_match_range(snippet, query),
            SearchSnippetMatcherKind::Regex(matcher) => matcher.find_match(snippet),
            SearchSnippetMatcherKind::Unsupported => None,
        }
    }
}

/// Builds one paginated turn-search response from the indexed search tables.
fn build_search_turns(
    connection: &Connection,
    request: SearchTurnsRequest<'_>,
) -> Result<SearchTurnsQueryData> {
    let project_id = request.project_id;
    let mode = request.mode;
    let fields = EvidenceFieldSelection::from_request(
        mode,
        request.include_tool_output,
        request.fields,
        request.excluded_fields,
    )?;
    let query = search_query_for_mode(mode, request.query)?;
    let response_provider = request.provider;
    let provider_filter = response_provider.map(SourceKind::directory_name);
    let session_id = request.session_id;
    let since = request.since;
    let until = request.until;
    let limit = request.limit;
    let offset = request.offset;
    let matched_path_limit = request.matched_path_limit;
    let match_limit = resolve_match_limit(mode, request.match_limit)?;
    let scope = SearchScope {
        project_id,
        provider: provider_filter,
        session_id,
        since,
        until,
        project_root: request.project_root,
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
                fields: fields.included_labels(),
                excluded_fields: fields.excluded_labels(),
                provider: response_provider,
                session_id: session_id.map(str::to_owned),
                since: since.map(str::to_owned),
                until: until.map(str::to_owned),
                limit: u64::try_from(limit).context("search limit exceeds u64 range")?,
                offset: u64::try_from(offset).context("search offset exceeds u64 range")?,
                has_more,
                matched_path_limit: matched_path_limit
                    .map(u64::try_from)
                    .transpose()
                    .context("matched path limit exceeds u64 range")?,
                match_limit: None,
                hits,
            });
        }
        SearchMode::Literal | SearchMode::Regex => query_evidence_hits(
            connection,
            EvidenceSearchRequest {
                scope,
                mode,
                query,
                fields: &fields,
                match_limit,
                limit,
                offset,
            },
        )?,
        SearchMode::FileName | SearchMode::PathFragment => {
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
                        SearchMode::PathFragment => FileSearchKind::Path,
                        SearchMode::Keyword
                        | SearchMode::Literal
                        | SearchMode::Regex
                        | SearchMode::FilePath => {
                            unreachable!("handled above")
                        }
                    },
                    desired_hit_count,
                },
            )?
        }
        SearchMode::FilePath => {
            let desired_hit_count = offset
                .checked_add(limit)
                .and_then(|value| value.checked_add(1))
                .context("search pagination exceeds usize range")?;
            query_file_path_glob_hits(
                connection,
                FilePathGlobSearchRequest {
                    scope,
                    query,
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
        .map(|hit| apply_search_hit_matched_path_limit(hit, matched_path_limit))
        .collect::<Vec<_>>();

    Ok(SearchTurnsQueryData {
        project_id: project_id.to_owned(),
        mode,
        query: query.to_owned(),
        include_tool_output: request.include_tool_output,
        fields: fields.included_labels(),
        excluded_fields: fields.excluded_labels(),
        provider: response_provider,
        session_id: session_id.map(str::to_owned),
        since: since.map(str::to_owned),
        until: until.map(str::to_owned),
        limit: u64::try_from(limit).context("search limit exceeds u64 range")?,
        offset: u64::try_from(offset).context("search offset exceeds u64 range")?,
        has_more,
        matched_path_limit: matched_path_limit
            .map(u64::try_from)
            .transpose()
            .context("matched path limit exceeds u64 range")?,
        match_limit: if matches!(mode, SearchMode::Literal | SearchMode::Regex) {
            Some(u64::try_from(match_limit).context("match limit exceeds u64 range")?)
        } else {
            None
        },
        hits,
    })
}

/// Resolves and validates the per-hit exact-search match preview cap.
fn resolve_match_limit(mode: SearchMode, match_limit: Option<usize>) -> Result<usize> {
    let uses_evidence_rows = matches!(mode, SearchMode::Literal | SearchMode::Regex);
    if match_limit.is_some() && !uses_evidence_rows {
        bail!("--match-limit is only supported with --mode literal or --mode regex");
    }
    Ok(match_limit.unwrap_or(DEFAULT_SEARCH_MATCH_LIMIT))
}

/// Applies the matched-path preview cap to one search hit.
fn apply_search_hit_matched_path_limit(
    mut hit: SearchTurnHit,
    matched_path_limit: Option<usize>,
) -> SearchTurnHit {
    hit.matched_paths_count = u64::try_from(hit.matched_paths.len()).unwrap_or(u64::MAX);
    let (matched_paths, matched_paths_truncated) =
        apply_matched_path_limit(hit.matched_paths, matched_path_limit);
    hit.matched_paths = matched_paths;
    hit.matched_paths_truncated = matched_paths_truncated;
    hit
}

impl EvidenceFieldSelection {
    /// Builds the allowed exact-search evidence fields from one request.
    fn from_request(
        mode: SearchMode,
        include_tool_output: bool,
        included: &[SearchEvidenceField],
        excluded: &[SearchEvidenceField],
    ) -> Result<Self> {
        let uses_evidence_rows = matches!(mode, SearchMode::Literal | SearchMode::Regex);
        if include_tool_output && !uses_evidence_rows {
            bail!("--include-tool-output is only supported with --mode literal or --mode regex");
        }
        if (!included.is_empty() || !excluded.is_empty()) && !uses_evidence_rows {
            bail!(
                "--field and --exclude-field are only supported with --mode literal or --mode regex"
            );
        }
        if included.contains(&SearchEvidenceField::ToolOutput) && !include_tool_output {
            bail!("--field tool-output requires --include-tool-output");
        }
        if let Some(overlap) = included.iter().find(|field| excluded.contains(field)) {
            bail!(
                "evidence field `{}` cannot be both included and excluded",
                overlap.as_str()
            );
        }

        let included = unique_evidence_fields(included);
        let excluded = unique_evidence_fields(excluded);
        let all_fields = SearchEvidenceField::ALL;
        let base_fields = if included.is_empty() {
            all_fields.as_slice()
        } else {
            included.as_slice()
        };
        let allowed = base_fields
            .iter()
            .copied()
            .filter(|field| include_tool_output || *field != SearchEvidenceField::ToolOutput)
            .filter(|field| !excluded.contains(field))
            .collect::<Vec<_>>();
        if uses_evidence_rows && allowed.is_empty() {
            bail!("exact search field filters exclude every evidence field");
        }

        Ok(Self {
            allowed,
            included,
            excluded,
        })
    }

    /// Returns request-included field labels for response echoes.
    fn included_labels(&self) -> Vec<String> {
        field_labels(&self.included)
    }

    /// Returns request-excluded field labels for response echoes.
    fn excluded_labels(&self) -> Vec<String> {
        field_labels(&self.excluded)
    }

    /// Builds one SQL `IN` predicate over the resolved allowed fields.
    fn sql_predicate(&self, column: &str) -> String {
        let placeholders = std::iter::repeat_n("?", self.allowed.len())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{column} IN ({placeholders})")
    }

    /// Appends the resolved allowed evidence fields as SQLite values.
    fn push_params(&self, params: &mut Vec<Value>) {
        params.extend(
            self.allowed
                .iter()
                .map(|field| Value::Text(field.as_str().to_owned())),
        );
    }
}

/// Deduplicates evidence fields while preserving caller order.
fn unique_evidence_fields(fields: &[SearchEvidenceField]) -> Vec<SearchEvidenceField> {
    fields
        .iter()
        .copied()
        .fold(Vec::new(), |mut unique, field| {
            if !unique.contains(&field) {
                unique.push(field);
            }
            unique
        })
}

/// Converts evidence fields to their stable query-protocol labels.
fn field_labels(fields: &[SearchEvidenceField]) -> Vec<String> {
    fields
        .iter()
        .map(|field| field.as_str().to_owned())
        .collect()
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
        SearchMode::Keyword
        | SearchMode::FileName
        | SearchMode::FilePath
        | SearchMode::PathFragment => {
            let query = query.trim();
            if query.is_empty() {
                bail!("search query must not be empty");
            }
            Ok(query)
        }
    }
}

/// Builds the candidate-turn SQL for exact evidence search.
fn evidence_search_turns_sql(fields: &EvidenceFieldSelection, literal: bool) -> String {
    let field_predicate = fields.sql_predicate("turn_evidence.field");
    let text_predicate = if literal {
        " AND instr(turn_evidence.text, ?) > 0"
    } else {
        ""
    };
    format!(
        "
    SELECT
        provider,
        session_id,
        turn_ordinal,
        started_at,
        completed_at,
        status,
        user_message,
        final_answer_text
    FROM turns
    WHERE project_id = ?
        AND (? IS NULL OR provider = ?)
        AND (? IS NULL OR session_id = ?)
        AND (? IS NULL OR julianday(started_at) >= julianday(?))
        AND (? IS NULL OR julianday(started_at) < julianday(?))
        AND (
            ? IS NULL
            OR started_at < ?
            OR (
                started_at = ?
                AND (
                    provider > ?
                    OR (
                        provider = ?
                        AND session_id > ?
                    )
                    OR (
                        provider = ?
                        AND session_id = ?
                        AND turn_ordinal > ?
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
                AND {field_predicate}{text_predicate}
        )
    ORDER BY
        started_at DESC,
        provider ASC,
        session_id ASC,
        turn_ordinal ASC
    LIMIT ?
"
    )
}

/// Builds the per-turn evidence row SQL for exact evidence search.
fn turn_evidence_rows_sql(fields: &EvidenceFieldSelection, literal: bool) -> String {
    let field_predicate = fields.sql_predicate("field");
    let text_predicate = if literal {
        " AND instr(text, ?) > 0"
    } else {
        ""
    };
    let limit = if literal { "\n    LIMIT ?" } else { "" };
    format!(
        "
    SELECT
        evidence_ordinal,
        field,
        text
    FROM turn_evidence
    WHERE project_id = ?
        AND provider = ?
        AND session_id = ?
        AND turn_ordinal = ?
        AND {field_predicate}{text_predicate}
    ORDER BY evidence_ordinal ASC{limit}
"
    )
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
                    row.get::<_, Option<String>>(8)?,
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
                final_answer_text,
                snippet,
            )| {
                let user_prompt_preview = preview_text(&user_message);
                let agent_answer_preview =
                    optional_agent_answer_preview(final_answer_text.as_deref());
                Ok(SearchTurnHit {
                    provider: parse_provider(&provider)?,
                    session_id,
                    turn_ordinal: sql_count_to_u64(turn_ordinal)?,
                    started_at,
                    completed_at,
                    status: parse_turn_status(&status)?,
                    user_prompt_preview: user_prompt_preview.text.clone(),
                    user_prompt_preview_chars: user_prompt_preview.chars,
                    user_prompt_total_chars: user_prompt_preview.total_chars,
                    agent_answer_preview: agent_answer_preview
                        .as_ref()
                        .map(|preview| preview.text.clone()),
                    agent_answer_preview_chars: agent_answer_preview
                        .as_ref()
                        .map(|preview| preview.chars),
                    agent_answer_total_chars: agent_answer_preview
                        .as_ref()
                        .map(|preview| preview.total_chars),
                    snippet: snippet
                        .filter(|value| !value.trim().is_empty())
                        .or(Some(user_prompt_preview.text)),
                    matched_paths: Vec::new(),
                    matched_paths_count: 0,
                    matched_paths_truncated: false,
                    matches: Vec::new(),
                    matches_count: 0,
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
        SearchMode::Keyword
        | SearchMode::FileName
        | SearchMode::FilePath
        | SearchMode::PathFragment => {
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
    let turn_sql = evidence_search_turns_sql(request.fields, false);
    let evidence_sql = turn_evidence_rows_sql(request.fields, false);
    let mut turn_statement = connection
        .prepare(&turn_sql)
        .context("failed to prepare evidence turn search query")?;
    let mut evidence_statement = connection
        .prepare(&evidence_sql)
        .context("failed to prepare turn evidence row query")?;

    collect_evidence_hits_by_turn(
        request,
        |cursor, turn_limit| {
            query_regex_evidence_turn_batch(
                &mut turn_statement,
                scope,
                cursor,
                request.fields,
                turn_limit,
            )
        },
        |turn| {
            query_regex_evidence_hit_for_turn(
                &mut evidence_statement,
                scope.project_id,
                turn,
                matcher,
                request.fields,
                request.match_limit,
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
    let turn_sql = evidence_search_turns_sql(request.fields, true);
    let evidence_sql = turn_evidence_rows_sql(request.fields, true);
    let mut turn_statement = connection
        .prepare(&turn_sql)
        .context("failed to prepare literal evidence turn search query")?;
    let mut evidence_statement = connection
        .prepare(&evidence_sql)
        .context("failed to prepare literal turn evidence row query")?;

    collect_evidence_hits_by_turn(
        request,
        |cursor, turn_limit| {
            query_literal_evidence_turn_batch(
                &mut turn_statement,
                scope,
                cursor,
                request.query,
                request.fields,
                turn_limit,
            )
        },
        |turn| {
            query_literal_evidence_hit_for_turn(
                &mut evidence_statement,
                scope.project_id,
                turn,
                request.query,
                request.fields,
                request.match_limit,
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
    fields: &EvidenceFieldSelection,
    turn_limit: i64,
) -> Result<Vec<EvidenceSearchTurn>> {
    let params = build_evidence_turn_batch_params(scope, cursor, fields, None, turn_limit);
    let mut rows = statement
        .query(params_from_iter(params))
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
    fields: &EvidenceFieldSelection,
    turn_limit: i64,
) -> Result<Vec<EvidenceSearchTurn>> {
    let params = build_evidence_turn_batch_params(scope, cursor, fields, Some(query), turn_limit);
    let mut rows = statement
        .query(params_from_iter(params))
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

/// Builds SQLite parameters for one exact-search candidate-turn batch.
fn build_evidence_turn_batch_params(
    scope: SearchScope<'_>,
    cursor: Option<&EvidenceTurnCursor>,
    fields: &EvidenceFieldSelection,
    literal_query: Option<&str>,
    turn_limit: i64,
) -> Vec<Value> {
    let cursor_started_at = cursor.map(|value| value.started_at.as_str());
    let cursor_provider = cursor.map(|value| value.provider.as_str());
    let cursor_session_id = cursor.map(|value| value.session_id.as_str());
    let cursor_turn_ordinal = cursor.map(|value| value.turn_ordinal);
    let mut params = vec![
        Value::Text(scope.project_id.to_owned()),
        optional_text_value(scope.provider),
        optional_text_value(scope.provider),
        optional_text_value(scope.session_id),
        optional_text_value(scope.session_id),
        optional_text_value(scope.since),
        optional_text_value(scope.since),
        optional_text_value(scope.until),
        optional_text_value(scope.until),
        optional_text_value(cursor_started_at),
        optional_text_value(cursor_started_at),
        optional_text_value(cursor_started_at),
        optional_text_value(cursor_provider),
        optional_text_value(cursor_provider),
        optional_text_value(cursor_session_id),
        optional_text_value(cursor_provider),
        optional_text_value(cursor_session_id),
        cursor_turn_ordinal.map_or(Value::Null, Value::Integer),
    ];
    fields.push_params(&mut params);
    if let Some(query) = literal_query {
        params.push(Value::Text(query.to_owned()));
    }
    params.push(Value::Integer(turn_limit));
    params
}

/// Queries matching literal evidence rows for one already-matched turn.
fn query_literal_evidence_hit_for_turn(
    statement: &mut rusqlite::Statement<'_>,
    project_id: &str,
    turn: EvidenceSearchTurn,
    query: &str,
    fields: &EvidenceFieldSelection,
    match_limit: usize,
) -> Result<Option<SearchTurnHit>> {
    let sql_match_limit = i64::try_from(match_limit.saturating_add(1))
        .context("evidence match preview limit exceeds SQLite INTEGER range")?;
    let mut params = vec![
        Value::Text(project_id.to_owned()),
        Value::Text(turn.provider_key.clone()),
        Value::Text(turn.session_id.clone()),
        Value::Integer(turn.turn_ordinal_key),
    ];
    fields.push_params(&mut params);
    params.push(Value::Text(query.to_owned()));
    params.push(Value::Integer(sql_match_limit));
    let mut rows = statement
        .query(params_from_iter(params))
        .context("failed to query literal turn evidence rows")?;
    let mut matches = Vec::<SearchTurnMatch>::new();
    let mut matches_truncated = false;
    while let Some(row) = rows
        .next()
        .context("failed to read literal turn evidence row")?
    {
        let evidence = read_evidence_text_row(row)?;
        if let Some(range) = literal_match_range(&evidence.text, query) {
            if matches.len() >= match_limit {
                matches_truncated = true;
                break;
            }
            matches.push(SearchTurnMatch {
                evidence_ordinal: evidence.evidence_ordinal,
                field: evidence.field,
                snippet: evidence_snippet(&evidence.text, range),
            });
        }
    }
    if matches.is_empty() && !matches_truncated {
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
    fields: &EvidenceFieldSelection,
    match_limit: usize,
) -> Result<Option<SearchTurnHit>> {
    let mut params = vec![
        Value::Text(project_id.to_owned()),
        Value::Text(turn.provider_key.clone()),
        Value::Text(turn.session_id.clone()),
        Value::Integer(turn.turn_ordinal_key),
    ];
    fields.push_params(&mut params);
    let mut rows = statement
        .query(params_from_iter(params))
        .context("failed to query turn evidence rows")?;
    let mut matches = Vec::<SearchTurnMatch>::new();
    let mut matches_truncated = false;
    while let Some(row) = rows.next().context("failed to read turn evidence row")? {
        let evidence = read_evidence_text_row(row)?;
        if let Some(range) = matcher.find_match(&evidence.text) {
            if matches.len() >= match_limit {
                matches_truncated = true;
                break;
            }
            matches.push(SearchTurnMatch {
                evidence_ordinal: evidence.evidence_ordinal,
                field: evidence.field,
                snippet: evidence_snippet(&evidence.text, range),
            });
        }
    }
    if matches.is_empty() && !matches_truncated {
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
        user_prompt_preview: turn.user_prompt_preview,
        user_prompt_preview_chars: turn.user_prompt_preview_chars,
        user_prompt_total_chars: turn.user_prompt_total_chars,
        agent_answer_preview: turn.agent_answer_preview,
        agent_answer_preview_chars: turn.agent_answer_preview_chars,
        agent_answer_total_chars: turn.agent_answer_total_chars,
        snippet: None,
        matched_paths: Vec::new(),
        matched_paths_count: 0,
        matched_paths_truncated: false,
        matches_count: u64::try_from(matches.len()).unwrap_or(u64::MAX),
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
    let final_answer_text = row.get::<_, Option<String>>(7)?;
    let user_prompt_preview = preview_text(&row.get::<_, String>(6)?);
    let agent_answer_preview = optional_agent_answer_preview(final_answer_text.as_deref());
    Ok(EvidenceSearchTurn {
        provider: parse_provider(&provider_key)?,
        provider_key,
        session_id: row.get(1)?,
        turn_ordinal: sql_count_to_u64(turn_ordinal_key)?,
        turn_ordinal_key,
        started_at: row.get(3)?,
        completed_at: row.get(4)?,
        status: parse_turn_status(&row.get::<_, String>(5)?)?,
        user_prompt_preview: user_prompt_preview.text,
        user_prompt_preview_chars: user_prompt_preview.chars,
        user_prompt_total_chars: user_prompt_preview.total_chars,
        agent_answer_preview: agent_answer_preview
            .as_ref()
            .map(|preview| preview.text.clone()),
        agent_answer_preview_chars: agent_answer_preview.as_ref().map(|preview| preview.chars),
        agent_answer_total_chars: agent_answer_preview
            .as_ref()
            .map(|preview| preview.total_chars),
    })
}

/// Reads one evidence text row for exact matching.
fn read_evidence_text_row(row: &rusqlite::Row<'_>) -> Result<EvidenceTextRow> {
    Ok(EvidenceTextRow {
        evidence_ordinal: sql_count_to_u64(row.get::<_, i64>(0)?)?,
        field: row.get(1)?,
        text: row.get(2)?,
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

/// Queries path-glob file hits and verifies candidates with the shared path matcher.
fn query_file_path_glob_hits(
    connection: &Connection,
    request: FilePathGlobSearchRequest<'_>,
) -> Result<Vec<SearchTurnHit>> {
    let desired_hit_count = request.desired_hit_count;
    if desired_hit_count == 0 {
        return Ok(Vec::new());
    }

    let scope = request.scope;
    let query = normalize_query_path_pattern(scope.project_root, request.query);
    let pattern =
        Pattern::new(&query).with_context(|| format!("invalid file-path glob `{query}`"))?;
    let path_selector = build_path_query_selector(scope.project_root, &query);
    let match_options = glob_match_options();
    let mut hits = Vec::<SearchTurnHit>::new();
    let mut candidate_offset = 0usize;

    loop {
        let batch = query_file_path_glob_turn_batch(
            connection,
            scope,
            &path_selector,
            FILE_PATH_GLOB_TURN_BATCH_ROWS,
            candidate_offset,
        )?;
        if batch.candidate_turn_count == 0 {
            break;
        }

        hits.extend(group_file_path_glob_rows(
            batch.rows,
            &pattern,
            &match_options,
            scope.project_root,
        ));
        if hits.len() >= desired_hit_count {
            break;
        }
        if batch.candidate_turn_count < FILE_PATH_GLOB_TURN_BATCH_ROWS {
            break;
        }
        candidate_offset = candidate_offset
            .checked_add(batch.candidate_turn_count)
            .context("search candidate offset exceeds usize range")?;
    }

    Ok(hits)
}

/// Queries one page of candidate turns and their path rows for glob verification.
fn query_file_path_glob_turn_batch(
    connection: &Connection,
    scope: SearchScope<'_>,
    path_selector: &PathQuerySelector,
    limit: usize,
    offset: usize,
) -> Result<FilePathGlobTurnBatch> {
    if matches!(path_selector, PathQuerySelector::Impossible) {
        return Ok(FilePathGlobTurnBatch {
            rows: Vec::new(),
            candidate_turn_count: 0,
        });
    }

    let sql = build_file_path_glob_turn_batch_sql(path_selector);
    let limit = i64::try_from(limit).context("search limit exceeds SQLite INTEGER range")?;
    let offset = i64::try_from(offset).context("search offset exceeds SQLite INTEGER range")?;
    let mut params = vec![
        Value::Text(scope.project_id.to_owned()),
        optional_text_value(scope.provider),
        optional_text_value(scope.session_id),
        optional_text_value(scope.since),
        optional_text_value(scope.until),
        Value::Integer(limit),
        Value::Integer(offset),
    ];
    if matches!(
        path_selector,
        PathQuerySelector::Exact { .. } | PathQuerySelector::Prefix { .. }
    ) {
        params.extend(path_selector.params());
    }

    let mut statement = connection
        .prepare(&sql)
        .context("failed to prepare file-path glob search query")?;
    let rows = statement
        .query_map(params_from_iter(params), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
            ))
        })
        .context("failed to query file-path glob search rows")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read file-path glob search rows")?;

    let mut candidate_turns = BTreeSet::<SearchTurnKey>::new();
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
                final_answer_text,
                repo_relative_path,
                path,
                matched_path,
            )| {
                let provider = parse_provider(&provider)?;
                let turn_ordinal = sql_count_to_u64(turn_ordinal)?;
                let user_prompt_preview = preview_text(&user_message);
                let agent_answer_preview =
                    optional_agent_answer_preview(final_answer_text.as_deref());
                candidate_turns.insert((provider, session_id.clone(), turn_ordinal));
                Ok(FilePathGlobRow {
                    provider,
                    session_id,
                    turn_ordinal,
                    started_at,
                    completed_at,
                    status: parse_turn_status(&status)?,
                    user_prompt_preview: user_prompt_preview.text,
                    user_prompt_preview_chars: user_prompt_preview.chars,
                    user_prompt_total_chars: user_prompt_preview.total_chars,
                    agent_answer_preview: agent_answer_preview
                        .as_ref()
                        .map(|preview| preview.text.clone()),
                    agent_answer_preview_chars: agent_answer_preview
                        .as_ref()
                        .map(|preview| preview.chars),
                    agent_answer_total_chars: agent_answer_preview
                        .as_ref()
                        .map(|preview| preview.total_chars),
                    repo_relative_path,
                    path,
                    matched_path,
                })
            },
        )
        .collect::<Result<Vec<_>>>()?;

    Ok(FilePathGlobTurnBatch {
        rows,
        candidate_turn_count: candidate_turns.len(),
    })
}

/// Converts optional string filters into SQLite values for dynamic query assembly.
fn optional_text_value(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |value| Value::Text(value.to_owned()))
}

/// Builds one optional agent-answer preview with its size metadata.
fn optional_agent_answer_preview(text: Option<&str>) -> Option<super::TextPreview> {
    text.map(preview_text)
}

/// Groups glob-verified path rows back into turn hits.
fn group_file_path_glob_rows(
    rows: Vec<FilePathGlobRow>,
    pattern: &Pattern,
    options: &MatchOptions,
    project_root: Option<&Path>,
) -> Vec<SearchTurnHit> {
    let mut grouped = Vec::<SearchTurnHit>::new();
    let mut current = None::<FileSearchHitAccumulator>;

    for row in rows {
        let is_same_turn = current.as_ref().is_some_and(|accumulator| {
            accumulator.provider == row.provider
                && accumulator.session_id == row.session_id
                && accumulator.turn_ordinal == row.turn_ordinal
        });
        if !is_same_turn {
            push_glob_hit_if_matched(&mut grouped, current.take());
            current = Some(start_file_path_glob_hit(&row));
        }

        if let Some(accumulator) = current.as_mut() {
            record_glob_path_match(accumulator, row, pattern, options, project_root);
        }
    }

    push_glob_hit_if_matched(&mut grouped, current);
    grouped
}

/// Starts one grouped glob path-search hit without assuming the first row matches.
fn start_file_path_glob_hit(row: &FilePathGlobRow) -> FileSearchHitAccumulator {
    FileSearchHitAccumulator {
        provider: row.provider,
        session_id: row.session_id.clone(),
        turn_ordinal: row.turn_ordinal,
        started_at: row.started_at.clone(),
        completed_at: row.completed_at.clone(),
        status: row.status,
        user_prompt_preview: row.user_prompt_preview.clone(),
        user_prompt_preview_chars: row.user_prompt_preview_chars,
        user_prompt_total_chars: row.user_prompt_total_chars,
        agent_answer_preview: row.agent_answer_preview.clone(),
        agent_answer_preview_chars: row.agent_answer_preview_chars,
        agent_answer_total_chars: row.agent_answer_total_chars,
        matched_paths: BTreeSet::new(),
    }
}

/// Records one path match when the shared glob matcher accepts the candidate row.
fn record_glob_path_match(
    accumulator: &mut FileSearchHitAccumulator,
    row: FilePathGlobRow,
    pattern: &Pattern,
    options: &MatchOptions,
    project_root: Option<&Path>,
) {
    if path_matches_glob(
        pattern,
        options,
        project_root,
        row.repo_relative_path.as_deref(),
        &row.path,
    ) {
        accumulator.matched_paths.insert(row.matched_path);
    }
}

/// Pushes a grouped glob hit only when at least one path survived glob verification.
fn push_glob_hit_if_matched(
    grouped: &mut Vec<SearchTurnHit>,
    accumulator: Option<FileSearchHitAccumulator>,
) {
    if let Some(accumulator) = accumulator.filter(|value| !value.matched_paths.is_empty()) {
        grouped.push(finalize_file_search_hit(accumulator));
    }
}

/// Builds the SQL for one candidate-turn page used by glob path search.
fn build_file_path_glob_turn_batch_sql(path_selector: &PathQuerySelector) -> String {
    let path_filter = match path_selector {
        PathQuerySelector::Exact { .. } | PathQuerySelector::Prefix { .. } => {
            format!(
                "\n                AND {}",
                path_selector.sql_predicate(8, 9)
            )
        }
        PathQuerySelector::Unbounded => String::new(),
        PathQuerySelector::Impossible => unreachable!("impossible selector is handled by caller"),
    };
    let final_path_filter = match path_selector {
        PathQuerySelector::Exact { .. } | PathQuerySelector::Prefix { .. } => {
            format!("\n            AND {}", path_selector.sql_predicate(8, 9))
        }
        PathQuerySelector::Unbounded => String::new(),
        PathQuerySelector::Impossible => unreachable!("impossible selector is handled by caller"),
    };
    let matched_path = "COALESCE(file_accesses.repo_relative_path, file_accesses.path)";

    format!(
        "
        WITH candidate_turns AS (
            SELECT
                turns.provider,
                turns.session_id,
                turns.turn_ordinal,
                turns.started_at,
                turns.completed_at,
                turns.status,
                turns.user_message,
                turns.final_answer_text
            FROM file_accesses
            INNER JOIN turns
                ON turns.project_id = file_accesses.project_id
                AND turns.provider = file_accesses.provider
                AND turns.session_id = file_accesses.session_id
                AND turns.turn_ordinal = file_accesses.turn_ordinal
            WHERE file_accesses.project_id = ?1
                AND (?2 IS NULL OR file_accesses.provider = ?2)
                AND (?3 IS NULL OR file_accesses.session_id = ?3)
                AND (?4 IS NULL OR julianday(turns.started_at) >= julianday(?4))
                AND (?5 IS NULL OR julianday(turns.started_at) < julianday(?5))
                AND NULLIF(TRIM(file_accesses.path), '') IS NOT NULL{path_filter}
            GROUP BY
                turns.provider,
                turns.session_id,
                turns.turn_ordinal,
                turns.started_at,
                turns.completed_at,
                turns.status,
                turns.user_message,
                turns.final_answer_text
            ORDER BY
                turns.started_at DESC,
                turns.provider ASC,
                turns.session_id ASC,
                turns.turn_ordinal ASC
            LIMIT ?6 OFFSET ?7
        )
        SELECT
            candidate_turns.provider,
            candidate_turns.session_id,
            candidate_turns.turn_ordinal,
            candidate_turns.started_at,
            candidate_turns.completed_at,
            candidate_turns.status,
            candidate_turns.user_message,
            candidate_turns.final_answer_text,
            file_accesses.repo_relative_path,
            file_accesses.path,
            {matched_path} AS matched_path
        FROM candidate_turns
        INNER JOIN file_accesses
            ON file_accesses.project_id = ?1
            AND file_accesses.provider = candidate_turns.provider
            AND file_accesses.session_id = candidate_turns.session_id
            AND file_accesses.turn_ordinal = candidate_turns.turn_ordinal
        WHERE NULLIF(TRIM(file_accesses.path), '') IS NOT NULL{final_path_filter}
        ORDER BY
            candidate_turns.started_at DESC,
            candidate_turns.provider ASC,
            candidate_turns.session_id ASC,
            candidate_turns.turn_ordinal ASC,
            matched_path COLLATE NOCASE ASC
        "
    )
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
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
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
                final_answer_text,
                matched_path,
            )| {
                let user_prompt_preview = preview_text(&user_message);
                let agent_answer_preview =
                    optional_agent_answer_preview(final_answer_text.as_deref());
                Ok(FileSearchRow {
                    provider: parse_provider(&provider)?,
                    session_id,
                    turn_ordinal: sql_count_to_u64(turn_ordinal)?,
                    started_at,
                    completed_at,
                    status: parse_turn_status(&status)?,
                    user_prompt_preview: user_prompt_preview.text,
                    user_prompt_preview_chars: user_prompt_preview.chars,
                    user_prompt_total_chars: user_prompt_preview.total_chars,
                    agent_answer_preview: agent_answer_preview
                        .as_ref()
                        .map(|preview| preview.text.clone()),
                    agent_answer_preview_chars: agent_answer_preview
                        .as_ref()
                        .map(|preview| preview.chars),
                    agent_answer_total_chars: agent_answer_preview
                        .as_ref()
                        .map(|preview| preview.total_chars),
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
        user_prompt_preview: row.user_prompt_preview,
        user_prompt_preview_chars: row.user_prompt_preview_chars,
        user_prompt_total_chars: row.user_prompt_total_chars,
        agent_answer_preview: row.agent_answer_preview,
        agent_answer_preview_chars: row.agent_answer_preview_chars,
        agent_answer_total_chars: row.agent_answer_total_chars,
        matched_paths,
    }
}

/// Finalizes one grouped file-search hit after every matching path has been collected.
fn finalize_file_search_hit(accumulator: FileSearchHitAccumulator) -> SearchTurnHit {
    let matched_paths = accumulator.matched_paths.into_iter().collect::<Vec<_>>();
    let matched_paths_count = u64::try_from(matched_paths.len()).unwrap_or(u64::MAX);
    SearchTurnHit {
        provider: accumulator.provider,
        session_id: accumulator.session_id,
        turn_ordinal: accumulator.turn_ordinal,
        started_at: accumulator.started_at,
        completed_at: accumulator.completed_at,
        status: accumulator.status,
        user_prompt_preview: accumulator.user_prompt_preview,
        user_prompt_preview_chars: accumulator.user_prompt_preview_chars,
        user_prompt_total_chars: accumulator.user_prompt_total_chars,
        agent_answer_preview: accumulator.agent_answer_preview,
        agent_answer_preview_chars: accumulator.agent_answer_preview_chars,
        agent_answer_total_chars: accumulator.agent_answer_total_chars,
        snippet: None,
        matched_paths,
        matched_paths_count,
        matched_paths_truncated: false,
        matches: Vec::new(),
        matches_count: 0,
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
                turns.user_message,
                turns.final_answer_text
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
                turns.user_message,
                turns.final_answer_text
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
            matched_turns.final_answer_text,
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
    let fields = EvidenceFieldSelection::from_request(SearchMode::Literal, false, &[], &[])?;
    connection
        .prepare(KEYWORD_SEARCH_SQL)
        .context("failed to prepare keyword search query")?;
    connection
        .prepare(&evidence_search_turns_sql(&fields, false))
        .context("failed to prepare evidence turn search query")?;
    connection
        .prepare(&evidence_search_turns_sql(&fields, true))
        .context("failed to prepare literal evidence turn search query")?;
    connection
        .prepare(&turn_evidence_rows_sql(&fields, false))
        .context("failed to prepare turn evidence row query")?;
    connection
        .prepare(&turn_evidence_rows_sql(&fields, true))
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
    for path_selector in [
        PathQuerySelector::Exact {
            relative: "src/lib.rs".to_owned(),
            absolute: None,
        },
        PathQuerySelector::Prefix {
            relative_like: "src/%".to_owned(),
            absolute_like: None,
        },
        PathQuerySelector::Unbounded,
    ] {
        connection
            .prepare(&build_file_path_glob_turn_batch_sql(&path_selector))
            .context("failed to prepare file-path glob search query")?;
    }
    Ok(())
}
