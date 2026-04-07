use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use darc_index::open_index_database;
use darc_index::policy::{
    HardDebuggingCandidate, extract_shell_command, rank_hard_debuggings,
    should_include_turn_in_active_time,
};
use darc_paths::SourceKind;
use darc_rollout::model::{NormalizedTurnStatus, NormalizedTurnStep};
use rusqlite::Connection;
use serde::Serialize;

/// Stores one indexed project aggregate used by the workspace sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectIndexAggregate {
    pub project_id: String,
    pub session_count: u64,
    pub turn_count: u64,
    pub last_activity_at: Option<String>,
}

/// Stores the resolved workspace root availability reported to machine clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RootInfo {
    pub default_root_path: PathBuf,
    pub requested_root_path: PathBuf,
    pub resolved_root_path: PathBuf,
    pub config_path: PathBuf,
    pub database_path: PathBuf,
    pub available: RootAvailability,
    pub issues: Vec<String>,
}

/// Stores the root/config/database availability flags for one workspace root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RootAvailability {
    pub root_exists: bool,
    pub config_exists: bool,
    pub database_exists: bool,
}

/// Stores one configured project summary enriched with indexed counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub local_path: PathBuf,
    pub sessions_root: PathBuf,
    pub git_upstream: Option<String>,
    pub known_paths: Vec<PathBuf>,
    pub known_path_count: usize,
    pub session_count: u64,
    pub turn_count: u64,
    pub last_activity_at: Option<String>,
}

/// Stores the full workspace query payload for project browsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceQueryData {
    pub root: RootInfo,
    pub projects: Vec<ProjectSummary>,
}

/// Identifies the normalized session shape stored in the shared index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Primary,
    Subagent,
}

/// Stores one indexed session summary for one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionSummary {
    pub project_id: String,
    pub provider: SourceKind,
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub session_kind: SessionKind,
    pub cwd: String,
    pub turn_count: u64,
    pub latest_turn_at: Option<String>,
    pub latest_status: Option<NormalizedTurnStatus>,
}

/// Stores the full session-list query payload for one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionsQueryData {
    pub project_id: String,
    pub sessions: Vec<SessionSummary>,
}

/// Stores one indexed turn summary for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnSummary {
    pub project_id: String,
    pub provider: SourceKind,
    pub session_id: String,
    pub turn_ordinal: u64,
    pub turn_id: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub status: NormalizedTurnStatus,
    pub user_preview: String,
    pub has_final_answer: bool,
    pub step_count: u64,
}

/// Stores the full turn-list query payload for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnsQueryData {
    pub project_id: String,
    pub provider: SourceKind,
    pub session_id: String,
    pub turns: Vec<TurnSummary>,
}

/// Stores one full normalized turn detail payload for one session turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnDetail {
    pub project_id: String,
    pub provider: SourceKind,
    pub session_id: String,
    pub turn_ordinal: u64,
    pub turn_id: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub status: NormalizedTurnStatus,
    pub user_message: String,
    pub final_answer_at: Option<String>,
    pub final_answer_text: Option<String>,
    pub step_count: u64,
    pub steps: Vec<NormalizedTurnStep>,
    pub raw_steps_json: Option<String>,
    pub insights: Option<TurnDetailInsights>,
}

/// Stores one optional derived insights block embedded in a turn detail payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnDetailInsights {
    pub duration_ms: u64,
    pub tool_call_count: u64,
    pub tool_output_count: u64,
    pub attachment_count: u64,
    pub delegation_count: u64,
    pub hook_summary_count: u64,
    pub has_final_answer: bool,
    pub tools: Vec<ToolUsageStat>,
    pub files: Vec<FileUsageStat>,
}

/// Stores one turn-scoped insights payload for one indexed session turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnInsights {
    pub project_id: String,
    pub provider: SourceKind,
    pub session_id: String,
    pub turn_ordinal: u64,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub status: NormalizedTurnStatus,
    pub duration_ms: u64,
    pub step_count: u64,
    pub tool_call_count: u64,
    pub tool_output_count: u64,
    pub attachment_count: u64,
    pub delegation_count: u64,
    pub hook_summary_count: u64,
    pub has_final_answer: bool,
    pub tools: Vec<ToolUsageStat>,
    pub shell_commands: Vec<ShellCommandSummary>,
    pub files: Vec<FileUsageStat>,
}

/// Stores one aggregated tool usage counter for project insights.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolUsageStat {
    pub name: String,
    pub count: u64,
}

/// Stores one shell-like command invocation reported in turn insights.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShellCommandSummary {
    pub tool_name: String,
    pub command_text: String,
    pub workdir: Option<String>,
}

/// Stores one aggregated file access counter for project insights.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileUsageStat {
    pub path: String,
    pub repo_relative_path: Option<String>,
    pub read_count: u64,
    pub write_count: u64,
}

/// Stores one per-day active-time counter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DailyTimeStat {
    pub date: String,
    pub active_time_ms: u64,
}

/// Stores one per-project daily active-time split used in workspace charts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectTimeStat {
    pub project_id: String,
    pub active_time_ms: u64,
}

/// Stores one per-day workspace active-time row with project splits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceDailyTimeStat {
    pub date: String,
    pub active_time_ms: u64,
    pub projects: Vec<ProjectTimeStat>,
}

/// Stores one active session aggregate used by workspace insights.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionRuntimeStat {
    pub project_id: String,
    pub provider: SourceKind,
    pub session_id: String,
    pub started_at: String,
    pub latest_turn_at: String,
    pub active_time_ms: u64,
    pub active_turn_count: u64,
    pub excluded_turn_count: u64,
}

/// Stores one hardest-debugging turn candidate used by project insights.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HardDebuggingTurn {
    pub project_id: String,
    pub provider: SourceKind,
    pub session_id: String,
    pub turn_ordinal: u64,
    pub step_count: u64,
    pub duration_ms: u64,
    pub status: NormalizedTurnStatus,
}

impl HardDebuggingCandidate for HardDebuggingTurn {
    fn project_id(&self) -> &str {
        &self.project_id
    }

    fn provider(&self) -> SourceKind {
        self.provider
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn turn_ordinal(&self) -> u64 {
        self.turn_ordinal
    }

    fn step_count(&self) -> u64 {
        self.step_count
    }

    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
}

/// Stores the workspace insights payload for one host-local reporting window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceInsights {
    pub window_start: String,
    pub window_end: String,
    pub daily_time: Vec<WorkspaceDailyTimeStat>,
    pub recent_sessions: Vec<SessionRuntimeStat>,
    pub active_session_count: u64,
    pub included_turn_count: u64,
    pub excluded_turn_count: u64,
    pub total_time_ms: u64,
}

/// Stores the project insights payload for one indexed project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectInsights {
    pub daily_time: Vec<DailyTimeStat>,
    pub most_common_tools: Vec<ToolUsageStat>,
    pub most_read_files: Vec<FileUsageStat>,
    pub most_written_files: Vec<FileUsageStat>,
    pub hard_debuggings: Vec<HardDebuggingTurn>,
    pub failure_count: u64,
    pub total_time_ms: u64,
}

/// Queries the indexed project aggregates for one workspace database.
pub fn list_project_index_aggregates(index_db_path: &Path) -> Result<Vec<ProjectIndexAggregate>> {
    let connection = open_existing_index_database(index_db_path)?;
    query_project_index_aggregates(&connection)
}

/// Queries the indexed session list for one project.
pub fn query_project_sessions(index_db_path: &Path, project_id: &str) -> Result<SessionsQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    Ok(SessionsQueryData {
        project_id: project_id.to_owned(),
        sessions: query_sessions(&connection, project_id)?,
    })
}

/// Queries the indexed turn list for one session.
pub fn query_session_turns(
    index_db_path: &Path,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
) -> Result<TurnsQueryData> {
    let connection = open_existing_index_database(index_db_path)?;
    Ok(TurnsQueryData {
        project_id: project_id.to_owned(),
        provider,
        session_id: session_id.to_owned(),
        turns: query_turns(&connection, project_id, provider, session_id)?,
    })
}

/// Queries one full normalized turn detail payload.
pub fn query_turn_detail(
    index_db_path: &Path,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
    include_raw: bool,
    include_insights: bool,
) -> Result<TurnDetail> {
    let connection = open_existing_index_database(index_db_path)?;
    build_turn_detail(
        &connection,
        project_id,
        provider,
        session_id,
        turn_ordinal,
        include_raw,
        include_insights,
    )
}

/// Queries one turn insights payload for one indexed provider session turn.
pub fn query_turn_insights(
    index_db_path: &Path,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
) -> Result<TurnInsights> {
    let connection = open_existing_index_database(index_db_path)?;
    build_turn_insights(&connection, project_id, provider, session_id, turn_ordinal)
}

/// Queries one workspace insights payload for a rolling host-local day window.
pub fn query_workspace_insights(
    index_db_path: &Path,
    window_days: u32,
) -> Result<WorkspaceInsights> {
    let connection = open_existing_index_database(index_db_path)?;
    build_workspace_insights(&connection, window_days)
}

/// Queries one project insights payload for one indexed project.
pub fn query_project_insights(
    index_db_path: &Path,
    project_id: &str,
    limit: usize,
) -> Result<ProjectInsights> {
    let connection = open_existing_index_database(index_db_path)?;
    build_project_insights(&connection, project_id, limit)
}

/// Opens one existing index database while still applying lightweight migrations.
pub(crate) fn open_existing_index_database(index_db_path: &Path) -> Result<Connection> {
    if !index_db_path.exists() {
        bail!("index database not found at {}", index_db_path.display());
    }
    open_index_database(index_db_path)
}

/// Queries the stored project aggregates for every indexed project.
fn query_project_index_aggregates(connection: &Connection) -> Result<Vec<ProjectIndexAggregate>> {
    let mut statement = connection
        .prepare(
            "
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
            ",
        )
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

/// Queries the indexed sessions for one configured project.
fn query_sessions(connection: &Connection, project_id: &str) -> Result<Vec<SessionSummary>> {
    let mut statement = connection
        .prepare(
            "
            WITH turn_stats AS (
                SELECT
                    project_id,
                    provider,
                    session_id,
                    COUNT(*) AS turn_count,
                    MAX(turn_ordinal) AS latest_turn_ordinal,
                    MAX(started_at) AS latest_turn_at
                FROM turns
                WHERE project_id = ?1
                GROUP BY project_id, provider, session_id
            )
            SELECT
                s.project_id,
                s.provider,
                s.session_id,
                s.parent_session_id,
                s.session_kind,
                s.cwd,
                COALESCE(turn_stats.turn_count, 0) AS turn_count,
                turn_stats.latest_turn_at,
                latest.status
            FROM sessions AS s
            LEFT JOIN turn_stats
                ON turn_stats.project_id = s.project_id
                AND turn_stats.provider = s.provider
                AND turn_stats.session_id = s.session_id
            LEFT JOIN turns AS latest
                ON latest.project_id = turn_stats.project_id
                AND latest.provider = turn_stats.provider
                AND latest.session_id = turn_stats.session_id
                AND latest.turn_ordinal = turn_stats.latest_turn_ordinal
            WHERE s.project_id = ?1
            ORDER BY
                turn_stats.latest_turn_at IS NULL ASC,
                turn_stats.latest_turn_at DESC,
                s.provider ASC,
                s.session_id DESC
            ",
        )
        .context("failed to prepare indexed session query")?;
    let rows = statement
        .query_map([project_id], |row| {
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
                })
            },
        )
        .collect()
}

/// Queries the indexed turns for one provider session.
fn query_turns(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
) -> Result<Vec<TurnSummary>> {
    let mut statement = connection
        .prepare(
            "
            SELECT
                project_id,
                provider,
                session_id,
                turn_ordinal,
                turn_id,
                started_at,
                completed_at,
                status,
                user_message,
                has_final_answer,
                step_count
            FROM turns
            WHERE project_id = ?1 AND provider = ?2 AND session_id = ?3
            ORDER BY turn_ordinal ASC
            ",
        )
        .context("failed to prepare indexed turn query")?;
    let rows = statement
        .query_map((project_id, provider.directory_name(), session_id), |row| {
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
            ))
        })
        .context("failed to query indexed turns")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read indexed turn rows")?;
    rows.into_iter()
        .map(
            |(
                project_id,
                provider,
                session_id,
                turn_ordinal,
                turn_id,
                started_at,
                completed_at,
                status,
                user_message,
                has_final_answer,
                step_count,
            )|
             -> Result<_> {
                Ok(TurnSummary {
                    project_id,
                    provider: parse_provider(&provider)?,
                    session_id,
                    turn_ordinal: sql_count_to_u64(turn_ordinal)?,
                    turn_id,
                    started_at,
                    completed_at,
                    status: parse_turn_status(&status)?,
                    user_preview: preview_text(&user_message),
                    has_final_answer: has_final_answer != 0,
                    step_count: sql_count_to_u64(step_count)?,
                })
            },
        )
        .collect()
}

/// Builds one normalized turn detail row from the index.
fn build_turn_detail(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
    include_raw: bool,
    include_insights: bool,
) -> Result<TurnDetail> {
    let row = query_indexed_turn_row(connection, project_id, provider, session_id, turn_ordinal)?;
    let insights = include_insights
        .then(|| build_turn_detail_insights(connection, &row))
        .transpose()?;
    row.into_turn_detail(include_raw, insights)
}

/// Builds one turn insights report from indexed turn, tool, and file rows.
pub(crate) fn build_turn_insights(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
) -> Result<TurnInsights> {
    let row = query_indexed_turn_row(connection, project_id, provider, session_id, turn_ordinal)?;
    let insights = build_turn_detail_insights(connection, &row)?;
    let shell_commands =
        query_turn_shell_commands(connection, project_id, provider, session_id, turn_ordinal)?;
    Ok(row.into_turn_insights(insights, shell_commands))
}

/// Builds one workspace insights report from indexed turn rows.
pub(crate) fn build_workspace_insights(
    connection: &Connection,
    window_days: u32,
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

    Ok(WorkspaceInsights {
        window_start: start_date.to_string(),
        window_end: anchor_date.to_string(),
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
        active_session_count: u64::try_from(recent_sessions.len()).unwrap_or(u64::MAX),
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
    limit: usize,
) -> Result<ProjectInsights> {
    let rows = query_project_insight_rows(connection, project_id, limit)?;
    let all_files = query_file_usage_stats(
        connection,
        FileUsageScope::RecentProject { project_id, limit },
    )?;
    let mut most_common_tools = query_tool_usage_stats(
        connection,
        ToolUsageScope::RecentProject { project_id, limit },
    )?;
    sort_tool_usage_stats(&mut most_common_tools);
    most_common_tools.truncate(10);
    let mut daily_time_map = BTreeMap::<String, u64>::new();
    let mut failure_count = 0_u64;
    let mut total_time_ms = 0_u64;
    let mut hard_debuggings = Vec::new();

    for row in rows {
        if row.status != NormalizedTurnStatus::Completed {
            failure_count = failure_count.saturating_add(1);
        }

        if should_include_turn_in_active_time(row.status, row.duration_ms) {
            total_time_ms = total_time_ms.saturating_add(row.duration_ms);
            let total = daily_time_map.entry(row.local_date.clone()).or_insert(0);
            *total = total.saturating_add(row.duration_ms);
        }

        hard_debuggings.push(HardDebuggingTurn {
            project_id: row.project_id.clone(),
            provider: row.provider,
            session_id: row.session_id.clone(),
            turn_ordinal: row.turn_ordinal,
            step_count: row.step_count,
            duration_ms: row.duration_ms,
            status: row.status,
        });
    }

    rank_hard_debuggings(&mut hard_debuggings);

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
        hard_debuggings,
        failure_count,
        total_time_ms,
    })
}

/// Queries one indexed turn row used by turn detail and turn insights.
fn query_indexed_turn_row(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
) -> Result<IndexedTurnRow> {
    let turn_ordinal =
        i64::try_from(turn_ordinal).context("turn ordinal exceeds SQLite INTEGER range")?;
    let row = connection
        .query_row(
            "
            SELECT
                project_id,
                provider,
                session_id,
                turn_ordinal,
                turn_id,
                started_at,
                completed_at,
                status,
                user_message,
                final_answer_at,
                final_answer_text,
                steps_json,
                COALESCE(duration_ms, 0),
                COALESCE(step_count, 0),
                COALESCE(tool_call_count, 0),
                COALESCE(tool_output_count, 0),
                COALESCE(attachment_count, 0),
                COALESCE(delegation_count, 0),
                COALESCE(hook_summary_count, 0),
                has_final_answer
            FROM turns
            WHERE project_id = ?1 AND provider = ?2 AND session_id = ?3 AND turn_ordinal = ?4
            ",
            (
                project_id,
                provider.directory_name(),
                session_id,
                turn_ordinal,
            ),
            |row| {
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
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, i64>(13)?,
                    row.get::<_, i64>(14)?,
                    row.get::<_, i64>(15)?,
                    row.get::<_, i64>(16)?,
                    row.get::<_, i64>(17)?,
                    row.get::<_, i64>(18)?,
                    row.get::<_, i64>(19)?,
                ))
            },
        )
        .with_context(|| {
            format!(
                "turn {turn_ordinal} was not found in session {session_id} for provider {}",
                provider.directory_name()
            )
        })?;
    Ok(IndexedTurnRow {
        project_id: row.0,
        provider: parse_provider(&row.1)?,
        session_id: row.2,
        turn_ordinal: sql_count_to_u64(row.3)?,
        turn_id: row.4,
        started_at: row.5,
        completed_at: row.6,
        status: parse_turn_status(&row.7)?,
        user_message: row.8,
        final_answer_at: row.9,
        final_answer_text: row.10,
        steps_json: row.11,
        duration_ms: sql_count_to_u64(row.12)?,
        step_count: sql_count_to_u64(row.13)?,
        tool_call_count: sql_count_to_u64(row.14)?,
        tool_output_count: sql_count_to_u64(row.15)?,
        attachment_count: sql_count_to_u64(row.16)?,
        delegation_count: sql_count_to_u64(row.17)?,
        hook_summary_count: sql_count_to_u64(row.18)?,
        has_final_answer: row.19 != 0,
    })
}

/// Queries the latest indexed host-local day in one workspace database.
fn query_latest_local_date(connection: &Connection) -> Result<Option<String>> {
    connection
        .query_row(
            "SELECT MAX(DATE(started_at, 'localtime')) FROM turns",
            [],
            |row| row.get(0),
        )
        .context("failed to query latest indexed local day")
}

/// Queries the current host-local civil day according to SQLite localtime rules.
fn query_local_today(connection: &Connection) -> Result<LocalDate> {
    let value = connection
        .query_row("SELECT DATE('now', 'localtime')", [], |row| {
            row.get::<_, String>(0)
        })
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
        .prepare(
            "
            SELECT
                project_id,
                provider,
                session_id,
                started_at,
                DATE(started_at, 'localtime'),
                status,
                COALESCE(duration_ms, 0)
            FROM turns
            WHERE DATE(started_at, 'localtime') >= ?1 AND DATE(started_at, 'localtime') < ?2
            ORDER BY started_at ASC, project_id ASC, provider ASC, session_id ASC, turn_ordinal ASC
            ",
        )
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
    limit: usize,
) -> Result<Vec<ProjectInsightRow>> {
    let limit =
        i64::try_from(limit).context("project insights limit exceeds SQLite INTEGER range")?;
    let mut statement = connection
        .prepare(
            "
            SELECT
                project_id,
                provider,
                session_id,
                turn_ordinal,
                DATE(started_at, 'localtime'),
                status,
                COALESCE(step_count, 0),
                COALESCE(duration_ms, 0)
            FROM turns
            WHERE project_id = ?1
            ORDER BY started_at DESC, provider ASC, session_id ASC, turn_ordinal ASC
            LIMIT ?2
            ",
        )
        .context("failed to prepare project insights query")?;
    let rows = statement
        .query_map((project_id, limit), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })
        .context("failed to query project insight rows")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read project insight rows")?;
    rows.into_iter()
        .map(
            |(
                project_id,
                provider,
                session_id,
                turn_ordinal,
                local_date,
                status,
                step_count,
                duration_ms,
            )|
             -> Result<_> {
                Ok(ProjectInsightRow {
                    project_id,
                    provider: parse_provider(&provider)?,
                    session_id,
                    turn_ordinal: sql_count_to_u64(turn_ordinal)?,
                    local_date,
                    status: parse_turn_status(&status)?,
                    step_count: sql_count_to_u64(step_count)?,
                    duration_ms: sql_count_to_u64(duration_ms)?,
                })
            },
        )
        .collect()
}

/// Queries grouped tool usage stats for one query scope.
fn query_tool_usage_stats(
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
                .prepare(
                    "
                    SELECT tool_name, COUNT(*) AS call_count
                    FROM tool_calls
                    WHERE project_id = ?1
                        AND provider = ?2
                        AND session_id = ?3
                        AND turn_ordinal = ?4
                        AND tool_name IS NOT NULL
                    GROUP BY tool_name
                    ",
                )
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
        ToolUsageScope::RecentProject { project_id, limit } => {
            let limit = i64::try_from(limit)
                .context("project insights limit exceeds SQLite INTEGER range")?;
            let mut statement = connection
                .prepare(
                    "
                    WITH recent_turns AS (
                        SELECT project_id, provider, session_id, turn_ordinal
                        FROM turns
                        WHERE project_id = ?1
                        ORDER BY started_at DESC, provider ASC, session_id ASC, turn_ordinal ASC
                        LIMIT ?2
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
                    ",
                )
                .context("failed to prepare project tool usage query")?;
            statement
                .query_map((project_id, limit), |row| {
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
fn query_file_usage_stats(
    connection: &Connection,
    scope: FileUsageScope<'_>,
) -> Result<Vec<FileUsageStat>> {
    let rows = match scope {
        FileUsageScope::Turn {
            project_id,
            provider,
            session_id,
            turn_ordinal,
        } => {
            let turn_ordinal =
                i64::try_from(turn_ordinal).context("turn ordinal exceeds SQLite INTEGER range")?;
            let mut statement = connection
                .prepare(
                    "
                    SELECT
                        path,
                        MIN(repo_relative_path) AS repo_relative_path,
                        SUM(CASE WHEN access_type IN ('read', 'list') THEN 1 ELSE 0 END) AS read_count,
                        SUM(CASE WHEN access_type IN ('write', 'edit') THEN 1 ELSE 0 END) AS write_count
                    FROM file_accesses
                    WHERE project_id = ?1
                        AND provider = ?2
                        AND session_id = ?3
                        AND turn_ordinal = ?4
                    GROUP BY path
                    ",
                )
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
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .context("failed to query turn file usage rows")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("failed to read turn file usage rows")?
        }
        FileUsageScope::RecentProject { project_id, limit } => {
            let limit = i64::try_from(limit)
                .context("project insights limit exceeds SQLite INTEGER range")?;
            let mut statement = connection
                .prepare(
                    "
                    WITH recent_turns AS (
                        SELECT project_id, provider, session_id, turn_ordinal
                        FROM turns
                        WHERE project_id = ?1
                        ORDER BY started_at DESC, provider ASC, session_id ASC, turn_ordinal ASC
                        LIMIT ?2
                    )
                    SELECT
                        file_accesses.path,
                        MIN(file_accesses.repo_relative_path) AS repo_relative_path,
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
                    GROUP BY file_accesses.path
                    ",
                )
                .context("failed to prepare project file usage query")?;
            statement
                .query_map((project_id, limit), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })
                .context("failed to query project file usage rows")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("failed to read project file usage rows")?
        }
    };
    rows.into_iter()
        .map(
            |(path, repo_relative_path, read_count, write_count)| -> Result<_> {
                Ok(FileUsageStat {
                    path,
                    repo_relative_path,
                    read_count: sql_count_to_u64(read_count)?,
                    write_count: sql_count_to_u64(write_count)?,
                })
            },
        )
        .collect()
}

/// Queries shell-like command invocations for one indexed turn.
fn query_turn_shell_commands(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
) -> Result<Vec<ShellCommandSummary>> {
    let turn_ordinal =
        i64::try_from(turn_ordinal).context("turn ordinal exceeds SQLite INTEGER range")?;
    let mut statement = connection
        .prepare(
            "
            SELECT tool_name, arguments_text
            FROM tool_calls
            WHERE project_id = ?1
                AND provider = ?2
                AND session_id = ?3
                AND turn_ordinal = ?4
                AND tool_name IS NOT NULL
                AND arguments_text IS NOT NULL
            ORDER BY call_ordinal ASC
            ",
        )
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

/// Parses one provider value stored in SQLite back into a source kind.
fn parse_provider(value: &str) -> Result<SourceKind> {
    match value {
        "claude" => Ok(SourceKind::Claude),
        "codex" => Ok(SourceKind::Codex),
        other => bail!("unsupported provider `{other}` in index"),
    }
}

/// Parses one indexed session kind value from SQLite.
pub(crate) fn parse_session_kind(value: &str) -> Result<SessionKind> {
    match value {
        "primary" => Ok(SessionKind::Primary),
        "subagent" => Ok(SessionKind::Subagent),
        other => bail!("unsupported session kind `{other}` in index"),
    }
}

/// Parses one indexed turn status value from SQLite.
fn parse_turn_status(value: &str) -> Result<NormalizedTurnStatus> {
    match value {
        "completed" => Ok(NormalizedTurnStatus::Completed),
        "aborted" => Ok(NormalizedTurnStatus::Aborted),
        "incomplete" => Ok(NormalizedTurnStatus::Incomplete),
        other => bail!("unsupported turn status `{other}` in index"),
    }
}

/// Converts one SQLite aggregate count into an unsigned integer.
fn sql_count_to_u64(value: i64) -> Result<u64> {
    u64::try_from(value).context("negative count encountered in SQLite query")
}

/// Normalizes one user message into a single-line turn preview.
fn preview_text(text: &str) -> String {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.len() <= 126 {
        return single_line;
    }
    let mut preview = single_line.chars().take(125).collect::<String>();
    preview.push('…');
    preview
}

/// Stores one internal session aggregate while building workspace insights.
#[derive(Debug, Clone)]
struct SessionAggregate {
    project_id: String,
    provider: SourceKind,
    session_id: String,
    started_at: String,
    latest_turn_at: String,
    active_time_ms: u64,
    active_turn_count: u64,
    excluded_turn_count: u64,
}

/// Stores one indexed turn row used by workspace insights.
#[derive(Debug, Clone)]
struct InsightTurnRow {
    project_id: String,
    provider: SourceKind,
    session_id: String,
    started_at: String,
    local_date: String,
    status: NormalizedTurnStatus,
    duration_ms: u64,
}

/// Stores one indexed turn row used by project insights.
#[derive(Debug, Clone)]
struct ProjectInsightRow {
    project_id: String,
    provider: SourceKind,
    session_id: String,
    turn_ordinal: u64,
    local_date: String,
    status: NormalizedTurnStatus,
    step_count: u64,
    duration_ms: u64,
}

/// Stores one fully decoded indexed turn row reused by turn queries.
#[derive(Debug, Clone)]
struct IndexedTurnRow {
    project_id: String,
    provider: SourceKind,
    session_id: String,
    turn_ordinal: u64,
    turn_id: Option<String>,
    started_at: String,
    completed_at: Option<String>,
    status: NormalizedTurnStatus,
    user_message: String,
    final_answer_at: Option<String>,
    final_answer_text: Option<String>,
    step_count: u64,
    tool_call_count: u64,
    tool_output_count: u64,
    attachment_count: u64,
    delegation_count: u64,
    hook_summary_count: u64,
    has_final_answer: bool,
    duration_ms: u64,
    steps_json: String,
}

impl IndexedTurnRow {
    /// Converts one indexed turn row into the public turn detail payload.
    fn into_turn_detail(
        self,
        include_raw: bool,
        insights: Option<TurnDetailInsights>,
    ) -> Result<TurnDetail> {
        let steps = serde_json::from_str::<Vec<NormalizedTurnStep>>(&self.steps_json)
            .context("failed to parse stored normalized turn steps")?;
        Ok(TurnDetail {
            project_id: self.project_id,
            provider: self.provider,
            session_id: self.session_id,
            turn_ordinal: self.turn_ordinal,
            turn_id: self.turn_id,
            started_at: self.started_at,
            completed_at: self.completed_at,
            status: self.status,
            user_message: self.user_message,
            final_answer_at: self.final_answer_at,
            final_answer_text: self.final_answer_text,
            step_count: self.step_count,
            steps,
            raw_steps_json: include_raw.then_some(self.steps_json),
            insights,
        })
    }

    /// Converts one indexed turn row into the public turn insights payload.
    fn into_turn_insights(
        self,
        insights: TurnDetailInsights,
        shell_commands: Vec<ShellCommandSummary>,
    ) -> TurnInsights {
        TurnInsights {
            project_id: self.project_id,
            provider: self.provider,
            session_id: self.session_id,
            turn_ordinal: self.turn_ordinal,
            started_at: self.started_at,
            completed_at: self.completed_at,
            status: self.status,
            duration_ms: self.duration_ms,
            step_count: self.step_count,
            tool_call_count: insights.tool_call_count,
            tool_output_count: insights.tool_output_count,
            attachment_count: insights.attachment_count,
            delegation_count: insights.delegation_count,
            hook_summary_count: insights.hook_summary_count,
            has_final_answer: insights.has_final_answer,
            tools: insights.tools,
            shell_commands,
            files: insights.files,
        }
    }
}

/// Builds one derived insights block for a turn detail payload.
fn build_turn_detail_insights(
    connection: &Connection,
    turn: &IndexedTurnRow,
) -> Result<TurnDetailInsights> {
    let mut tools = query_tool_usage_stats(
        connection,
        ToolUsageScope::Turn {
            project_id: &turn.project_id,
            provider: turn.provider,
            session_id: &turn.session_id,
            turn_ordinal: turn.turn_ordinal,
        },
    )?;
    sort_tool_usage_stats(&mut tools);

    let mut files = query_file_usage_stats(
        connection,
        FileUsageScope::Turn {
            project_id: &turn.project_id,
            provider: turn.provider,
            session_id: &turn.session_id,
            turn_ordinal: turn.turn_ordinal,
        },
    )?;
    sort_turn_file_usage_stats(&mut files);

    Ok(TurnDetailInsights {
        duration_ms: turn.duration_ms,
        tool_call_count: turn.tool_call_count,
        tool_output_count: turn.tool_output_count,
        attachment_count: turn.attachment_count,
        delegation_count: turn.delegation_count,
        hook_summary_count: turn.hook_summary_count,
        has_final_answer: turn.has_final_answer,
        tools,
        files,
    })
}

/// Identifies the supported grouped tool-usage query scopes.
#[derive(Debug, Clone, Copy)]
enum ToolUsageScope<'a> {
    Turn {
        project_id: &'a str,
        provider: SourceKind,
        session_id: &'a str,
        turn_ordinal: u64,
    },
    RecentProject {
        project_id: &'a str,
        limit: usize,
    },
}

/// Identifies the supported grouped file-usage query scopes.
#[derive(Debug, Clone, Copy)]
enum FileUsageScope<'a> {
    Turn {
        project_id: &'a str,
        provider: SourceKind,
        session_id: &'a str,
        turn_ordinal: u64,
    },
    RecentProject {
        project_id: &'a str,
        limit: usize,
    },
}

/// Sorts grouped tool-usage stats by descending frequency then name.
fn sort_tool_usage_stats(stats: &mut [ToolUsageStat]) {
    stats.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.name.cmp(&right.name))
    });
}

/// Sorts turn file-usage stats by total accesses, writes, reads, then path.
fn sort_turn_file_usage_stats(stats: &mut [FileUsageStat]) {
    stats.sort_by(|left, right| {
        let left_total = left.read_count.saturating_add(left.write_count);
        let right_total = right.read_count.saturating_add(right.write_count);
        right_total
            .cmp(&left_total)
            .then_with(|| right.write_count.cmp(&left.write_count))
            .then_with(|| right.read_count.cmp(&left.read_count))
            .then_with(|| left.path.cmp(&right.path))
    });
}

/// Stores one civil day used for local-day query-window calculations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LocalDate {
    year: i64,
    month: u32,
    day: u32,
}

impl LocalDate {
    /// Parses one `YYYY-MM-DD` civil date string.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split('-');
        let year = parts.next()?.parse().ok()?;
        let month = parts.next()?.parse().ok()?;
        let day = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self { year, month, day })
    }

    /// Offsets one civil date by a whole-number day count.
    pub(crate) fn add_days(self, days: i64) -> Option<Self> {
        let base_days = self.days_since_epoch()?;
        base_days.checked_add(days).map(Self::from_days_since_epoch)
    }

    /// Converts one civil day into Unix days.
    fn days_since_epoch(self) -> Option<i64> {
        if !(1..=12).contains(&self.month) || !(1..=31).contains(&self.day) {
            return None;
        }
        let month = i64::from(self.month);
        let day = i64::from(self.day);
        let adjusted_year = self.year - if month <= 2 { 1 } else { 0 };
        let era = if adjusted_year >= 0 {
            adjusted_year / 400
        } else {
            (adjusted_year - 399) / 400
        };
        let year_of_era = adjusted_year - era * 400;
        let month_of_year = month + if month > 2 { -3 } else { 9 };
        let day_of_year = (153 * month_of_year + 2) / 5 + day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        Some(era * 146_097 + day_of_era - 719_468)
    }

    /// Converts one Unix-day count back into one civil date.
    fn from_days_since_epoch(days: i64) -> Self {
        let z = days + 719_468;
        let era = if z >= 0 {
            z / 146_097
        } else {
            (z - 146_096) / 146_097
        };
        let day_of_era = z - era * 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let mut year = year_of_era + era * 400;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_prime = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
        let month = month_prime + if month_prime < 10 { 3 } else { -9 };
        year += if month <= 2 { 1 } else { 0 };
        Self {
            year,
            month: u32::try_from(month).unwrap_or(1),
            day: u32::try_from(day).unwrap_or(1),
        }
    }
}

impl std::fmt::Display for LocalDate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{:04}-{:02}-{:02}",
            self.year, self.month, self.day
        )
    }
}
