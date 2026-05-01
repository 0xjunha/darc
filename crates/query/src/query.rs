mod bundles;
mod files;
mod insights;
mod projects;
mod search;
mod turns;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
pub use bundles::query_project_session_bundle;
pub use darc_index::evidence::EvidenceField as SearchEvidenceField;
use darc_index::open_index_database;
use darc_paths::SourceKind;
use darc_rollout::model::{NormalizedTokenUsage, NormalizedTurnStatus, NormalizedTurnStep};
pub use files::{display_path_for_access, query_project_files, query_project_session_files};
#[cfg(test)]
pub(crate) use insights::{build_project_insights, build_workspace_insights};
pub use insights::{query_project_insights, query_workspace_insights};
pub use projects::{
    list_project_index_aggregates, lookup_project_session_id, lookup_project_session_matches,
    query_project_sessions, query_project_turns, query_resolve_sessions,
};
use rusqlite::Connection;
pub use search::{SearchSnippetMatcher, query_search_turns, search_snippet_match_range};
use serde::Serialize;
#[cfg(test)]
pub(crate) use turns::build_turn_insights;
pub use turns::{
    TurnExistenceResolver, query_session_turn_details, query_turn_detail, query_turn_exists,
    query_turn_insights,
};

#[cfg(test)]
/// Prepares every read-side SQL statement against one initialized SQLite schema.
pub(crate) fn smoke_test_sql(connection: &Connection) -> Result<()> {
    projects::smoke_test_sql(connection)?;
    files::smoke_test_sql(connection)?;
    turns::smoke_test_sql(connection)?;
    insights::smoke_test_sql(connection)?;
    search::smoke_test_sql(connection)?;
    Ok(())
}

/// Applies offset/limit pagination to a fully ranked in-memory row set.
pub(crate) fn paginate_ranked_rows<T>(
    rows: Vec<T>,
    limit: usize,
    offset: usize,
) -> Result<(Vec<T>, bool)> {
    let page_end = offset
        .checked_add(limit)
        .context("query pagination exceeds usize range")?;
    let has_more = rows.len() > page_end;
    let rows = rows.into_iter().skip(offset).take(limit).collect();
    Ok((rows, has_more))
}

/// Applies one optional matched-path preview cap to an already ordered path list.
pub(crate) fn apply_matched_path_limit(
    mut paths: Vec<String>,
    matched_path_limit: Option<usize>,
) -> (Vec<String>, bool) {
    if let Some(limit) = matched_path_limit
        && paths.len() > limit
    {
        paths.truncate(limit);
        return (paths, true);
    }
    (paths, false)
}

/// Caps `resolve-session` responses to one generous deterministic page.
pub const DEFAULT_RESOLVE_SESSION_MATCH_LIMIT: usize = 50;

/// Caps per-row matched path previews unless callers opt into all matched paths.
pub const DEFAULT_MATCHED_PATH_LIMIT: usize = 20;

/// Caps per-hit exact-search evidence match previews unless callers ask for more.
pub const DEFAULT_SEARCH_MATCH_LIMIT: usize = 20;

/// Caps default row pages for agent-facing browse and search commands.
pub const DEFAULT_QUERY_PAGE_LIMIT: usize = 10;

/// Caps default embedded turn pages in composite session bundles.
pub const DEFAULT_SESSION_BUNDLE_TURN_LIMIT: usize = 5;

/// Caps turn-detail step previews unless callers ask for a larger page.
pub const DEFAULT_TURN_STEP_LIMIT: usize = 10;

/// Caps workspace-insight recent session previews unless callers ask for a larger page.
pub const DEFAULT_WORKSPACE_RECENT_SESSION_LIMIT: usize = 50;

/// Caps embedded session-file rows in composite session bundles.
pub const DEFAULT_SESSION_BUNDLE_FILE_LIMIT: usize = 100;

/// Caps standard user/agent text previews in summary and search rows.
pub const DEFAULT_TEXT_PREVIEW_CHARS: usize = 500;

/// Caps one-line turn-list previews for quick timeline skims.
pub const ONELINE_TEXT_PREVIEW_CHARS: usize = 300;

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
    pub primary_model: Option<String>,
    pub token_usage: Option<NormalizedTokenUsage>,
    pub total_token_count: Option<u64>,
    pub effective_agent_runtime_ms: Option<u64>,
    pub changed_file_count: u64,
    pub added_line_count: u64,
    pub removed_line_count: u64,
    pub first_turn_at: Option<String>,
    pub first_user_prompt: Option<String>,
    pub first_user_prompt_truncated: bool,
    pub first_user_prompt_chars: Option<u64>,
    pub first_user_prompt_total_chars: Option<u64>,
    pub final_agent_message: Option<String>,
    pub final_agent_message_truncated: bool,
    pub final_agent_message_chars: Option<u64>,
    pub final_agent_message_total_chars: Option<u64>,
    pub aborted_turn_count: u64,
    pub edited_files: Vec<String>,
}

/// Identifies the supported session-list projection modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionsView {
    #[default]
    Compact,
    Full,
}

/// Stores the full session-list query payload for one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionsQueryData {
    pub project_id: String,
    pub provider: Option<SourceKind>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub touched_path: Option<String>,
    pub view: SessionsView,
    pub limit: u64,
    pub offset: u64,
    pub has_more: bool,
    pub sessions: Vec<SessionSummary>,
}

/// Collects the supported inputs for one project session-list query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionsQueryRequest<'a> {
    pub project_id: &'a str,
    pub project_root: Option<&'a Path>,
    pub provider: Option<SourceKind>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub touched_path: Option<&'a str>,
    pub view: SessionsView,
    pub limit: usize,
    pub offset: usize,
}

/// Stores one provider plus canonical session id candidate returned by `resolve-session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedSessionMatch {
    pub project_id: String,
    pub provider: SourceKind,
    pub session_id: String,
}

/// Stores the session-resolution payload returned by `darc query resolve-session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolveSessionQueryData {
    pub query: String,
    pub matches: Vec<ResolvedSessionMatch>,
    pub total: u64,
    pub truncated: bool,
}

/// Collects the supported filters for one session-resolution query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveSessionQueryRequest<'a> {
    pub query: &'a str,
    pub project_id: Option<&'a str>,
    pub provider: Option<SourceKind>,
    pub limit: usize,
}

/// Identifies which file-pivot query variant populated one files payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesQueryMode {
    Top,
    Path,
    CoTouchedWith,
}

/// Stores one session ranked by how often it touched one matched file set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileSessionSummary {
    pub provider: SourceKind,
    pub session_id: String,
    pub touch_count: u64,
    pub read_count: u64,
    pub write_count: u64,
    pub first_turn_ordinal: u64,
    pub last_turn_ordinal: u64,
    pub first_touched_at: String,
    pub last_touched_at: String,
    pub matched_paths: Vec<String>,
    pub matched_paths_count: u64,
    pub matched_paths_truncated: bool,
}

/// Stores one file row returned by a file-pivot query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FilePivotSummary {
    pub path: String,
    pub co_touch_count: Option<u64>,
    pub touch_count: Option<u64>,
    pub session_count: Option<u64>,
    pub read_count: Option<u64>,
    pub write_count: Option<u64>,
    pub first_touched_at: Option<String>,
    pub last_touched_at: Option<String>,
}

/// Stores one file-level query payload for most-touched, path, and co-touch ranking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FilesQueryData {
    pub project_id: String,
    pub mode: FilesQueryMode,
    pub provider: Option<SourceKind>,
    pub path: Option<String>,
    pub co_touched_with: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: u64,
    pub offset: u64,
    pub has_more: bool,
    pub matched_path_limit: Option<u64>,
    pub sessions: Vec<FileSessionSummary>,
    pub files: Vec<FilePivotSummary>,
}

/// Collects the supported inputs for one file-pivot query request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilesQueryRequest<'a> {
    pub project_id: &'a str,
    pub project_root: Option<&'a Path>,
    pub provider: Option<SourceKind>,
    pub path: Option<&'a str>,
    pub co_touched_with: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub limit: usize,
    pub offset: usize,
    pub matched_path_limit: Option<usize>,
}

/// Collects the supported inputs for one session-file query request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionFilesQueryRequest<'a> {
    pub project_id: &'a str,
    pub project_root: Option<&'a Path>,
    pub provider: SourceKind,
    pub session_id: &'a str,
    pub limit: usize,
    pub offset: usize,
}

/// Stores one session-scoped per-file access summary row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionFileSummary {
    pub path: String,
    pub repo_relative_path: Option<String>,
    pub read_count: u64,
    pub write_count: u64,
    pub first_turn_ordinal: u64,
    pub last_turn_ordinal: u64,
}

/// Stores the per-file access summary payload for one indexed session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionFilesQueryData {
    pub project_id: String,
    pub provider: SourceKind,
    pub session_id: String,
    pub file_count: u64,
    pub limit: u64,
    pub offset: u64,
    pub has_more: bool,
    pub files: Vec<SessionFileSummary>,
}

/// Identifies which turn-detail projection one session bundle returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionBundleView {
    #[default]
    Full,
    Narrative,
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
    pub user_prompt_preview: String,
    pub user_prompt_preview_chars: u64,
    pub user_prompt_total_chars: u64,
    /// Caches the compact first-line user preview for CLI `--view oneline` rendering.
    #[serde(skip_serializing)]
    pub oneline_user_prompt_preview: String,
    #[serde(skip_serializing)]
    pub oneline_user_prompt_preview_chars: u64,
    #[serde(skip_serializing)]
    pub oneline_user_prompt_total_chars: u64,
    /// Caches the compact first-line answer preview for CLI `--view oneline` rendering.
    #[serde(skip_serializing)]
    pub oneline_agent_answer_preview: Option<String>,
    #[serde(skip_serializing)]
    pub oneline_agent_answer_preview_chars: Option<u64>,
    #[serde(skip_serializing)]
    pub oneline_agent_answer_total_chars: Option<u64>,
    pub agent_answer_preview: Option<String>,
    pub agent_answer_preview_chars: Option<u64>,
    pub agent_answer_total_chars: Option<u64>,
    pub has_final_answer: bool,
    pub step_count: u64,
    pub tool_call_count: u64,
    pub primary_model: Option<String>,
    pub token_usage: Option<NormalizedTokenUsage>,
    pub total_token_count: Option<u64>,
    pub effective_agent_runtime_ms: Option<u64>,
    pub changed_file_count: u64,
    pub added_line_count: u64,
    pub removed_line_count: u64,
}

/// Identifies which turn-list projection one machine client requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TurnsView {
    #[default]
    Full,
    Oneline,
}

/// Stores the full turn-list query payload for one provider session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnsQueryData {
    pub project_id: String,
    pub provider: SourceKind,
    pub session_id: String,
    pub since: Option<String>,
    pub until: Option<String>,
    pub view: TurnsView,
    pub limit: u64,
    pub offset: u64,
    pub has_more: bool,
    pub turns: Vec<TurnSummary>,
}

/// Collects the supported filters for one machine-readable session-scoped turn-list query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnsQueryRequest<'a> {
    pub project_id: &'a str,
    pub provider: SourceKind,
    pub session_id: &'a str,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub view: TurnsView,
    pub limit: usize,
    pub offset: usize,
}

/// Identifies the supported turn-search modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Keyword,
    Literal,
    Regex,
    FileName,
    FilePath,
    PathFragment,
}

/// Stores one paginated turn-search response for one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchTurnsQueryData {
    pub project_id: String,
    pub mode: SearchMode,
    pub query: String,
    pub include_tool_output: bool,
    pub fields: Vec<String>,
    pub excluded_fields: Vec<String>,
    pub provider: Option<SourceKind>,
    pub session_id: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: u64,
    pub offset: u64,
    pub has_more: bool,
    pub matched_path_limit: Option<u64>,
    pub match_limit: Option<u64>,
    pub hits: Vec<SearchTurnHit>,
}

/// Collects the supported filters and pagination inputs for one turn-search query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchTurnsRequest<'a> {
    pub project_id: &'a str,
    pub project_root: Option<&'a Path>,
    pub mode: SearchMode,
    pub query: &'a str,
    pub include_tool_output: bool,
    pub fields: &'a [SearchEvidenceField],
    pub excluded_fields: &'a [SearchEvidenceField],
    pub provider: Option<SourceKind>,
    pub session_id: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub limit: usize,
    pub offset: usize,
    pub matched_path_limit: Option<usize>,
    pub match_limit: Option<usize>,
}

/// Stores one field-level evidence match nested inside a turn search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchTurnMatch {
    pub evidence_ordinal: u64,
    pub field: String,
    pub snippet: String,
}

/// Stores one turn hit returned by the search protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchTurnHit {
    pub provider: SourceKind,
    pub session_id: String,
    pub turn_ordinal: u64,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub status: NormalizedTurnStatus,
    pub user_prompt_preview: String,
    pub user_prompt_preview_chars: u64,
    pub user_prompt_total_chars: u64,
    pub agent_answer_preview: Option<String>,
    pub agent_answer_preview_chars: Option<u64>,
    pub agent_answer_total_chars: Option<u64>,
    pub snippet: Option<String>,
    pub matched_paths: Vec<String>,
    pub matched_paths_count: u64,
    pub matched_paths_truncated: bool,
    pub matches: Vec<SearchTurnMatch>,
    pub matches_count: u64,
    pub matches_truncated: bool,
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
    pub step_limit: u64,
    pub step_offset: u64,
    pub steps_has_more: bool,
    pub steps: Vec<NormalizedTurnStep>,
    pub raw_steps_json: Option<String>,
    pub insights: Option<TurnDetailInsights>,
}

/// Stores one composite session bundle for extractor-friendly single-call reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionBundleQueryData {
    pub project_id: String,
    pub provider: SourceKind,
    pub session_id: String,
    pub session_view: SessionsView,
    pub view: SessionBundleView,
    pub turn_limit: u64,
    pub turn_offset: u64,
    pub turns_has_more: bool,
    pub session_file_limit: u64,
    pub session_file_count: u64,
    pub session_files_has_more: bool,
    pub step_limit: u64,
    pub step_offset: u64,
    pub session: SessionSummary,
    pub turns: Vec<TurnDetail>,
    pub session_files: SessionFilesQueryData,
}

/// Collects the supported filters for one composite session bundle query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionBundleQueryRequest<'a> {
    pub project_id: &'a str,
    pub provider: SourceKind,
    pub session_id: &'a str,
    pub project_root: Option<&'a Path>,
    pub session_view: SessionsView,
    pub view: SessionBundleView,
    pub turn_limit: usize,
    pub turn_offset: usize,
    pub step_limit: usize,
    pub step_offset: usize,
}

/// Stores one turn-detail projection and enrichment configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnDetailOptions {
    pub include_raw: bool,
    pub include_insights: bool,
    pub narrative: bool,
    pub step_limit: usize,
    pub step_offset: usize,
}

impl Default for TurnDetailOptions {
    fn default() -> Self {
        Self {
            include_raw: false,
            include_insights: false,
            narrative: false,
            step_limit: DEFAULT_TURN_STEP_LIMIT,
            step_offset: 0,
        }
    }
}

/// Stores one optional derived insights block embedded in a turn detail payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnDetailInsights {
    pub primary_model: Option<String>,
    pub token_usage: Option<NormalizedTokenUsage>,
    pub duration_ms: u64,
    pub effective_agent_runtime_ms: Option<u64>,
    pub total_token_count: Option<u64>,
    pub changed_file_count: u64,
    pub added_line_count: u64,
    pub removed_line_count: u64,
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
    pub primary_model: Option<String>,
    pub token_usage: Option<NormalizedTokenUsage>,
    pub duration_ms: u64,
    pub effective_agent_runtime_ms: Option<u64>,
    pub total_token_count: Option<u64>,
    pub changed_file_count: u64,
    pub added_line_count: u64,
    pub removed_line_count: u64,
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

/// Stores the workspace insights payload for one host-local reporting window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceInsights {
    pub window_start: String,
    pub window_end: String,
    pub recent_session_limit: u64,
    pub recent_session_offset: u64,
    pub recent_sessions_has_more: bool,
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
    pub provider: Option<SourceKind>,
    pub turn_limit: u64,
    pub inspected_turn_count: u64,
    pub turns_has_more: bool,
    pub daily_time: Vec<DailyTimeStat>,
    pub most_common_tools: Vec<ToolUsageStat>,
    pub most_read_files: Vec<FileUsageStat>,
    pub most_written_files: Vec<FileUsageStat>,
    pub failure_count: u64,
    pub total_time_ms: u64,
}

/// Opens one existing index database while still applying lightweight migrations.
pub(crate) fn open_existing_index_database(index_db_path: &Path) -> Result<Connection> {
    if !index_db_path.exists() {
        bail!("index database not found at {}", index_db_path.display());
    }
    open_index_database(index_db_path)
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

/// Converts one nullable SQLite aggregate count into an optional unsigned integer.
fn optional_sql_count_to_u64(value: Option<i64>) -> Result<Option<u64>> {
    value.map(sql_count_to_u64).transpose()
}

/// Stores one normalized text preview plus source-size metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextPreview {
    text: String,
    truncated: bool,
    chars: u64,
    total_chars: u64,
}

/// Normalizes one text field into a single-line preview with metadata.
fn preview_text(text: &str) -> TextPreview {
    preview_text_with_limit(text, DEFAULT_TEXT_PREVIEW_CHARS)
}

/// Normalizes one text field into a capped single-line preview with metadata.
fn preview_text_with_limit(text: &str, max_chars: usize) -> TextPreview {
    preview_normalized_text(&normalize_preview_whitespace(text), max_chars)
}

/// Normalizes one text field's first line into a capped single-line preview with metadata.
fn preview_first_line(text: &str) -> TextPreview {
    preview_text_first_line_with_limit(text, ONELINE_TEXT_PREVIEW_CHARS)
}

/// Normalizes one text field's first line into a capped single-line preview with metadata.
fn preview_text_first_line_with_limit(text: &str, max_chars: usize) -> TextPreview {
    preview_normalized_text(
        &normalize_preview_whitespace(text.lines().next().unwrap_or_default()),
        max_chars,
    )
}

/// Collapses one preview string's whitespace into single spaces.
fn normalize_preview_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Builds metadata for one already normalized text preview.
fn preview_normalized_text(text: &str, max_chars: usize) -> TextPreview {
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return TextPreview {
            text: text.to_owned(),
            truncated: false,
            chars: u64::try_from(total_chars).unwrap_or(u64::MAX),
            total_chars: u64::try_from(total_chars).unwrap_or(u64::MAX),
        };
    }
    let mut preview = text
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    preview.push('…');
    let chars = preview.chars().count();
    TextPreview {
        text: preview,
        truncated: true,
        chars: u64::try_from(chars).unwrap_or(u64::MAX),
        total_chars: u64::try_from(total_chars).unwrap_or(u64::MAX),
    }
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
    local_date: String,
    status: NormalizedTurnStatus,
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
    primary_model: Option<String>,
    token_usage: Option<NormalizedTokenUsage>,
    duration_ms: u64,
    effective_agent_runtime_ms: Option<u64>,
    total_token_count: Option<u64>,
    changed_file_count: u64,
    added_line_count: u64,
    removed_line_count: u64,
    steps_json: String,
}

impl IndexedTurnRow {
    /// Converts one indexed turn row into the public turn detail payload.
    fn into_turn_detail(
        self,
        options: TurnDetailOptions,
        insights: Option<TurnDetailInsights>,
    ) -> Result<TurnDetail> {
        if options.include_raw && options.narrative {
            bail!("raw turn payloads require full turn detail view");
        }
        let steps = serde_json::from_str::<Vec<NormalizedTurnStep>>(&self.steps_json)
            .context("failed to parse stored normalized turn steps")?;
        let (steps, steps_has_more) = page_turn_steps(steps, options)?;
        let steps = if options.narrative {
            steps.into_iter().map(to_narrative_turn_step).collect()
        } else {
            steps
        };
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
            step_limit: u64::try_from(options.step_limit)
                .context("query limit exceeds u64 range")?,
            step_offset: u64::try_from(options.step_offset)
                .context("query offset exceeds u64 range")?,
            steps_has_more,
            steps,
            raw_steps_json: (options.include_raw && !options.narrative).then_some(self.steps_json),
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
            primary_model: self.primary_model,
            token_usage: self.token_usage,
            duration_ms: self.duration_ms,
            effective_agent_runtime_ms: self.effective_agent_runtime_ms,
            total_token_count: self.total_token_count,
            changed_file_count: self.changed_file_count,
            added_line_count: self.added_line_count,
            removed_line_count: self.removed_line_count,
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

/// Applies step-level pagination to one parsed turn-detail step list.
fn page_turn_steps(
    steps: Vec<NormalizedTurnStep>,
    options: TurnDetailOptions,
) -> Result<(Vec<NormalizedTurnStep>, bool)> {
    let page_end = options
        .step_offset
        .checked_add(options.step_limit)
        .context("query pagination exceeds usize range")?;
    let has_more = steps.len() > page_end;
    let steps = steps
        .into_iter()
        .skip(options.step_offset)
        .take(options.step_limit)
        .collect();
    Ok((steps, has_more))
}

/// Projects one normalized turn step into one narrative-only view.
fn to_narrative_turn_step(step: NormalizedTurnStep) -> NormalizedTurnStep {
    match step {
        NormalizedTurnStep::Reasoning { .. } | NormalizedTurnStep::Commentary { .. } => step,
        NormalizedTurnStep::ToolCall {
            timestamp,
            call_id,
            name,
            ..
        } => NormalizedTurnStep::ToolCall {
            timestamp,
            call_id,
            name,
            arguments: String::new(),
        },
        NormalizedTurnStep::ToolCallOutput {
            timestamp, call_id, ..
        } => NormalizedTurnStep::ToolCallOutput {
            timestamp,
            call_id,
            output: String::new(),
        },
        NormalizedTurnStep::Attachment {
            timestamp,
            attachment_type,
            ..
        } => NormalizedTurnStep::Attachment {
            timestamp,
            attachment_type,
            payload_json: String::new(),
        },
        NormalizedTurnStep::Delegation {
            timestamp,
            call_id,
            task_id,
            event,
            agent_id,
            agent_type,
            status,
            summary,
            ..
        } => NormalizedTurnStep::Delegation {
            timestamp,
            call_id,
            task_id,
            event,
            agent_id,
            agent_type,
            status,
            summary,
            payload_json: String::new(),
        },
        NormalizedTurnStep::HookSummary {
            timestamp,
            call_id,
            hook_count,
            prevented_continuation,
            has_output,
            level,
            ..
        } => NormalizedTurnStep::HookSummary {
            timestamp,
            call_id,
            hook_count,
            prevented_continuation,
            has_output,
            level,
            payload_json: String::new(),
        },
        NormalizedTurnStep::ProviderResponseItem {
            timestamp,
            item_type,
            ..
        } => NormalizedTurnStep::ProviderResponseItem {
            timestamp,
            item_type,
            payload_json: String::new(),
        },
    }
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
        provider: Option<SourceKind>,
        limit: usize,
    },
}

/// Identifies the supported grouped file-usage query scopes.
#[derive(Debug, Clone, Copy)]
enum FileUsageScope<'a> {
    Turn {
        project_id: &'a str,
        project_root: Option<&'a Path>,
        provider: SourceKind,
        session_id: &'a str,
        turn_ordinal: u64,
    },
    RecentProject {
        project_id: &'a str,
        project_root: Option<&'a Path>,
        provider: Option<SourceKind>,
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
