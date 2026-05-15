use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

use anyhow::{Context, Result};
use darc_paths::{SourceKind, normalize_access_path_candidate};
use darc_store::policy::{extract_shell_command, should_include_turn_in_active_time};
use rusqlite::Connection;

use super::{
    DailyTimeStat, FileUsageScope, FileUsageStat, InsightTurnRow, LocalDate, ProjectInsightRow,
    ProjectInsights, ProjectTimeStat, SessionAggregate, SessionRuntimeStat, ShellCommandSummary,
    ToolUsageScope, ToolUsageStat, WorkspaceDailyTimeStat, WorkspaceInsights,
    open_existing_index_database, paginate_ranked_rows, parse_provider, parse_turn_status,
    sort_tool_usage_stats, sql_count_to_u64,
};
use crate::query::files::display_path_for_access;

const LATEST_LOCAL_DATE_SQL: &str = "
    SELECT MAX(DATE(turns.started_at, 'localtime'))
    FROM turns
    INNER JOIN sessions
        ON sessions.project_id = turns.project_id
        AND sessions.provider = turns.provider
        AND sessions.session_id = turns.session_id
    WHERE sessions.origin_kind = 'local'
";
const LOCAL_TODAY_SQL: &str = "SELECT DATE('now', 'localtime')";
const WORKSPACE_INSIGHT_ROWS_SQL: &str = "
    SELECT
        turns.project_id,
        turns.provider,
        turns.session_id,
        turns.started_at,
        DATE(turns.started_at, 'localtime'),
        turns.status,
        COALESCE(turns.duration_ms, 0)
    FROM turns
    INNER JOIN sessions
        ON sessions.project_id = turns.project_id
        AND sessions.provider = turns.provider
        AND sessions.session_id = turns.session_id
    WHERE sessions.origin_kind = 'local'
        AND DATE(turns.started_at, 'localtime') >= ?1
        AND DATE(turns.started_at, 'localtime') < ?2
    ORDER BY turns.started_at ASC, turns.project_id ASC, turns.provider ASC, turns.session_id ASC, turns.turn_ordinal ASC
";

const PROJECT_INSIGHT_ROWS_SQL: &str = "
    SELECT
        turns.project_id,
        turns.provider,
        turns.session_id,
        turns.turn_ordinal,
        DATE(turns.started_at, 'localtime'),
        turns.status,
        COALESCE(turns.duration_ms, 0)
    FROM turns
    INNER JOIN sessions
        ON sessions.project_id = turns.project_id
        AND sessions.provider = turns.provider
        AND sessions.session_id = turns.session_id
    WHERE turns.project_id = ?1
        AND sessions.origin_kind = 'local'
        AND (?2 IS NULL OR turns.provider = ?2)
    ORDER BY turns.started_at DESC, turns.provider ASC, turns.session_id ASC, turns.turn_ordinal ASC
    LIMIT ?3
";

const TURN_TOOL_USAGE_SQL: &str = "
    SELECT tool_name, COUNT(*) AS call_count
    FROM tool_calls
    WHERE project_id = ?1
        AND provider = ?2
        AND session_id = ?3
        AND turn_ordinal = ?4
        AND tool_name IS NOT NULL
    GROUP BY tool_name
";

const RECENT_PROJECT_TOOL_USAGE_SQL: &str = "
    WITH recent_turns AS (
        SELECT turns.project_id, turns.provider, turns.session_id, turns.turn_ordinal
        FROM turns
        INNER JOIN sessions
            ON sessions.project_id = turns.project_id
            AND sessions.provider = turns.provider
            AND sessions.session_id = turns.session_id
        WHERE turns.project_id = ?1
            AND sessions.origin_kind = 'local'
            AND (?2 IS NULL OR turns.provider = ?2)
        ORDER BY turns.started_at DESC, turns.provider ASC, turns.session_id ASC, turns.turn_ordinal ASC
        LIMIT ?3
    )
    SELECT tool_calls.tool_name, COUNT(*) AS call_count
    FROM recent_turns
    INNER JOIN tool_calls
        ON tool_calls.project_id = recent_turns.project_id
        AND tool_calls.provider = recent_turns.provider
        AND tool_calls.session_id = recent_turns.session_id
        AND tool_calls.turn_ordinal = recent_turns.turn_ordinal
    WHERE tool_calls.tool_name IS NOT NULL
    GROUP BY tool_calls.tool_name
";

const TURN_FILE_USAGE_SQL: &str = "
    SELECT
        repo_relative_path,
        path,
        SUM(CASE WHEN access_type IN ('read', 'list') THEN 1 ELSE 0 END) AS read_count,
        SUM(CASE WHEN access_type IN ('write', 'edit') THEN 1 ELSE 0 END) AS write_count
    FROM file_accesses
    WHERE project_id = ?1
        AND provider = ?2
        AND session_id = ?3
        AND turn_ordinal = ?4
        AND NULLIF(TRIM(path), '') IS NOT NULL
    GROUP BY repo_relative_path, path
";

const RECENT_PROJECT_FILE_USAGE_SQL: &str = "
    WITH recent_turns AS (
        SELECT turns.project_id, turns.provider, turns.session_id, turns.turn_ordinal
        FROM turns
        INNER JOIN sessions
            ON sessions.project_id = turns.project_id
            AND sessions.provider = turns.provider
            AND sessions.session_id = turns.session_id
        WHERE turns.project_id = ?1
            AND sessions.origin_kind = 'local'
            AND (?2 IS NULL OR turns.provider = ?2)
        ORDER BY turns.started_at DESC, turns.provider ASC, turns.session_id ASC, turns.turn_ordinal ASC
        LIMIT ?3
    )
    SELECT
        file_accesses.repo_relative_path,
        file_accesses.path,
        SUM(CASE
            WHEN file_accesses.access_type IN ('read', 'list') THEN 1
            ELSE 0
        END) AS read_count,
        SUM(CASE
            WHEN file_accesses.access_type IN ('write', 'edit') THEN 1
            ELSE 0
        END) AS write_count
    FROM recent_turns
    INNER JOIN file_accesses
        ON file_accesses.project_id = recent_turns.project_id
        AND file_accesses.provider = recent_turns.provider
        AND file_accesses.session_id = recent_turns.session_id
        AND file_accesses.turn_ordinal = recent_turns.turn_ordinal
    WHERE NULLIF(TRIM(file_accesses.path), '') IS NOT NULL
    GROUP BY file_accesses.repo_relative_path, file_accesses.path
";

const TURN_SHELL_COMMANDS_SQL: &str = "
    SELECT tool_name, arguments_text
    FROM tool_calls
    WHERE project_id = ?1
        AND provider = ?2
        AND session_id = ?3
        AND turn_ordinal = ?4
        AND tool_name IS NOT NULL
        AND arguments_text IS NOT NULL
    ORDER BY call_ordinal ASC
";

/// Queries one workspace insights payload for a rolling host-local day window.
pub fn query_workspace_insights(
    index_db_path: &Path,
    window_days: u32,
    recent_session_limit: usize,
    recent_session_offset: usize,
) -> Result<WorkspaceInsights> {
    let connection = open_existing_index_database(index_db_path)?;
    build_workspace_insights(
        &connection,
        window_days,
        recent_session_limit,
        recent_session_offset,
    )
}

/// Queries one project insights payload for one indexed project.
pub fn query_project_insights(
    index_db_path: &Path,
    project_id: &str,
    project_root: Option<&Path>,
    provider: Option<SourceKind>,
    limit: usize,
) -> Result<ProjectInsights> {
    let connection = open_existing_index_database(index_db_path)?;
    build_project_insights(&connection, project_id, project_root, provider, limit)
}

/// Builds one workspace insights report from indexed turn rows.
pub(crate) fn build_workspace_insights(
    connection: &Connection,
    window_days: u32,
    recent_session_limit: usize,
    recent_session_offset: usize,
) -> Result<WorkspaceInsights> {
    let window_days = window_days.max(1);
    let anchor_date = query_latest_local_date(connection)?
        .as_deref()
        .and_then(LocalDate::parse)
        .unwrap_or(query_local_today(connection)?);
    let start_date = anchor_date
        .add_days(-(i64::from(window_days) - 1))
        .context("failed to calculate workspace insights window start")?;
    let end_exclusive = anchor_date
        .add_days(1)
        .context("failed to calculate workspace insights window end")?;
    let rows = query_workspace_insight_rows(connection, &start_date, &end_exclusive)?;

    let date_keys = (0..window_days)
        .map(|offset| {
            start_date
                .add_days(i64::from(offset))
                .context("failed to enumerate workspace insights dates")
                .map(|date| date.to_string())
        })
        .collect::<Result<Vec<_>>>()?;
    let mut daily_time_map = BTreeMap::<String, u64>::new();
    let mut daily_project_time_map = BTreeMap::<String, BTreeMap<String, u64>>::new();
    for date in &date_keys {
        daily_time_map.insert(date.clone(), 0);
        daily_project_time_map.insert(date.clone(), BTreeMap::new());
    }

    let mut session_map = HashMap::<(String, SourceKind, String), SessionAggregate>::new();
    let mut total_time_ms = 0_u64;
    let mut included_turn_count = 0_u64;
    let mut excluded_turn_count = 0_u64;

    for row in rows {
        let key = (row.project_id.clone(), row.provider, row.session_id.clone());
        let aggregate = session_map.entry(key).or_insert_with(|| SessionAggregate {
            project_id: row.project_id.clone(),
            provider: row.provider,
            session_id: row.session_id.clone(),
            started_at: row.started_at.clone(),
            latest_turn_at: row.started_at.clone(),
            active_time_ms: 0,
            active_turn_count: 0,
            excluded_turn_count: 0,
        });
        if row.started_at < aggregate.started_at {
            aggregate.started_at = row.started_at.clone();
        }
        if row.started_at > aggregate.latest_turn_at {
            aggregate.latest_turn_at = row.started_at.clone();
        }

        if should_include_turn_in_active_time(row.status, row.duration_ms) {
            aggregate.active_time_ms = aggregate.active_time_ms.saturating_add(row.duration_ms);
            aggregate.active_turn_count = aggregate.active_turn_count.saturating_add(1);
            total_time_ms = total_time_ms.saturating_add(row.duration_ms);
            included_turn_count = included_turn_count.saturating_add(1);

            if let Some(total) = daily_time_map.get_mut(&row.local_date) {
                *total = (*total).saturating_add(row.duration_ms);
            }
            if let Some(projects) = daily_project_time_map.get_mut(&row.local_date) {
                let project_total = projects.entry(row.project_id.clone()).or_insert(0);
                *project_total = project_total.saturating_add(row.duration_ms);
            }
        } else {
            aggregate.excluded_turn_count = aggregate.excluded_turn_count.saturating_add(1);
            excluded_turn_count = excluded_turn_count.saturating_add(1);
        }
    }

    let mut recent_sessions = session_map
        .into_values()
        .filter(|session| session.active_time_ms > 0)
        .map(|session| SessionRuntimeStat {
            project_id: session.project_id,
            provider: session.provider,
            session_id: session.session_id,
            started_at: session.started_at,
            latest_turn_at: session.latest_turn_at,
            active_time_ms: session.active_time_ms,
            active_turn_count: session.active_turn_count,
            excluded_turn_count: session.excluded_turn_count,
        })
        .collect::<Vec<_>>();
    recent_sessions.sort_by(|left, right| {
        right
            .latest_turn_at
            .cmp(&left.latest_turn_at)
            .then_with(|| right.started_at.cmp(&left.started_at))
            .then_with(|| left.project_id.cmp(&right.project_id))
            .then_with(|| left.provider.cmp(&right.provider))
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    let active_session_count = u64::try_from(recent_sessions.len()).unwrap_or(u64::MAX);
    let (recent_sessions, recent_sessions_has_more) =
        paginate_ranked_rows(recent_sessions, recent_session_limit, recent_session_offset)?;

    Ok(WorkspaceInsights {
        window_start: start_date.to_string(),
        window_end: anchor_date.to_string(),
        recent_session_limit: u64::try_from(recent_session_limit)
            .context("query limit exceeds u64 range")?,
        recent_session_offset: u64::try_from(recent_session_offset)
            .context("query offset exceeds u64 range")?,
        recent_sessions_has_more,
        daily_time: date_keys
            .into_iter()
            .map(|date| WorkspaceDailyTimeStat {
                active_time_ms: *daily_time_map.get(&date).unwrap_or(&0),
                projects: daily_project_time_map
                    .get(&date)
                    .into_iter()
                    .flat_map(|projects| projects.iter())
                    .map(|(project_id, active_time_ms)| ProjectTimeStat {
                        project_id: project_id.clone(),
                        active_time_ms: *active_time_ms,
                    })
                    .collect(),
                date,
            })
            .collect(),
        active_session_count,
        recent_sessions,
        included_turn_count,
        excluded_turn_count,
        total_time_ms,
    })
}

/// Builds one project insights report from indexed turn rows.
pub(crate) fn build_project_insights(
    connection: &Connection,
    project_id: &str,
    project_root: Option<&Path>,
    provider: Option<SourceKind>,
    limit: usize,
) -> Result<ProjectInsights> {
    let (rows, turns_has_more) =
        query_project_insight_rows(connection, project_id, provider, limit)?;
    let inspected_turn_count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
    let all_files = query_file_usage_stats(
        connection,
        FileUsageScope::RecentProject {
            project_id,
            project_root,
            provider,
            limit,
        },
    )?;
    let mut most_common_tools = query_tool_usage_stats(
        connection,
        ToolUsageScope::RecentProject {
            project_id,
            provider,
            limit,
        },
    )?;
    sort_tool_usage_stats(&mut most_common_tools);
    most_common_tools.truncate(10);
    let mut daily_time_map = BTreeMap::<String, u64>::new();
    let mut failure_count = 0_u64;
    let mut total_time_ms = 0_u64;

    for row in rows {
        if row.status != darc_rollout::model::NormalizedTurnStatus::Completed {
            failure_count = failure_count.saturating_add(1);
        }

        if should_include_turn_in_active_time(row.status, row.duration_ms) {
            total_time_ms = total_time_ms.saturating_add(row.duration_ms);
            let total = daily_time_map.entry(row.local_date.clone()).or_insert(0);
            *total = total.saturating_add(row.duration_ms);
        }
    }

    let mut most_read_files = all_files
        .iter()
        .filter(|stat| stat.read_count > 0)
        .cloned()
        .collect::<Vec<_>>();
    most_read_files.sort_by(|left, right| {
        right
            .read_count
            .cmp(&left.read_count)
            .then_with(|| left.path.cmp(&right.path))
    });
    most_read_files.truncate(10);

    let mut most_written_files = all_files
        .into_iter()
        .filter(|stat| stat.write_count > 0)
        .collect::<Vec<_>>();
    most_written_files.sort_by(|left, right| {
        right
            .write_count
            .cmp(&left.write_count)
            .then_with(|| left.path.cmp(&right.path))
    });
    most_written_files.truncate(10);

    Ok(ProjectInsights {
        provider,
        turn_limit: u64::try_from(limit).context("project insights limit exceeds u64 range")?,
        inspected_turn_count,
        turns_has_more,
        daily_time: daily_time_map
            .into_iter()
            .map(|(date, active_time_ms)| DailyTimeStat {
                date,
                active_time_ms,
            })
            .collect(),
        most_common_tools,
        most_read_files,
        most_written_files,
        failure_count,
        total_time_ms,
    })
}

/// Queries the latest indexed host-local day in one workspace database.
fn query_latest_local_date(connection: &Connection) -> Result<Option<String>> {
    connection
        .query_row(LATEST_LOCAL_DATE_SQL, [], |row| row.get(0))
        .context("failed to query latest indexed local day")
}

/// Queries the current host-local civil day according to SQLite localtime rules.
fn query_local_today(connection: &Connection) -> Result<LocalDate> {
    let value = connection
        .query_row(LOCAL_TODAY_SQL, [], |row| row.get::<_, String>(0))
        .context("failed to query current local day")?;
    LocalDate::parse(&value).with_context(|| format!("failed to parse SQLite local day `{value}`"))
}

/// Queries the turn rows needed to build workspace insights.
fn query_workspace_insight_rows(
    connection: &Connection,
    start_date: &LocalDate,
    end_exclusive: &LocalDate,
) -> Result<Vec<InsightTurnRow>> {
    let started_at_or_after = start_date.to_string();
    let started_before = end_exclusive.to_string();
    let mut statement = connection
        .prepare(WORKSPACE_INSIGHT_ROWS_SQL)
        .context("failed to prepare workspace insights query")?;
    let rows = statement
        .query_map((started_at_or_after, started_before), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .context("failed to query workspace insight rows")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read workspace insight rows")?;
    rows.into_iter()
        .map(
            |(project_id, provider, session_id, started_at, local_date, status, duration_ms)| {
                Ok(InsightTurnRow {
                    project_id,
                    provider: parse_provider(&provider)?,
                    session_id,
                    started_at,
                    local_date,
                    status: parse_turn_status(&status)?,
                    duration_ms: sql_count_to_u64(duration_ms)?,
                })
            },
        )
        .collect()
}

/// Queries the turn rows needed to build one project insights report.
fn query_project_insight_rows(
    connection: &Connection,
    project_id: &str,
    provider: Option<SourceKind>,
    limit: usize,
) -> Result<(Vec<ProjectInsightRow>, bool)> {
    let page_limit = limit
        .checked_add(1)
        .context("project insights limit exceeds usize range")?;
    let page_limit =
        i64::try_from(page_limit).context("project insights limit exceeds SQLite INTEGER range")?;
    let provider = provider.map(SourceKind::directory_name);
    let mut statement = connection
        .prepare(PROJECT_INSIGHT_ROWS_SQL)
        .context("failed to prepare project insights query")?;
    let mut rows = statement
        .query_map((project_id, provider, page_limit), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .context("failed to query project insight rows")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read project insight rows")?;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let rows = rows
        .into_iter()
        .map(
            |(
                _project_id,
                _provider,
                _session_id,
                _turn_ordinal,
                local_date,
                status,
                duration_ms,
            )|
             -> Result<_> {
                Ok(ProjectInsightRow {
                    local_date,
                    status: parse_turn_status(&status)?,
                    duration_ms: sql_count_to_u64(duration_ms)?,
                })
            },
        )
        .collect::<Result<Vec<_>>>()?;
    Ok((rows, has_more))
}

/// Queries grouped tool usage stats for one query scope.
pub(crate) fn query_tool_usage_stats(
    connection: &Connection,
    scope: ToolUsageScope<'_>,
) -> Result<Vec<ToolUsageStat>> {
    let rows = match scope {
        ToolUsageScope::Turn {
            project_id,
            provider,
            session_id,
            turn_ordinal,
        } => {
            let turn_ordinal =
                i64::try_from(turn_ordinal).context("turn ordinal exceeds SQLite INTEGER range")?;
            let mut statement = connection
                .prepare(TURN_TOOL_USAGE_SQL)
                .context("failed to prepare turn tool usage query")?;
            statement
                .query_map(
                    (
                        project_id,
                        provider.directory_name(),
                        session_id,
                        turn_ordinal,
                    ),
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .context("failed to query turn tool usage rows")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("failed to read turn tool usage rows")?
        }
        ToolUsageScope::RecentProject {
            project_id,
            provider,
            limit,
        } => {
            let limit = i64::try_from(limit)
                .context("project insights limit exceeds SQLite INTEGER range")?;
            let provider = provider.map(SourceKind::directory_name);
            let mut statement = connection
                .prepare(RECENT_PROJECT_TOOL_USAGE_SQL)
                .context("failed to prepare project tool usage query")?;
            statement
                .query_map((project_id, provider, limit), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .context("failed to query project tool usage rows")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("failed to read project tool usage rows")?
        }
    };
    rows.into_iter()
        .map(|(name, count)| -> Result<_> {
            Ok(ToolUsageStat {
                name,
                count: sql_count_to_u64(count)?,
            })
        })
        .collect()
}

/// Queries grouped file usage stats for one query scope.
pub(crate) fn query_file_usage_stats(
    connection: &Connection,
    scope: FileUsageScope<'_>,
) -> Result<Vec<FileUsageStat>> {
    let rows = match scope {
        FileUsageScope::Turn {
            project_id,
            provider,
            session_id,
            turn_ordinal,
            ..
        } => {
            let turn_ordinal =
                i64::try_from(turn_ordinal).context("turn ordinal exceeds SQLite INTEGER range")?;
            let mut statement = connection
                .prepare(TURN_FILE_USAGE_SQL)
                .context("failed to prepare turn file usage query")?;
            statement
                .query_map(
                    (
                        project_id,
                        provider.directory_name(),
                        session_id,
                        turn_ordinal,
                    ),
                    |row| {
                        Ok(FileUsageRawStat {
                            repo_relative_path: row.get::<_, Option<String>>(0)?,
                            path: row.get::<_, String>(1)?,
                            read_count: row.get::<_, i64>(2)?,
                            write_count: row.get::<_, i64>(3)?,
                        })
                    },
                )
                .context("failed to query turn file usage rows")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("failed to read turn file usage rows")?
        }
        FileUsageScope::RecentProject {
            project_id,
            provider,
            limit,
            ..
        } => {
            let limit = i64::try_from(limit)
                .context("project insights limit exceeds SQLite INTEGER range")?;
            let provider = provider.map(SourceKind::directory_name);
            let mut statement = connection
                .prepare(RECENT_PROJECT_FILE_USAGE_SQL)
                .context("failed to prepare project file usage query")?;
            statement
                .query_map((project_id, provider, limit), |row| {
                    Ok(FileUsageRawStat {
                        repo_relative_path: row.get::<_, Option<String>>(0)?,
                        path: row.get::<_, String>(1)?,
                        read_count: row.get::<_, i64>(2)?,
                        write_count: row.get::<_, i64>(3)?,
                    })
                })
                .context("failed to query project file usage rows")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("failed to read project file usage rows")?
        }
    };
    merge_file_usage_stats(rows, scope.project_root())
}

/// Stores one raw grouped file-usage row before display-path normalization.
struct FileUsageRawStat {
    repo_relative_path: Option<String>,
    path: String,
    read_count: i64,
    write_count: i64,
}

impl<'a> FileUsageScope<'a> {
    /// Returns the configured project root used for insight file display paths.
    fn project_root(self) -> Option<&'a Path> {
        match self {
            Self::Turn { project_root, .. } | Self::RecentProject { project_root, .. } => {
                project_root
            }
        }
    }
}

/// Merges raw file-usage rows by the path shown in insights payloads.
fn merge_file_usage_stats(
    rows: Vec<FileUsageRawStat>,
    project_root: Option<&Path>,
) -> Result<Vec<FileUsageStat>> {
    let mut stats = BTreeMap::<String, (u64, u64)>::new();
    for row in rows {
        let Some(path) =
            display_file_usage_path(project_root, row.repo_relative_path.as_deref(), &row.path)
        else {
            continue;
        };
        let read_count = sql_count_to_u64(row.read_count)?;
        let write_count = sql_count_to_u64(row.write_count)?;
        let entry = stats.entry(path).or_insert((0, 0));
        entry.0 = entry.0.saturating_add(read_count);
        entry.1 = entry.1.saturating_add(write_count);
    }

    Ok(stats
        .into_iter()
        .map(|(path, (read_count, write_count))| FileUsageStat {
            path,
            read_count,
            write_count,
        })
        .collect())
}

/// Returns the in-project relative path for a file access, falling back to the stored path.
fn display_file_usage_path(
    project_root: Option<&Path>,
    repo_relative_path: Option<&str>,
    path: &str,
) -> Option<String> {
    display_path_for_access(project_root, repo_relative_path, path)
        .or_else(|| normalize_access_path_candidate(path))
}

/// Queries shell-like command invocations for one indexed turn.
pub(crate) fn query_turn_shell_commands(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
) -> Result<Vec<ShellCommandSummary>> {
    let turn_ordinal =
        i64::try_from(turn_ordinal).context("turn ordinal exceeds SQLite INTEGER range")?;
    let mut statement = connection
        .prepare(TURN_SHELL_COMMANDS_SQL)
        .context("failed to prepare turn shell command query")?;
    let rows = statement
        .query_map(
            (
                project_id,
                provider.directory_name(),
                session_id,
                turn_ordinal,
            ),
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .context("failed to query turn shell command rows")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read turn shell command rows")?;
    Ok(rows
        .into_iter()
        .filter_map(|(tool_name, arguments_text)| {
            extract_shell_command(&tool_name, &arguments_text).map(|command| ShellCommandSummary {
                tool_name,
                command_text: command.command_text,
                workdir: command.workdir,
            })
        })
        .collect())
}

#[cfg(test)]
/// Prepares every insight query against one live schema.
pub(super) fn smoke_test_sql(connection: &Connection) -> Result<()> {
    for (label, sql) in [
        ("latest local date query", LATEST_LOCAL_DATE_SQL),
        ("local today query", LOCAL_TODAY_SQL),
        ("workspace insight rows query", WORKSPACE_INSIGHT_ROWS_SQL),
        ("project insight rows query", PROJECT_INSIGHT_ROWS_SQL),
        ("turn tool usage query", TURN_TOOL_USAGE_SQL),
        (
            "recent project tool usage query",
            RECENT_PROJECT_TOOL_USAGE_SQL,
        ),
        ("turn file usage query", TURN_FILE_USAGE_SQL),
        (
            "recent project file usage query",
            RECENT_PROJECT_FILE_USAGE_SQL,
        ),
        ("turn shell commands query", TURN_SHELL_COMMANDS_SQL),
    ] {
        connection
            .prepare(sql)
            .with_context(|| format!("failed to prepare {label}"))?;
    }
    Ok(())
}
