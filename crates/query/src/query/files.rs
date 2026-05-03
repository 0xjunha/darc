use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    path::Path,
};

use anyhow::{Context, Result, bail};
use darc_paths::SourceKind;
use glob::{MatchOptions, Pattern};
use rusqlite::{Connection, params_from_iter, types::Value};

use super::{
    FilePivotSummary, FileSessionSummary, FilesQueryData, FilesQueryMode, FilesQueryRequest,
    SessionFileSummary, SessionFilesQueryData, SessionFilesQueryRequest, SessionSummary,
    apply_matched_path_limit, open_existing_index_database, paginate_ranked_rows, parse_provider,
    sql_count_to_u64,
};

const MAX_SESSION_KEYS_PER_QUERY: usize = 250;

/// Stores one stable session identity used while intersecting file-touch filters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SessionKey {
    provider: SourceKind,
    session_id: String,
}

/// Stores one session identity selected by an indexed touched-path lookup.
#[derive(Debug, Clone)]
pub(crate) struct TouchedSessionKey {
    pub(crate) provider: SourceKind,
    pub(crate) session_id: String,
}

/// Stores filters for one exact touched-path session-page lookup.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TouchedSessionPageRequest<'a> {
    pub(crate) project_id: &'a str,
    pub(crate) project_root: Option<&'a Path>,
    pub(crate) provider: Option<SourceKind>,
    pub(crate) since: Option<&'a str>,
    pub(crate) until: Option<&'a str>,
    pub(crate) touched_path: &'a str,
    pub(crate) limit: usize,
    pub(crate) offset: usize,
}

/// Stores one grouped raw file-touch row before canonicalization.
#[derive(Debug, Clone)]
struct RawSessionFileRow {
    provider: SourceKind,
    session_id: String,
    repo_relative_path: Option<String>,
    path: String,
    read_count: u64,
    write_count: u64,
    first_turn_ordinal: u64,
    last_turn_ordinal: u64,
    first_touched_at: String,
    last_touched_at: String,
}

/// Stores one canonical per-session file summary after path normalization.
#[derive(Debug, Clone)]
struct AggregatedSessionFileRow {
    provider: SourceKind,
    session_id: String,
    path: String,
    repo_relative_path: Option<String>,
    read_count: u64,
    write_count: u64,
    first_turn_ordinal: u64,
    last_turn_ordinal: u64,
    first_touched_at: String,
    last_touched_at: String,
}

/// Stores one in-progress session ranking row while combining matched files.
#[derive(Debug, Clone)]
struct FileSessionAccumulator {
    touch_count: u64,
    read_count: u64,
    write_count: u64,
    first_turn_ordinal: u64,
    last_turn_ordinal: u64,
    first_touched_at: String,
    last_touched_at: String,
    matched_paths: BTreeSet<String>,
}

/// Stores one in-progress project-wide file ranking row.
#[derive(Debug, Clone)]
struct TouchedFileAccumulator {
    session_keys: BTreeSet<SessionKey>,
    read_count: u64,
    write_count: u64,
    first_touched_at: String,
    last_touched_at: String,
}

/// Stores the supported path-selector plans used to narrow file-access queries in SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathQuerySelector {
    Exact {
        relative: String,
        absolute: Option<String>,
    },
    Prefix {
        relative_like: String,
        absolute_like: Option<String>,
    },
    Unbounded,
    Impossible,
}

/// Stores the supported filters for one grouped file-access query.
#[derive(Debug, Clone, Copy)]
struct SessionFileQueryFilters<'a> {
    provider: Option<SourceKind>,
    session_id: Option<&'a str>,
    since: Option<&'a str>,
    until: Option<&'a str>,
    path_selector: Option<&'a PathQuerySelector>,
}

/// Stores one raw SQLite tuple returned by grouped file-access queries before normalization.
type RawSessionFileSqlRow = (
    String,
    String,
    Option<String>,
    String,
    i64,
    i64,
    i64,
    i64,
    String,
    String,
);

/// Stores one ungrouped touched-path file row before final glob verification.
type TouchedPathFileRow = (SourceKind, String, Option<String>, String);

impl PathQuerySelector {
    /// Returns the vetted SQL predicate for one path-selector plan.
    pub(crate) fn sql_predicate(&self, relative_param: usize, absolute_param: usize) -> String {
        match self {
            Self::Exact { .. } => format!(
                "(
                    file_accesses.repo_relative_path = ?{relative_param} COLLATE NOCASE
                    OR file_accesses.path = ?{relative_param} COLLATE NOCASE
                    OR (?{absolute_param} IS NOT NULL AND file_accesses.path = ?{absolute_param} COLLATE NOCASE)
                )"
            ),
            Self::Prefix { .. } => format!(
                "(
                    file_accesses.repo_relative_path LIKE ?{relative_param} ESCAPE '!' COLLATE NOCASE
                    OR file_accesses.path LIKE ?{relative_param} ESCAPE '!' COLLATE NOCASE
                    OR (?{absolute_param} IS NOT NULL AND file_accesses.path LIKE ?{absolute_param} ESCAPE '!' COLLATE NOCASE)
                )"
            ),
            Self::Unbounded | Self::Impossible => {
                unreachable!("only bounded path selectors emit SQL predicates")
            }
        }
    }

    /// Returns the SQLite parameter tail for one path-selector plan.
    pub(crate) fn params(&self) -> Vec<Value> {
        match self {
            Self::Exact { relative, absolute } => {
                vec![
                    Value::Text(relative.clone()),
                    absolute
                        .as_ref()
                        .map_or(Value::Null, |value| Value::Text(value.clone())),
                ]
            }
            Self::Prefix {
                relative_like,
                absolute_like,
            } => {
                vec![
                    Value::Text(relative_like.clone()),
                    absolute_like
                        .as_ref()
                        .map_or(Value::Null, |value| Value::Text(value.clone())),
                ]
            }
            Self::Unbounded | Self::Impossible => Vec::new(),
        }
    }
}

/// Queries one file-pivot payload from the indexed file-access tables.
pub fn query_project_files(
    index_db_path: &Path,
    request: FilesQueryRequest<'_>,
) -> Result<FilesQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    build_files_query(&connection, request)
}

/// Queries one session-scoped per-file summary payload from indexed file accesses.
pub fn query_project_session_files(
    index_db_path: &Path,
    request: SessionFilesQueryRequest<'_>,
) -> Result<SessionFilesQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    build_session_files_query(&connection, request)
}

/// Filters one session-summary candidate batch to sessions that touched a glob-matched file path.
pub(crate) fn filter_session_summaries_by_touched_path(
    connection: &Connection,
    project_id: &str,
    project_root: Option<&Path>,
    sessions: Vec<SessionSummary>,
    touched_path: &str,
) -> Result<Vec<SessionSummary>> {
    if sessions.is_empty() {
        return Ok(sessions);
    }

    let touched_path = normalize_query_path_pattern(project_root, touched_path);
    let pattern = Pattern::new(&touched_path)
        .with_context(|| format!("invalid touched-path glob `{touched_path}`"))?;
    let path_selector = build_path_query_selector(project_root, &touched_path);
    let session_keys = sessions
        .iter()
        .map(|session| SessionKey {
            provider: session.provider,
            session_id: session.session_id.clone(),
        })
        .collect::<Vec<_>>();
    let rows = query_touched_path_file_rows(connection, project_id, &session_keys, &path_selector)?;
    let match_options = glob_match_options();
    let mut matching_sessions = BTreeSet::<SessionKey>::new();
    for (provider, session_id, repo_relative_path, path) in rows {
        if path_matches_glob(
            &pattern,
            &match_options,
            project_root,
            repo_relative_path.as_deref(),
            &path,
        ) {
            matching_sessions.insert(SessionKey {
                provider,
                session_id,
            });
        }
    }
    Ok(sessions
        .into_iter()
        .filter(|session| {
            matching_sessions.contains(&SessionKey {
                provider: session.provider,
                session_id: session.session_id.clone(),
            })
        })
        .collect())
}

/// Queries one exact touched-path session page directly from indexed file accesses.
pub(crate) fn query_exact_touched_session_page(
    connection: &Connection,
    request: TouchedSessionPageRequest<'_>,
) -> Result<Option<(Vec<TouchedSessionKey>, bool)>> {
    let touched_path = normalize_query_path_pattern(request.project_root, request.touched_path);
    let path_selector = build_path_query_selector(request.project_root, &touched_path);
    if !matches!(path_selector, PathQuerySelector::Exact { .. }) {
        return Ok(None);
    }

    let page_limit = request
        .limit
        .checked_add(1)
        .context("query limit exceeds usize range")?;
    let rows = query_exact_touched_session_keys(
        connection,
        TouchedSessionPageRequest {
            limit: page_limit,
            ..request
        },
        &path_selector,
    )?;
    let has_more = rows.len() > request.limit;
    let sessions = rows.into_iter().take(request.limit).collect::<Vec<_>>();
    Ok(Some((sessions, has_more)))
}

/// Queries exact touched-path session keys with SQL-side pagination.
fn query_exact_touched_session_keys(
    connection: &Connection,
    request: TouchedSessionPageRequest<'_>,
    path_selector: &PathQuerySelector,
) -> Result<Vec<TouchedSessionKey>> {
    let provider = request.provider.map(SourceKind::directory_name);
    let mut params = vec![
        Value::Text(request.project_id.to_owned()),
        optional_text_value(provider),
        optional_text_value(request.since),
        optional_text_value(request.until),
    ];
    params.extend(path_selector.params());
    params.push(Value::Integer(
        i64::try_from(request.limit).context("query limit exceeds SQLite INTEGER range")?,
    ));
    params.push(Value::Integer(
        i64::try_from(request.offset).context("query offset exceeds SQLite INTEGER range")?,
    ));

    let sql = build_exact_touched_session_page_sql(path_selector);
    let mut statement = connection
        .prepare(&sql)
        .context("failed to prepare exact touched-path session query")?;
    statement
        .query_map(params_from_iter(params), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("failed to query exact touched-path sessions")?
        .map(|row| {
            let (provider, session_id) = row.context("failed to read touched-path session row")?;
            Ok(TouchedSessionKey {
                provider: parse_provider(&provider)?,
                session_id,
            })
        })
        .collect()
}

/// Builds the SQL for one exact touched-path session page.
fn build_exact_touched_session_page_sql(path_selector: &PathQuerySelector) -> String {
    let path_filter = path_selector.sql_predicate(5, 6);
    format!(
        "
    WITH touched_sessions AS (
        SELECT DISTINCT
            file_accesses.provider,
            file_accesses.session_id
        FROM file_accesses
        WHERE file_accesses.project_id = ?1
            AND (?2 IS NULL OR file_accesses.provider = ?2)
            AND file_accesses.access_type IN ('read', 'write', 'edit')
            AND NULLIF(TRIM(file_accesses.path), '') IS NOT NULL
            AND {path_filter}
    ),
    latest_session_turns AS (
        SELECT
            turns.provider,
            turns.session_id,
            MAX(turns.started_at) AS latest_turn_at
        FROM turns
        INNER JOIN touched_sessions
            ON touched_sessions.provider = turns.provider
            AND touched_sessions.session_id = turns.session_id
        WHERE turns.project_id = ?1
        GROUP BY turns.provider, turns.session_id
    )
    SELECT
        touched_sessions.provider,
        touched_sessions.session_id
    FROM touched_sessions
    LEFT JOIN latest_session_turns
        ON latest_session_turns.provider = touched_sessions.provider
        AND latest_session_turns.session_id = touched_sessions.session_id
    WHERE (?3 IS NULL OR julianday(latest_session_turns.latest_turn_at) >= julianday(?3))
        AND (?4 IS NULL OR julianday(latest_session_turns.latest_turn_at) < julianday(?4))
    ORDER BY
        latest_session_turns.latest_turn_at IS NULL ASC,
        latest_session_turns.latest_turn_at DESC,
        touched_sessions.provider ASC,
        touched_sessions.session_id DESC
    LIMIT ?7 OFFSET ?8
"
    )
}

/// Queries touched-path candidate file rows for one bounded session candidate set.
fn query_touched_path_file_rows(
    connection: &Connection,
    project_id: &str,
    session_keys: &[SessionKey],
    path_selector: &PathQuerySelector,
) -> Result<Vec<TouchedPathFileRow>> {
    if session_keys.is_empty() || matches!(path_selector, PathQuerySelector::Impossible) {
        return Ok(Vec::new());
    }

    let mut rows = Vec::new();
    for session_chunk in session_keys.chunks(MAX_SESSION_KEYS_PER_QUERY) {
        let sql = build_touched_path_file_rows_sql(session_chunk.len(), path_selector);
        let mut params = build_session_key_values_params(project_id, session_chunk.iter())?;
        if matches!(
            path_selector,
            PathQuerySelector::Exact { .. } | PathQuerySelector::Prefix { .. }
        ) {
            params.extend(path_selector.params());
        }
        let mut statement = connection
            .prepare(&sql)
            .context("failed to prepare touched-path file row query")?;
        rows.extend(
            statement
                .query_map(params_from_iter(params), read_touched_path_file_row)
                .context("failed to query touched-path file rows")?
                .map(|row| {
                    let (provider, session_id, repo_relative_path, path) =
                        row.context("failed to read touched-path file row")?;
                    Ok((provider, session_id, repo_relative_path, path))
                })
                .collect::<Result<Vec<_>>>()?,
        );
    }
    Ok(rows)
}

/// Builds one requested-session file-row query with optional path selector narrowing.
fn build_touched_path_file_rows_sql(row_count: usize, path_selector: &PathQuerySelector) -> String {
    let relative_param = row_count
        .checked_mul(2)
        .and_then(|value| value.checked_add(2))
        .expect("placeholder index should stay within usize range");
    let absolute_param = relative_param
        .checked_add(1)
        .expect("placeholder index should stay within usize range");
    let path_filter = match path_selector {
        PathQuerySelector::Exact { .. } | PathQuerySelector::Prefix { .. } => {
            format!(
                "\n            AND {}",
                path_selector.sql_predicate(relative_param, absolute_param)
            )
        }
        PathQuerySelector::Unbounded => String::new(),
        PathQuerySelector::Impossible => unreachable!("impossible selector is handled by caller"),
    };
    build_session_key_values_query_sql(
        row_count,
        &format!(
            "
        SELECT DISTINCT
            file_accesses.provider,
            file_accesses.session_id,
            file_accesses.repo_relative_path,
            file_accesses.path
        FROM requested
        INNER JOIN file_accesses
            ON file_accesses.project_id = ?1
            AND file_accesses.provider = requested.provider
            AND file_accesses.session_id = requested.session_id
        WHERE NULLIF(TRIM(file_accesses.path), '') IS NOT NULL{path_filter}
        ORDER BY
            file_accesses.provider ASC,
            file_accesses.session_id ASC,
            COALESCE(file_accesses.repo_relative_path, file_accesses.path) COLLATE NOCASE ASC
        "
        ),
    )
}

/// Reads one distinct session/path row from SQLite.
fn read_touched_path_file_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TouchedPathFileRow> {
    let provider = row.get::<_, String>(0)?;
    let provider = parse_provider(&provider).map_err(to_rusqlite_error)?;
    Ok((
        provider,
        row.get(1)?,
        row.get::<_, Option<String>>(2)?,
        row.get(3)?,
    ))
}

/// Converts one query-layer error into a rusqlite callback error.
fn to_rusqlite_error(error: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error.to_string(),
        )),
    )
}

/// Normalizes one path query so absolute project-root inputs match stored repo-relative paths.
pub(crate) fn normalize_query_path_pattern(project_root: Option<&Path>, path: &str) -> String {
    let normalized = normalize_path_literal(path);
    project_root
        .and_then(|project_root| strip_project_root(&normalized, project_root))
        .filter(|value| !value.is_empty())
        .unwrap_or(normalized)
}

/// Returns the stable glob-match options shared by every touched-path filter.
pub(crate) fn glob_match_options() -> MatchOptions {
    MatchOptions {
        case_sensitive: false,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    }
}

/// Returns whether one indexed file access matches one project-scoped query glob.
pub(crate) fn path_matches_glob(
    pattern: &Pattern,
    options: &MatchOptions,
    project_root: Option<&Path>,
    repo_relative_path: Option<&str>,
    path: &str,
) -> bool {
    candidate_query_paths(project_root, repo_relative_path, path)
        .iter()
        .any(|candidate| pattern.matches_with(candidate, *options))
}

/// Builds one validated file-pivot payload from the indexed file-access tables.
fn build_files_query(
    connection: &Connection,
    request: FilesQueryRequest<'_>,
) -> Result<FilesQueryData> {
    let path = optional_non_empty_file_selector("PATH/--path", request.path)?;
    let co_touched_with =
        optional_non_empty_file_selector("--co-touched-with", request.co_touched_with)?;
    match (path, co_touched_with) {
        (None, None) => {
            let (files, has_more) = query_top_touched_files(connection, request)?;
            Ok(FilesQueryData {
                project_id: request.project_id.to_owned(),
                mode: FilesQueryMode::Top,
                provider: request.provider,
                path: None,
                co_touched_with: None,
                since: request.since.map(str::to_owned),
                until: request.until.map(str::to_owned),
                limit: u64::try_from(request.limit).context("query limit exceeds u64 range")?,
                offset: u64::try_from(request.offset).context("query offset exceeds u64 range")?,
                has_more,
                matched_path_limit: None,
                sessions: Vec::new(),
                files,
            })
        }
        (Some(path), None) => {
            let sessions = query_file_session_matches(
                connection,
                request.project_id,
                request.project_root,
                request.provider,
                path,
                request.since,
                request.until,
            )?;
            let (sessions, has_more) =
                paginate_ranked_rows(sessions, request.limit, request.offset)?;
            let sessions =
                apply_file_session_matched_path_limit(sessions, request.matched_path_limit);
            Ok(FilesQueryData {
                project_id: request.project_id.to_owned(),
                mode: FilesQueryMode::Path,
                provider: request.provider,
                path: Some(path.to_owned()),
                co_touched_with: None,
                since: request.since.map(str::to_owned),
                until: request.until.map(str::to_owned),
                limit: u64::try_from(request.limit).context("query limit exceeds u64 range")?,
                offset: u64::try_from(request.offset).context("query offset exceeds u64 range")?,
                has_more,
                matched_path_limit: request
                    .matched_path_limit
                    .map(u64::try_from)
                    .transpose()
                    .context("matched path limit exceeds u64 range")?,
                sessions,
                files: Vec::new(),
            })
        }
        (None, Some(seed_path)) => {
            let (files, has_more) = query_co_touched_files(
                connection,
                CoTouchedFilePageRequest {
                    project_id: request.project_id,
                    project_root: request.project_root,
                    provider: request.provider,
                    since: request.since,
                    until: request.until,
                    seed_path,
                    limit: request.limit,
                    offset: request.offset,
                },
            )?;
            Ok(FilesQueryData {
                project_id: request.project_id.to_owned(),
                mode: FilesQueryMode::CoTouchedWith,
                provider: request.provider,
                path: None,
                co_touched_with: Some(seed_path.to_owned()),
                since: request.since.map(str::to_owned),
                until: request.until.map(str::to_owned),
                limit: u64::try_from(request.limit).context("query limit exceeds u64 range")?,
                offset: u64::try_from(request.offset).context("query offset exceeds u64 range")?,
                has_more,
                matched_path_limit: None,
                sessions: Vec::new(),
                files,
            })
        }
        (Some(_), Some(_)) => {
            bail!("file query requires exactly one of --path or --co-touched-with")
        }
    }
}

/// Trims one optional selector while rejecting explicitly empty input.
fn optional_non_empty_file_selector<'a>(
    label: &str,
    value: Option<&'a str>,
) -> Result<Option<&'a str>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        bail!("{label} cannot be empty");
    }
    Ok(Some(value))
}

/// Applies the matched-path preview cap to each file-session row.
fn apply_file_session_matched_path_limit(
    sessions: Vec<FileSessionSummary>,
    matched_path_limit: Option<usize>,
) -> Vec<FileSessionSummary> {
    sessions
        .into_iter()
        .map(|mut session| {
            session.matched_paths_count =
                u64::try_from(session.matched_paths.len()).unwrap_or(u64::MAX);
            let (matched_paths, matched_paths_truncated) =
                apply_matched_path_limit(session.matched_paths, matched_path_limit);
            session.matched_paths = matched_paths;
            session.matched_paths_truncated = matched_paths_truncated;
            session
        })
        .collect()
}

/// Builds one session-scoped per-file summary payload from canonicalized file touches.
pub(crate) fn build_session_files_query(
    connection: &Connection,
    request: SessionFilesQueryRequest<'_>,
) -> Result<SessionFilesQueryData> {
    let rows = query_raw_session_file_rows(
        connection,
        request.project_id,
        SessionFileQueryFilters {
            provider: Some(request.provider),
            session_id: Some(request.session_id),
            since: None,
            until: None,
            path_selector: None,
        },
    )?;
    let mut files = aggregate_session_file_rows(rows, request.project_root)
        .into_iter()
        .map(|row| SessionFileSummary {
            path: row.path,
            repo_relative_path: row.repo_relative_path,
            read_count: row.read_count,
            write_count: row.write_count,
            first_turn_ordinal: row.first_turn_ordinal,
            last_turn_ordinal: row.last_turn_ordinal,
        })
        .collect::<Vec<_>>();
    sort_session_file_summaries(&mut files);
    let file_count = u64::try_from(files.len()).context("session file count exceeds u64 range")?;
    let (files, has_more) = paginate_ranked_rows(files, request.limit, request.offset)?;
    Ok(SessionFilesQueryData {
        project_id: request.project_id.to_owned(),
        provider: request.provider,
        session_id: request.session_id.to_owned(),
        file_count,
        limit: u64::try_from(request.limit).context("query limit exceeds u64 range")?,
        offset: u64::try_from(request.offset).context("query offset exceeds u64 range")?,
        has_more,
        files,
    })
}

/// Queries project-wide touched files ranked by access frequency.
fn query_top_touched_files(
    connection: &Connection,
    request: FilesQueryRequest<'_>,
) -> Result<(Vec<FilePivotSummary>, bool)> {
    let mut files = BTreeMap::<String, TouchedFileAccumulator>::new();
    for_each_raw_session_file_row(
        connection,
        request.project_id,
        SessionFileQueryFilters {
            provider: request.provider,
            session_id: None,
            since: request.since,
            until: request.until,
            path_selector: None,
        },
        |row| {
            let Some(path) = display_path_for_access(
                request.project_root,
                row.repo_relative_path.as_deref(),
                &row.path,
            ) else {
                return Ok(());
            };
            let key = SessionKey {
                provider: row.provider,
                session_id: row.session_id.clone(),
            };
            files
                .entry(path)
                .and_modify(|file| {
                    file.session_keys.insert(key.clone());
                    file.read_count = file.read_count.saturating_add(row.read_count);
                    file.write_count = file.write_count.saturating_add(row.write_count);
                    if row.first_touched_at < file.first_touched_at {
                        file.first_touched_at = row.first_touched_at.clone();
                    }
                    if row.last_touched_at > file.last_touched_at {
                        file.last_touched_at = row.last_touched_at.clone();
                    }
                })
                .or_insert_with(|| TouchedFileAccumulator {
                    session_keys: BTreeSet::from([key]),
                    read_count: row.read_count,
                    write_count: row.write_count,
                    first_touched_at: row.first_touched_at.clone(),
                    last_touched_at: row.last_touched_at.clone(),
                });
            Ok(())
        },
    )?;

    let mut files = files
        .into_iter()
        .map(|(path, file)| {
            let touch_count = file.read_count.saturating_add(file.write_count);
            FilePivotSummary {
                path,
                co_touch_count: None,
                touch_count: Some(touch_count),
                session_count: Some(
                    u64::try_from(file.session_keys.len())
                        .expect("session count should fit into u64"),
                ),
                read_count: Some(file.read_count),
                write_count: Some(file.write_count),
                first_touched_at: Some(file.first_touched_at),
                last_touched_at: Some(file.last_touched_at),
            }
        })
        .collect::<Vec<_>>();
    paginate_top_touched_files(&mut files, request.limit, request.offset)
}

/// Applies most-touched pagination after sorting only the prefix needed for the requested page.
fn paginate_top_touched_files(
    files: &mut Vec<FilePivotSummary>,
    limit: usize,
    offset: usize,
) -> Result<(Vec<FilePivotSummary>, bool)> {
    let page_end = offset
        .checked_add(limit)
        .context("query pagination exceeds usize range")?;
    let has_more = files.len() > page_end;
    let sort_len = page_end.min(files.len());
    if sort_len == 0 {
        files.clear();
    } else if sort_len < files.len() {
        files.select_nth_unstable_by(sort_len, compare_top_touched_files);
        files.truncate(sort_len);
    }
    files.sort_by(compare_top_touched_files);
    Ok((files.drain(..).skip(offset).take(limit).collect(), has_more))
}

/// Compares most-touched rows by rank descending and path ascending.
fn compare_top_touched_files(left: &FilePivotSummary, right: &FilePivotSummary) -> Ordering {
    right
        .touch_count
        .cmp(&left.touch_count)
        .then_with(|| right.session_count.cmp(&left.session_count))
        .then_with(|| right.last_touched_at.cmp(&left.last_touched_at))
        .then_with(|| left.path.cmp(&right.path))
}

/// Queries one file-to-session pivot ranked by descending touch frequency.
fn query_file_session_matches(
    connection: &Connection,
    project_id: &str,
    project_root: Option<&Path>,
    provider: Option<SourceKind>,
    path: &str,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<FileSessionSummary>> {
    let path = normalize_query_path_pattern(project_root, path);
    let pattern = Pattern::new(&path).with_context(|| format!("invalid path glob `{path}`"))?;
    let path_selector = build_path_query_selector(project_root, &path);
    let match_options = glob_match_options();
    let rows = query_raw_session_file_rows(
        connection,
        project_id,
        SessionFileQueryFilters {
            provider,
            session_id: None,
            since,
            until,
            path_selector: Some(&path_selector),
        },
    )?;
    let aggregates = aggregate_session_file_rows(
        rows.into_iter()
            .filter(|row| {
                path_matches_glob(
                    &pattern,
                    &match_options,
                    project_root,
                    row.repo_relative_path.as_deref(),
                    &row.path,
                )
            })
            .collect(),
        project_root,
    );

    let mut sessions = BTreeMap::<SessionKey, FileSessionAccumulator>::new();
    for row in aggregates {
        let key = SessionKey {
            provider: row.provider,
            session_id: row.session_id.clone(),
        };
        let touch_count = row.read_count.saturating_add(row.write_count);
        sessions
            .entry(key)
            .and_modify(|session| {
                session.touch_count = session.touch_count.saturating_add(touch_count);
                session.read_count = session.read_count.saturating_add(row.read_count);
                session.write_count = session.write_count.saturating_add(row.write_count);
                session.first_turn_ordinal = session.first_turn_ordinal.min(row.first_turn_ordinal);
                session.last_turn_ordinal = session.last_turn_ordinal.max(row.last_turn_ordinal);
                if row.first_touched_at < session.first_touched_at {
                    session.first_touched_at = row.first_touched_at.clone();
                }
                if row.last_touched_at > session.last_touched_at {
                    session.last_touched_at = row.last_touched_at.clone();
                }
                session.matched_paths.insert(row.path.clone());
            })
            .or_insert_with(|| FileSessionAccumulator {
                touch_count,
                read_count: row.read_count,
                write_count: row.write_count,
                first_turn_ordinal: row.first_turn_ordinal,
                last_turn_ordinal: row.last_turn_ordinal,
                first_touched_at: row.first_touched_at.clone(),
                last_touched_at: row.last_touched_at.clone(),
                matched_paths: BTreeSet::from([row.path]),
            });
    }

    let mut sessions = sessions
        .into_iter()
        .map(|(key, session)| {
            let matched_paths = session.matched_paths.into_iter().collect::<Vec<_>>();
            let matched_paths_count = u64::try_from(matched_paths.len()).unwrap_or(u64::MAX);
            FileSessionSummary {
                provider: key.provider,
                session_id: key.session_id,
                touch_count: session.touch_count,
                read_count: session.read_count,
                write_count: session.write_count,
                first_turn_ordinal: session.first_turn_ordinal,
                last_turn_ordinal: session.last_turn_ordinal,
                first_touched_at: session.first_touched_at,
                last_touched_at: session.last_touched_at,
                matched_paths,
                matched_paths_count,
                matched_paths_truncated: false,
            }
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        right
            .touch_count
            .cmp(&left.touch_count)
            .then_with(|| right.last_touched_at.cmp(&left.last_touched_at))
            .then_with(|| left.provider.cmp(&right.provider))
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    Ok(sessions)
}

/// Queries files that co-occur with one seed path in the same sessions.
fn query_co_touched_files(
    connection: &Connection,
    request: CoTouchedFilePageRequest<'_>,
) -> Result<(Vec<FilePivotSummary>, bool)> {
    let seed_path = normalize_project_scoped_query_path(request.project_root, request.seed_path);
    let Some(seed_path) = seed_path else {
        return Ok((Vec::new(), false));
    };
    let page_limit = request
        .limit
        .checked_add(1)
        .context("query limit exceeds usize range")?;
    let rows = query_co_touched_file_page(
        connection,
        CoTouchedFilePageRequest {
            project_id: request.project_id,
            project_root: request.project_root,
            provider: request.provider,
            since: request.since,
            until: request.until,
            seed_path: &seed_path,
            limit: page_limit,
            offset: request.offset,
        },
    )?;
    let has_more = rows.len() > request.limit;
    let files = rows
        .into_iter()
        .take(request.limit)
        .map(|(path, co_touch_count)| FilePivotSummary {
            path,
            co_touch_count: Some(co_touch_count),
            touch_count: None,
            session_count: None,
            read_count: None,
            write_count: None,
            first_touched_at: None,
            last_touched_at: None,
        })
        .collect::<Vec<_>>();
    Ok((files, has_more))
}

/// Stores one paginated co-touched file query.
struct CoTouchedFilePageRequest<'a> {
    project_id: &'a str,
    project_root: Option<&'a Path>,
    provider: Option<SourceKind>,
    since: Option<&'a str>,
    until: Option<&'a str>,
    seed_path: &'a str,
    limit: usize,
    offset: usize,
}

/// Queries one co-touched file page directly from distinct session/path rows.
fn query_co_touched_file_page(
    connection: &Connection,
    request: CoTouchedFilePageRequest<'_>,
) -> Result<Vec<(String, u64)>> {
    let selector = PathQuerySelector::Exact {
        relative: request.seed_path.to_owned(),
        absolute: absolute_project_path(request.project_root, request.seed_path),
    };
    let sql = build_co_touched_file_page_sql(&selector);
    let provider = request.provider.map(SourceKind::directory_name);
    let mut params = vec![
        Value::Text(request.project_id.to_owned()),
        optional_text_value(provider),
        optional_text_value(request.since),
        optional_text_value(request.until),
    ];
    params.extend(selector.params());
    params.extend(project_root_display_params(request.project_root));
    params.push(Value::Integer(
        i64::try_from(request.limit).context("query limit exceeds SQLite INTEGER range")?,
    ));
    params.push(Value::Integer(
        i64::try_from(request.offset).context("query offset exceeds SQLite INTEGER range")?,
    ));

    let mut statement = connection
        .prepare(&sql)
        .context("failed to prepare co-touched file query")?;
    statement
        .query_map(params_from_iter(params), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .context("failed to query co-touched files")?
        .map(|row| {
            let (path, co_touch_count) = row.context("failed to read co-touched file row")?;
            Ok((path, sql_count_to_u64(co_touch_count)?))
        })
        .collect()
}

/// Builds one direct co-touched file page query.
fn build_co_touched_file_page_sql(seed_selector: &PathQuerySelector) -> String {
    let seed_path_filter = seed_selector.sql_predicate(5, 6);
    let display_path = sql_display_path_for_access("file_accesses");
    format!(
        "
    WITH seed_sessions AS (
        SELECT DISTINCT
            file_accesses.provider,
            file_accesses.session_id
        FROM file_accesses
        INNER JOIN turns AS seed_turns
            ON seed_turns.project_id = file_accesses.project_id
            AND seed_turns.provider = file_accesses.provider
            AND seed_turns.session_id = file_accesses.session_id
            AND seed_turns.turn_ordinal = file_accesses.turn_ordinal
        WHERE file_accesses.project_id = ?1
            AND (?2 IS NULL OR file_accesses.provider = ?2)
            AND (?3 IS NULL OR seed_turns.started_at >= ?3)
            AND (?4 IS NULL OR seed_turns.started_at < ?4)
            AND file_accesses.access_type IN ('read', 'write', 'edit')
            AND NULLIF(TRIM(file_accesses.path), '') IS NOT NULL
            AND {seed_path_filter}
    ),
    session_paths AS (
        SELECT DISTINCT
            file_accesses.provider,
            file_accesses.session_id,
            {display_path} AS display_path
        FROM seed_sessions
        INNER JOIN file_accesses
            ON file_accesses.project_id = ?1
            AND file_accesses.provider = seed_sessions.provider
            AND file_accesses.session_id = seed_sessions.session_id
        INNER JOIN turns
            ON turns.project_id = file_accesses.project_id
            AND turns.provider = file_accesses.provider
            AND turns.session_id = file_accesses.session_id
            AND turns.turn_ordinal = file_accesses.turn_ordinal
        WHERE file_accesses.access_type IN ('read', 'write', 'edit')
            AND NULLIF(TRIM(file_accesses.path), '') IS NOT NULL
            AND (?3 IS NULL OR turns.started_at >= ?3)
            AND (?4 IS NULL OR turns.started_at < ?4)
    )
    SELECT
        display_path,
        COUNT(*) AS co_touch_count
    FROM session_paths
    WHERE NULLIF(TRIM(display_path), '') IS NOT NULL
        AND display_path <> ?5 COLLATE NOCASE
    GROUP BY display_path
    ORDER BY
        co_touch_count DESC,
        display_path COLLATE NOCASE ASC
    LIMIT ?11 OFFSET ?12
"
    )
}

/// Builds the SQL expression matching Darc's project-root display path rules.
fn sql_display_path_for_access(table_alias: &str) -> String {
    let path = format!("TRIM({table_alias}.path)");
    format!(
        "CASE
            WHEN NULLIF(TRIM({table_alias}.repo_relative_path), '') IS NOT NULL
                THEN TRIM({table_alias}.repo_relative_path)
            WHEN ?7 IS NOT NULL AND {path} = ?7
                THEN ''
            WHEN ?7 IS NOT NULL AND {path} LIKE ?8 ESCAPE '!'
                THEN SUBSTR({path}, LENGTH(?7) + 2)
            WHEN ?9 IS NOT NULL AND {path} = ?9
                THEN ''
            WHEN ?9 IS NOT NULL AND {path} LIKE ?10 ESCAPE '!'
                THEN SUBSTR({path}, LENGTH(?9) + 2)
            ELSE NULL
        END"
    )
}

/// Builds project-root parameters used by SQL-side path display normalization.
fn project_root_display_params(project_root: Option<&Path>) -> Vec<Value> {
    let project_root = project_root.map(normalize_path_string);
    let project_root_like = project_root.as_deref().map(project_root_child_like_pattern);
    let private_root = project_root.as_deref().and_then(private_path_variant);
    let private_root_like = private_root.as_deref().map(project_root_child_like_pattern);
    [
        optional_owned_text_value(project_root),
        optional_owned_text_value(project_root_like),
        optional_owned_text_value(private_root),
        optional_owned_text_value(private_root_like),
    ]
    .into()
}

/// Returns a `LIKE` pattern matching direct descendants of one normalized root path.
fn project_root_child_like_pattern(root: &str) -> String {
    if root == "/" {
        "/%".to_owned()
    } else {
        format!("{}/%", escape_like_pattern(root))
    }
}

/// Returns the macOS `/private` alternate spelling for one normalized root path.
fn private_path_variant(path: &str) -> Option<String> {
    path.strip_prefix("/private")
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            (!path.starts_with("/private/") && path.starts_with('/'))
                .then(|| format!("/private{path}"))
        })
}

/// Converts optional owned string filters into SQLite values.
fn optional_owned_text_value(value: Option<String>) -> Value {
    value.map_or(Value::Null, Value::Text)
}

/// Converts optional borrowed string filters into SQLite values.
fn optional_text_value(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |value| Value::Text(value.to_owned()))
}

/// Queries grouped raw file rows for one provider/session/time selector set.
fn query_raw_session_file_rows(
    connection: &Connection,
    project_id: &str,
    filters: SessionFileQueryFilters<'_>,
) -> Result<Vec<RawSessionFileRow>> {
    let mut rows = Vec::new();
    for_each_raw_session_file_row(connection, project_id, filters, |row| {
        rows.push(row);
        Ok(())
    })?;
    Ok(rows)
}

/// Visits grouped raw file rows for one provider/session/time selector set.
fn for_each_raw_session_file_row<F>(
    connection: &Connection,
    project_id: &str,
    filters: SessionFileQueryFilters<'_>,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(RawSessionFileRow) -> Result<()>,
{
    if filters
        .path_selector
        .is_some_and(|selector| matches!(selector, PathQuerySelector::Impossible))
    {
        return Ok(());
    }

    let sql = build_session_file_rows_sql(filters.path_selector);
    let mut statement = connection
        .prepare(&sql)
        .context("failed to prepare session file rows query")?;
    let params = build_session_file_rows_params(project_id, filters)?;
    for row in statement
        .query_map(params_from_iter(params), read_raw_session_file_row)
        .context("failed to query session file rows")?
    {
        let row = row.context("failed to read session file row")?;
        visit(build_raw_session_file_row(row)?)?;
    }
    Ok(())
}

/// Reads one raw grouped file row from SQLite before type normalization.
fn read_raw_session_file_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawSessionFileSqlRow> {
    Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, Option<String>>(2)?,
        row.get::<_, String>(3)?,
        row.get::<_, i64>(4)?,
        row.get::<_, i64>(5)?,
        row.get::<_, i64>(6)?,
        row.get::<_, i64>(7)?,
        row.get::<_, String>(8)?,
        row.get::<_, String>(9)?,
    ))
}

/// Converts one raw SQLite grouped file row into the public raw file-row shape.
fn build_raw_session_file_row(row: RawSessionFileSqlRow) -> Result<RawSessionFileRow> {
    Ok(RawSessionFileRow {
        provider: parse_provider(&row.0)?,
        session_id: row.1,
        repo_relative_path: row.2,
        path: row.3,
        read_count: sql_count_to_u64(row.4)?,
        write_count: sql_count_to_u64(row.5)?,
        first_turn_ordinal: sql_count_to_u64(row.6)?,
        last_turn_ordinal: sql_count_to_u64(row.7)?,
        first_touched_at: row.8,
        last_touched_at: row.9,
    })
}

/// Collapses raw file rows onto one canonical display path per session.
fn aggregate_session_file_rows(
    rows: Vec<RawSessionFileRow>,
    project_root: Option<&Path>,
) -> Vec<AggregatedSessionFileRow> {
    let mut aggregates = BTreeMap::<(SourceKind, String, String), AggregatedSessionFileRow>::new();
    for row in rows {
        let Some(display_path) =
            display_path_for_access(project_root, row.repo_relative_path.as_deref(), &row.path)
        else {
            continue;
        };
        let key = (row.provider, row.session_id.clone(), display_path.clone());
        aggregates
            .entry(key)
            .and_modify(|aggregate| {
                aggregate.read_count = aggregate.read_count.saturating_add(row.read_count);
                aggregate.write_count = aggregate.write_count.saturating_add(row.write_count);
                aggregate.first_turn_ordinal =
                    aggregate.first_turn_ordinal.min(row.first_turn_ordinal);
                aggregate.last_turn_ordinal =
                    aggregate.last_turn_ordinal.max(row.last_turn_ordinal);
                if row.first_touched_at < aggregate.first_touched_at {
                    aggregate.first_touched_at = row.first_touched_at.clone();
                }
                if row.last_touched_at > aggregate.last_touched_at {
                    aggregate.last_touched_at = row.last_touched_at.clone();
                }
                if aggregate.repo_relative_path.is_none() && row.repo_relative_path.is_some() {
                    aggregate.repo_relative_path = row
                        .repo_relative_path
                        .as_deref()
                        .and_then(normalize_project_scoped_relative_path);
                }
            })
            .or_insert_with(|| AggregatedSessionFileRow {
                provider: row.provider,
                session_id: row.session_id,
                path: display_path,
                repo_relative_path: row
                    .repo_relative_path
                    .as_deref()
                    .and_then(normalize_project_scoped_relative_path),
                read_count: row.read_count,
                write_count: row.write_count,
                first_turn_ordinal: row.first_turn_ordinal,
                last_turn_ordinal: row.last_turn_ordinal,
                first_touched_at: row.first_touched_at,
                last_touched_at: row.last_touched_at,
            });
    }
    aggregates.into_values().collect()
}

/// Sorts session-scoped file summaries by access frequency then canonical path.
fn sort_session_file_summaries(files: &mut [SessionFileSummary]) {
    files.sort_by(|left, right| {
        let left_total = left.read_count.saturating_add(left.write_count);
        let right_total = right.read_count.saturating_add(right.write_count);
        right_total
            .cmp(&left_total)
            .then_with(|| right.write_count.cmp(&left.write_count))
            .then_with(|| right.read_count.cmp(&left.read_count))
            .then_with(|| left.path.cmp(&right.path))
    });
}

/// Returns the canonical candidate paths that one query glob should match against.
fn candidate_query_paths(
    project_root: Option<&Path>,
    repo_relative_path: Option<&str>,
    path: &str,
) -> Vec<String> {
    display_path_for_access(project_root, repo_relative_path, path)
        .into_iter()
        .collect()
}

/// Returns one canonical in-project display path for one indexed file access.
pub fn display_path_for_access(
    project_root: Option<&Path>,
    repo_relative_path: Option<&str>,
    path: &str,
) -> Option<String> {
    repo_relative_path
        .and_then(normalize_project_scoped_relative_path)
        .or_else(|| {
            project_root
                .and_then(|project_root| strip_project_root(path, project_root))
                .and_then(|value| normalize_project_scoped_relative_path(&value))
        })
}

/// Normalizes one exact query path into a project-scoped relative identity when possible.
fn normalize_project_scoped_query_path(project_root: Option<&Path>, path: &str) -> Option<String> {
    let path = normalize_query_path_pattern(project_root, path);
    normalize_project_scoped_relative_path(&path)
}

/// Normalizes one stored relative path while rejecting values that escape the project root.
fn normalize_project_scoped_relative_path(path: &str) -> Option<String> {
    let normalized = normalize_path_literal(path);
    (!normalized.is_empty()
        && !is_absolute_path_literal(&normalized)
        && normalized
            .split('/')
            .next()
            .is_none_or(|component| component != ".."))
    .then_some(normalized)
}

/// Builds the narrowest SQL selector Darc can derive from one normalized query pattern.
pub(crate) fn build_path_query_selector(
    project_root: Option<&Path>,
    pattern: &str,
) -> PathQuerySelector {
    if pattern.is_empty() {
        return PathQuerySelector::Impossible;
    }
    if is_absolute_path_literal(pattern) {
        return PathQuerySelector::Impossible;
    }
    if !path_has_glob_meta(pattern) {
        let relative = pattern.to_owned();
        let absolute = absolute_project_path(project_root, pattern);
        return PathQuerySelector::Exact { relative, absolute };
    }
    let Some(prefix) = extract_glob_literal_prefix(pattern).filter(|prefix| !prefix.is_empty())
    else {
        return PathQuerySelector::Unbounded;
    };
    PathQuerySelector::Prefix {
        relative_like: prefix_like_pattern(prefix),
        absolute_like: absolute_project_path(project_root, prefix)
            .map(|value| prefix_like_pattern(&value)),
    }
}

/// Returns whether one query pattern includes glob metacharacters.
fn path_has_glob_meta(pattern: &str) -> bool {
    pattern.chars().any(|ch| matches!(ch, '*' | '?' | '['))
}

/// Returns the literal prefix before the first glob metacharacter.
fn extract_glob_literal_prefix(pattern: &str) -> Option<&str> {
    let index = pattern
        .char_indices()
        .find_map(|(index, ch)| matches!(ch, '*' | '?' | '[').then_some(index))
        .unwrap_or(pattern.len());
    Some(&pattern[..index])
}

/// Returns the absolute project path for one canonical relative query path.
fn absolute_project_path(project_root: Option<&Path>, relative_path: &str) -> Option<String> {
    let project_root = project_root.map(normalize_path_string)?;
    Some(join_normalized_paths(&project_root, relative_path))
}

/// Joins one normalized root path with one normalized relative suffix.
fn join_normalized_paths(root: &str, suffix: &str) -> String {
    if suffix.is_empty() || suffix == "." {
        return root.to_owned();
    }
    if root == "/" {
        format!("/{suffix}")
    } else {
        format!("{root}/{suffix}")
    }
}

/// Builds one SQL `LIKE` prefix pattern with literal wildcard escaping.
fn prefix_like_pattern(query: &str) -> String {
    format!("{}%", escape_like_pattern(query))
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

/// Builds the grouped file-row query SQL for one optional path selector.
fn build_session_file_rows_sql(path_selector: Option<&PathQuerySelector>) -> String {
    let path_filter = match path_selector {
        Some(PathQuerySelector::Exact { .. }) | Some(PathQuerySelector::Prefix { .. }) => {
            format!(
                "\n        AND {}",
                path_selector
                    .expect("selector should exist")
                    .sql_predicate(6, 7)
            )
        }
        Some(PathQuerySelector::Unbounded) | Some(PathQuerySelector::Impossible) | None => {
            String::new()
        }
    };
    format!(
        "
    SELECT
        file_accesses.provider,
        file_accesses.session_id,
        file_accesses.repo_relative_path,
        file_accesses.path,
        SUM(CASE WHEN file_accesses.access_type = 'read' THEN 1 ELSE 0 END) AS read_count,
        SUM(CASE WHEN file_accesses.access_type IN ('write', 'edit') THEN 1 ELSE 0 END) AS write_count,
        MIN(file_accesses.turn_ordinal) AS first_turn_ordinal,
        MAX(file_accesses.turn_ordinal) AS last_turn_ordinal,
        MIN(turns.started_at) AS first_touched_at,
        MAX(turns.started_at) AS last_touched_at
    FROM file_accesses
    INNER JOIN turns
        ON turns.project_id = file_accesses.project_id
        AND turns.provider = file_accesses.provider
        AND turns.session_id = file_accesses.session_id
        AND turns.turn_ordinal = file_accesses.turn_ordinal
    WHERE file_accesses.project_id = ?1
        AND (?2 IS NULL OR file_accesses.provider = ?2)
        AND (?3 IS NULL OR file_accesses.session_id = ?3)
        AND (?4 IS NULL OR turns.started_at >= ?4)
        AND (?5 IS NULL OR turns.started_at < ?5)
        AND file_accesses.access_type IN ('read', 'write', 'edit')
        AND NULLIF(TRIM(file_accesses.path), '') IS NOT NULL{path_filter}
    GROUP BY
        file_accesses.provider,
        file_accesses.session_id,
        file_accesses.repo_relative_path,
        file_accesses.path
    ORDER BY
        last_touched_at DESC,
        file_accesses.provider ASC,
        file_accesses.session_id ASC,
        COALESCE(file_accesses.repo_relative_path, file_accesses.path) COLLATE NOCASE ASC
"
    )
}

/// Builds one SQLite parameter list for one grouped file-row query.
fn build_session_file_rows_params(
    project_id: &str,
    filters: SessionFileQueryFilters<'_>,
) -> Result<Vec<Value>> {
    let mut params = vec![
        Value::Text(project_id.to_owned()),
        filters.provider.map_or(Value::Null, |provider| {
            Value::Text(provider.directory_name().to_owned())
        }),
        filters
            .session_id
            .map_or(Value::Null, |session_id| Value::Text(session_id.to_owned())),
        filters
            .since
            .map_or(Value::Null, |value| Value::Text(value.to_owned())),
        filters
            .until
            .map_or(Value::Null, |value| Value::Text(value.to_owned())),
    ];
    if let Some(path_selector) = filters.path_selector {
        params.extend(path_selector.params());
    }
    Ok(params)
}

/// Builds one dynamic `WITH requested AS (VALUES ...)` SQL query for session-key joins.
fn build_session_key_values_query_sql(row_count: usize, select_sql: &str) -> String {
    let mut sql = String::from("WITH requested(provider, session_id) AS (VALUES ");
    for row_index in 0..row_count {
        if row_index > 0 {
            sql.push_str(", ");
        }
        let base = row_index
            .checked_mul(2)
            .and_then(|value| value.checked_add(2))
            .expect("placeholder index should stay within usize range");
        write!(&mut sql, "(?{base}, ?{})", base + 1)
            .expect("formatting SQL placeholders should not fail");
    }
    sql.push(')');
    sql.push('\n');
    sql.push_str(select_sql);
    sql
}

/// Builds one SQLite parameter list for a dynamic requested-session query.
fn build_session_key_values_params<'a>(
    project_id: &str,
    session_keys: impl IntoIterator<Item = &'a SessionKey>,
) -> Result<Vec<Value>> {
    let mut params = vec![Value::Text(project_id.to_owned())];
    for session_key in session_keys {
        params.push(Value::Text(
            session_key.provider.directory_name().to_owned(),
        ));
        params.push(Value::Text(session_key.session_id.clone()));
    }
    Ok(params)
}

/// Strips one configured project root from one absolute access path when possible.
fn strip_project_root(path: &str, project_root: &Path) -> Option<String> {
    let path = normalize_path_literal(path);
    let project_root = normalize_path_string(project_root);
    strip_root_prefix_from_path(&path, &project_root)
        .or_else(|| {
            macos_private_variants(&project_root)
                .find_map(|variant| strip_root_prefix_from_path(&path, &variant))
        })
        .or_else(|| {
            macos_private_variants(&path)
                .find_map(|path_variant| strip_root_prefix_from_path(&path_variant, &project_root))
        })
}

/// Normalizes one filesystem path into Darc's stable slash-separated display form.
fn normalize_path_string(path: &Path) -> String {
    normalize_path_literal(&path.to_string_lossy())
}

/// Normalizes one raw path string into Darc's stable slash-separated display form.
fn normalize_path_literal(path: &str) -> String {
    let path = path.trim().replace('\\', "/");
    if path.is_empty() {
        return String::new();
    }

    let (prefix, remainder, absolute) = if let Some(remainder) = path.strip_prefix('/') {
        (Some("/".to_owned()), remainder, true)
    } else if let Some(remainder) = strip_windows_drive_root(&path) {
        (Some(path[..2].to_owned()), remainder, true)
    } else {
        (None, path.as_str(), false)
    };

    let mut components = Vec::<String>::new();
    for component in remainder.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            if components
                .last()
                .is_some_and(|last| !last.is_empty() && last != "..")
            {
                components.pop();
            } else if !absolute {
                components.push(component.to_owned());
            }
            continue;
        }
        components.push(component.to_owned());
    }

    let suffix = components.join("/");
    match prefix.as_deref() {
        Some("/") if suffix.is_empty() => "/".to_owned(),
        Some("/") => format!("/{suffix}"),
        Some(prefix) if suffix.is_empty() => format!("{prefix}/"),
        Some(prefix) => format!("{prefix}/{suffix}"),
        None => suffix,
    }
}

/// Removes one normalized root prefix from one normalized absolute path when it is boundary-aligned.
fn strip_root_prefix_from_path(path: &str, root: &str) -> Option<String> {
    if root.is_empty() || !is_absolute_path_literal(path) {
        return None;
    }
    if path == root {
        return Some(String::new());
    }
    path.strip_prefix(root)
        .and_then(|suffix| suffix.strip_prefix('/'))
        .map(str::to_owned)
}

/// Returns whether one normalized path literal is absolute on common host platforms.
fn is_absolute_path_literal(path: &str) -> bool {
    path.starts_with('/') || strip_windows_drive_root(path).is_some()
}

/// Removes one `C:/`-style drive prefix when present.
fn strip_windows_drive_root(path: &str) -> Option<&str> {
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        Some(&path[3..])
    } else {
        None
    }
}

/// Yields the macOS `/private` toggled variants for one normalized absolute path.
fn macos_private_variants<'a>(path: &'a str) -> impl Iterator<Item = String> + 'a {
    [
        path.strip_prefix("/private").map(str::to_owned),
        Some(format!("/private{path}")).filter(|_| !path.starts_with("/private/")),
    ]
    .into_iter()
    .flatten()
}

#[cfg(test)]
/// Prepares every file-query SQL statement against one live schema.
pub(super) fn smoke_test_sql(connection: &Connection) -> Result<()> {
    for (label, sql) in [
        ("session file rows query", build_session_file_rows_sql(None)),
        (
            "session file rows query with exact selector",
            build_session_file_rows_sql(Some(&PathQuerySelector::Exact {
                relative: "README.md".to_owned(),
                absolute: Some("/tmp/repo/README.md".to_owned()),
            })),
        ),
        (
            "touched-path requested session file rows query",
            build_touched_path_file_rows_sql(
                1,
                &PathQuerySelector::Exact {
                    relative: "README.md".to_owned(),
                    absolute: Some("/tmp/repo/README.md".to_owned()),
                },
            ),
        ),
        (
            "co-touched file page query",
            build_co_touched_file_page_sql(&PathQuerySelector::Exact {
                relative: "README.md".to_owned(),
                absolute: Some("/tmp/repo/README.md".to_owned()),
            }),
        ),
        (
            "requested session paths query",
            build_session_key_values_query_sql(
                1,
                "
                SELECT
                    file_accesses.provider,
                    file_accesses.session_id,
                    file_accesses.repo_relative_path,
                    file_accesses.path
                FROM requested
                INNER JOIN file_accesses
                    ON file_accesses.project_id = ?1
                    AND file_accesses.provider = requested.provider
                    AND file_accesses.session_id = requested.session_id
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
