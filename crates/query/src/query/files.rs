use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    path::Path,
};

use anyhow::{Context, Result, bail};
use darc_paths::SourceKind;
use glob::{MatchOptions, Pattern};
use rusqlite::{Connection, params_from_iter, types::Value};

use super::{
    CoTouchedFileSummary, FileSessionSummary, FilesQueryData, FilesQueryMode, FilesQueryRequest,
    SessionFileSummary, SessionFilesQueryData, SessionSummary, open_existing_index_database,
    paginate_ranked_rows, parse_provider, sql_count_to_u64,
};

const MAX_SESSION_KEYS_PER_QUERY: usize = 250;

/// Stores one stable session identity used while intersecting file-touch filters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SessionKey {
    provider: SourceKind,
    session_id: String,
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

/// Stores the supported path-selector plans used to narrow file-access queries in SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PathQuerySelector {
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
    fn sql_predicate(&self, relative_param: usize, absolute_param: usize) -> String {
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
    fn params(&self) -> Vec<Value> {
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
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    project_root: Option<&Path>,
) -> Result<SessionFilesQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    build_session_files_query(&connection, project_id, provider, session_id, project_root)
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
                .query_map(params_from_iter(params), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .context("failed to query touched-path file rows")?
                .map(|row| {
                    let (provider, session_id, repo_relative_path, path) =
                        row.context("failed to read touched-path file row")?;
                    Ok((
                        parse_provider(&provider)?,
                        session_id,
                        repo_relative_path,
                        path,
                    ))
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
    let path = request
        .path
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let co_touched_with = request
        .co_touched_with
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (path, co_touched_with) {
        (Some(path), None) => {
            let sessions = query_file_session_matches(
                connection,
                request.project_id,
                request.project_root,
                path,
                request.since,
                request.until,
            )?;
            let (sessions, has_more) =
                paginate_ranked_rows(sessions, request.limit, request.offset)?;
            Ok(FilesQueryData {
                project_id: request.project_id.to_owned(),
                mode: FilesQueryMode::Path,
                path: Some(path.to_owned()),
                co_touched_with: None,
                since: request.since.map(str::to_owned),
                until: request.until.map(str::to_owned),
                limit: u64::try_from(request.limit).context("query limit exceeds u64 range")?,
                offset: u64::try_from(request.offset).context("query offset exceeds u64 range")?,
                has_more,
                sessions,
                files: Vec::new(),
            })
        }
        (None, Some(seed_path)) => {
            if request.since.is_some() {
                bail!("--since requires --path");
            }
            if request.until.is_some() {
                bail!("--until requires --path");
            }
            let files = query_co_touched_files(
                connection,
                request.project_id,
                request.project_root,
                seed_path,
            )?;
            let (files, has_more) = paginate_ranked_rows(files, request.limit, request.offset)?;
            Ok(FilesQueryData {
                project_id: request.project_id.to_owned(),
                mode: FilesQueryMode::CoTouchedWith,
                path: None,
                co_touched_with: Some(seed_path.to_owned()),
                since: None,
                until: None,
                limit: u64::try_from(request.limit).context("query limit exceeds u64 range")?,
                offset: u64::try_from(request.offset).context("query offset exceeds u64 range")?,
                has_more,
                sessions: Vec::new(),
                files,
            })
        }
        (Some(_), Some(_)) => {
            bail!("query files requires exactly one of --path or --co-touched-with")
        }
        (None, None) => bail!("query files requires exactly one of --path or --co-touched-with"),
    }
}

/// Builds one session-scoped per-file summary payload from canonicalized file touches.
pub(crate) fn build_session_files_query(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    project_root: Option<&Path>,
) -> Result<SessionFilesQueryData> {
    let rows = query_raw_session_file_rows(
        connection,
        project_id,
        SessionFileQueryFilters {
            provider: Some(provider),
            session_id: Some(session_id),
            since: None,
            until: None,
            path_selector: None,
        },
    )?;
    let mut files = aggregate_session_file_rows(rows, project_root)
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
    Ok(SessionFilesQueryData {
        project_id: project_id.to_owned(),
        provider,
        session_id: session_id.to_owned(),
        files,
    })
}

/// Queries one file-to-session pivot ranked by descending touch frequency.
fn query_file_session_matches(
    connection: &Connection,
    project_id: &str,
    project_root: Option<&Path>,
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
            provider: None,
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
        .map(|(key, session)| FileSessionSummary {
            provider: key.provider,
            session_id: key.session_id,
            touch_count: session.touch_count,
            read_count: session.read_count,
            write_count: session.write_count,
            first_turn_ordinal: session.first_turn_ordinal,
            last_turn_ordinal: session.last_turn_ordinal,
            first_touched_at: session.first_touched_at,
            last_touched_at: session.last_touched_at,
            matched_paths: session.matched_paths.into_iter().collect(),
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
    project_id: &str,
    project_root: Option<&Path>,
    seed_path: &str,
) -> Result<Vec<CoTouchedFileSummary>> {
    let seed_path = normalize_project_scoped_query_path(project_root, seed_path);
    let Some(seed_path) = seed_path else {
        return Ok(Vec::new());
    };
    let seed_selector = build_path_query_selector(project_root, &seed_path);
    let seed_rows = query_raw_session_file_rows(
        connection,
        project_id,
        SessionFileQueryFilters {
            provider: None,
            session_id: None,
            since: None,
            until: None,
            path_selector: Some(&seed_selector),
        },
    )?;
    let seed_session_keys = aggregate_session_file_rows(seed_rows, project_root)
        .into_iter()
        .filter(|row| row.path.eq_ignore_ascii_case(&seed_path))
        .map(|row| SessionKey {
            provider: row.provider,
            session_id: row.session_id,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let aggregates =
        query_raw_session_file_rows_for_session_keys(connection, project_id, &seed_session_keys)?;
    let aggregates = aggregate_session_file_rows(aggregates, project_root);

    let mut files_by_session = BTreeMap::<SessionKey, Vec<AggregatedSessionFileRow>>::new();
    for row in aggregates {
        files_by_session
            .entry(SessionKey {
                provider: row.provider,
                session_id: row.session_id.clone(),
            })
            .or_default()
            .push(row);
    }

    let mut co_touched_counts = BTreeMap::<String, u64>::new();
    for rows in files_by_session.values() {
        if !rows
            .iter()
            .any(|row| row.path.eq_ignore_ascii_case(&seed_path))
        {
            continue;
        }
        for row in rows
            .iter()
            .filter(|row| !row.path.eq_ignore_ascii_case(&seed_path))
        {
            *co_touched_counts.entry(row.path.clone()).or_default() += 1;
        }
    }

    let mut files = co_touched_counts
        .into_iter()
        .map(|(path, co_touch_count)| CoTouchedFileSummary {
            path,
            co_touch_count,
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| {
        right
            .co_touch_count
            .cmp(&left.co_touch_count)
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(files)
}

/// Queries grouped raw file rows for one provider/session/time selector set.
fn query_raw_session_file_rows(
    connection: &Connection,
    project_id: &str,
    filters: SessionFileQueryFilters<'_>,
) -> Result<Vec<RawSessionFileRow>> {
    if filters
        .path_selector
        .is_some_and(|selector| matches!(selector, PathQuerySelector::Impossible))
    {
        return Ok(Vec::new());
    }

    let sql = build_session_file_rows_sql(filters.path_selector);
    let mut statement = connection
        .prepare(&sql)
        .context("failed to prepare session file rows query")?;
    let params = build_session_file_rows_params(project_id, filters)?;
    let rows = statement
        .query_map(params_from_iter(params), read_raw_session_file_row)
        .context("failed to query session file rows")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read session file rows")?;
    rows.into_iter()
        .map(build_raw_session_file_row)
        .collect::<Result<Vec<_>>>()
}

/// Queries grouped raw file rows for one requested session set.
fn query_raw_session_file_rows_for_session_keys(
    connection: &Connection,
    project_id: &str,
    session_keys: &[SessionKey],
) -> Result<Vec<RawSessionFileRow>> {
    if session_keys.is_empty() {
        return Ok(Vec::new());
    }

    let mut rows = Vec::new();
    for session_chunk in session_keys.chunks(MAX_SESSION_KEYS_PER_QUERY) {
        let sql = build_requested_session_file_rows_sql(session_chunk.len());
        let mut statement = connection
            .prepare(&sql)
            .context("failed to prepare requested session file rows query")?;
        let params = build_session_key_values_params(project_id, session_chunk.iter())?;
        let chunk_rows = statement
            .query_map(params_from_iter(params), read_raw_session_file_row)
            .context("failed to query requested session file rows")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read requested session file rows")?;
        rows.extend(
            chunk_rows
                .into_iter()
                .map(build_raw_session_file_row)
                .collect::<Result<Vec<_>>>()?,
        );
    }
    Ok(rows)
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
fn display_path_for_access(
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
fn build_path_query_selector(project_root: Option<&Path>, pattern: &str) -> PathQuerySelector {
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

/// Builds one requested-session grouped file-row query SQL.
fn build_requested_session_file_rows_sql(row_count: usize) -> String {
    build_session_key_values_query_sql(
        row_count,
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
        FROM requested
        INNER JOIN file_accesses
            ON file_accesses.project_id = ?1
            AND file_accesses.provider = requested.provider
            AND file_accesses.session_id = requested.session_id
        INNER JOIN turns
            ON turns.project_id = file_accesses.project_id
            AND turns.provider = file_accesses.provider
            AND turns.session_id = file_accesses.session_id
            AND turns.turn_ordinal = file_accesses.turn_ordinal
        WHERE file_accesses.access_type IN ('read', 'write', 'edit')
            AND NULLIF(TRIM(file_accesses.path), '') IS NOT NULL
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
        ",
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
            "requested session file rows query",
            build_requested_session_file_rows_sql(1),
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
