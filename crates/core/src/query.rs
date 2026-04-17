use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use darc_index::INDEX_DB_FILE_NAME;
use darc_paths::{
    SourceKind, parse_utc_timestamp, resolve_query_time_bound as resolve_shared_query_time_bound,
};
pub use darc_query::{
    CoTouchedFileSummary, DailyTimeStat, FileSessionSummary, FileUsageStat, FilesQueryData,
    FilesQueryMode, FilesQueryRequest, HardDebuggingTurn, ProjectInsights, ProjectSummary,
    ProjectTimeStat, RootAvailability, RootInfo, SearchMode, SearchTurnHit, SearchTurnsQueryData,
    SearchTurnsRequest, SessionFileSummary, SessionFilesQueryData, SessionKind, SessionRuntimeStat,
    SessionSummary, SessionsQueryData, ShellCommandSummary, ToolUsageStat, TurnDetail,
    TurnDetailInsights, TurnDetailOptions, TurnInsights, TurnMatchKind, TurnMatchesQueryData,
    TurnMatchesQueryRequest, TurnSearchRole, TurnSummary, TurnsQueryData, TurnsQueryRequest,
    WorkspaceDailyTimeStat, WorkspaceInsights, WorkspaceQueryData,
};
use darc_query::{
    ProjectIndexAggregate, list_project_index_aggregates, query_project_files,
    query_project_insights, query_project_session_files, query_project_sessions,
    query_project_turn_matches as query_index_turn_matches,
    query_project_turns as query_index_turns, query_search_turns as query_project_search_turns,
    query_session_turn_details as query_project_session_turn_details, query_turn_detail,
    query_turn_insights, query_workspace_insights,
};
use darc_wiki::{
    ContextWikiLayout, DigestId, DigestSummary, EntryId, EntryStatus, EntrySummary, EntryType,
    RunId, RunPhase, RunState, RunStatus, RunSummary, list_digests, list_entries,
    load_digest_detail, load_entry_detail, load_registry,
};
use serde::Serialize;

use crate::{
    config::{ProjectConfig, SharedConfig, load_config},
    constants::CONFIG_FILE_NAME,
    default_root_path,
    init::normalize_project_config,
    wiki::{
        DigestResultArtifact, DigestRuntimeArtifact, DigestValidationArtifact,
        load_project_wiki_run_from_layout, load_visible_run_summaries,
    },
};

/// Queries the workspace sidebar payload for one darc root.
pub fn query_workspace(root: Option<PathBuf>) -> WorkspaceQueryData {
    let mut root_info = inspect_root(root);
    let Some(config_path) = root_info
        .available
        .config_exists
        .then(|| root_info.config_path.clone())
    else {
        return WorkspaceQueryData {
            root: root_info,
            projects: Vec::new(),
        };
    };

    let config = match load_normalized_config(&config_path) {
        Ok(config) => config,
        Err(error) => {
            root_info.issues.push(format!(
                "Darc config.toml could not be parsed: {}",
                error.root_cause()
            ));
            return WorkspaceQueryData {
                root: root_info,
                projects: Vec::new(),
            };
        }
    };

    let aggregate_map = if root_info.available.database_exists {
        match list_project_index_aggregates(&root_info.database_path) {
            Ok(aggregates) => aggregate_map(aggregates),
            Err(error) => {
                root_info.issues.push(format!(
                    "Darc database could not be queried: {}",
                    error.root_cause()
                ));
                Default::default()
            }
        }
    } else {
        Default::default()
    };

    let mut projects = config
        .projects
        .into_iter()
        .map(|project| {
            let aggregate = aggregate_map.get(&project.id);
            ProjectSummary {
                id: project.id,
                name: project.name,
                local_path: project.local_path,
                sessions_root: project.sessions_root,
                git_upstream: project.git_upstream,
                known_path_count: project.known_paths.len(),
                known_paths: project.known_paths,
                session_count: aggregate.map_or(0, |aggregate| aggregate.session_count),
                turn_count: aggregate.map_or(0, |aggregate| aggregate.turn_count),
                last_activity_at: aggregate
                    .and_then(|aggregate| aggregate.last_activity_at.clone()),
            }
        })
        .collect::<Vec<_>>();
    projects.sort_by(|left, right| {
        right
            .last_activity_at
            .cmp(&left.last_activity_at)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });

    WorkspaceQueryData {
        root: root_info,
        projects,
    }
}

/// Queries the session-list payload for one configured project.
pub fn query_sessions(
    root: Option<PathBuf>,
    project_id: &str,
    since: Option<&str>,
    until: Option<&str>,
    touched_path: Option<&str>,
) -> Result<SessionsQueryData> {
    let context = load_project_query_context(root, project_id)?;
    query_project_sessions(
        &context.root.database_path,
        &context.project.id,
        Some(context.project.local_path.as_path()),
        since,
        until,
        touched_path,
    )
}

/// Queries one file-pivot payload for one configured project.
pub fn query_files(
    root: Option<PathBuf>,
    request: FilesQueryRequest<'_>,
) -> Result<FilesQueryData> {
    let context = load_project_query_context(root, request.project_id)?;
    query_project_files(
        &context.root.database_path,
        FilesQueryRequest {
            project_id: &context.project.id,
            project_root: Some(context.project.local_path.as_path()),
            ..request
        },
    )
}

/// Queries one session-scoped per-file access summary payload.
pub fn query_session_files(
    root: Option<PathBuf>,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
) -> Result<SessionFilesQueryData> {
    let context = load_project_query_context(root, project_id)?;
    query_project_session_files(
        &context.root.database_path,
        &context.project.id,
        provider,
        session_id,
        Some(context.project.local_path.as_path()),
    )
}

/// Queries the turn-list payload for one configured provider session.
pub fn query_turns(
    root: Option<PathBuf>,
    request: TurnsQueryRequest<'_>,
) -> Result<TurnsQueryData> {
    let context = load_project_query_context(root, request.project_id)?;
    query_index_turns(
        &context.root.database_path,
        TurnsQueryRequest {
            project_id: &context.project.id,
            ..request
        },
    )
}

/// Queries the grep-scoped turn-match payload for one configured project.
pub fn query_turn_matches(
    root: Option<PathBuf>,
    request: TurnMatchesQueryRequest<'_>,
) -> Result<TurnMatchesQueryData> {
    let context = load_project_query_context(root, request.project_id)?;
    query_index_turn_matches(
        &context.root.database_path,
        TurnMatchesQueryRequest {
            project_id: &context.project.id,
            project_root: Some(context.project.local_path.as_path()),
            ..request
        },
    )
}

/// Queries one full turn-detail payload for one configured provider session turn.
pub fn query_turn(
    root: Option<PathBuf>,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
    options: TurnDetailOptions,
) -> Result<TurnDetail> {
    let context = load_project_query_context(root, project_id)?;
    query_turn_detail(
        &context.root.database_path,
        &context.project.id,
        provider,
        session_id,
        turn_ordinal,
        options,
    )
}

/// Queries every full turn-detail payload for one configured provider session.
pub fn query_session_turn_details(
    root: Option<PathBuf>,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    options: TurnDetailOptions,
) -> Result<Vec<TurnDetail>> {
    let context = load_project_query_context(root, project_id)?;
    query_project_session_turn_details(
        &context.root.database_path,
        &context.project.id,
        provider,
        session_id,
        options,
    )
}

/// Queries the turn insights payload for one configured provider session turn.
pub fn query_turn_insight_report(
    root: Option<PathBuf>,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
) -> Result<TurnInsights> {
    let context = load_project_query_context(root, project_id)?;
    query_turn_insights(
        &context.root.database_path,
        &context.project.id,
        provider,
        session_id,
        turn_ordinal,
    )
}

/// Queries one paginated turn-search payload for one configured project.
pub fn query_search_turns(
    root: Option<PathBuf>,
    request: SearchTurnsRequest<'_>,
) -> Result<SearchTurnsQueryData> {
    let context = load_project_query_context(root, request.project_id)?;
    query_project_search_turns(
        &context.root.database_path,
        SearchTurnsRequest {
            project_id: &context.project.id,
            ..request
        },
    )
}

/// Queries the workspace insights payload for one darc root and host-local day window.
pub fn query_workspace_insight_report(
    root: Option<PathBuf>,
    window_days: u32,
) -> Result<WorkspaceInsights> {
    let root_info = inspect_root(root);
    ensure_database_exists(&root_info)?;
    query_workspace_insights(&root_info.database_path, window_days)
}

/// Queries the project insights payload for one configured project.
pub fn query_project_insight_report(
    root: Option<PathBuf>,
    project_id: &str,
    limit: usize,
) -> Result<ProjectInsights> {
    let context = load_project_query_context(root, project_id)?;
    query_project_insights(&context.root.database_path, &context.project.id, limit)
}

/// Stores the registry payload for one project-scoped wiki query.
#[derive(Debug, Clone, Serialize)]
pub struct WikiRegistryQueryData {
    pub project_id: String,
    pub schema_version: u32,
    pub categories: Vec<String>,
    pub domains: Vec<String>,
}

/// Stores the supported filters for one project-scoped wiki entry list query.
#[derive(Debug, Clone, Default)]
pub struct WikiEntriesQueryOptions {
    pub category: Option<String>,
    pub domain: Option<String>,
    pub status: Option<EntryStatus>,
}

/// Stores the entry-list payload for one project-scoped wiki query.
#[derive(Debug, Clone, Serialize)]
pub struct WikiEntriesQueryData {
    pub project_id: String,
    pub entries: Vec<WikiEntryListItem>,
}

/// Stores one project-scoped wiki entry detail payload for the read protocol.
#[derive(Debug, Clone, Serialize)]
pub struct WikiEntryQueryData {
    pub project_id: String,
    pub entry: WikiEntryDetailItem,
}

/// Stores the supported limits for one project-scoped wiki digest list query.
#[derive(Debug, Clone, Default)]
pub struct WikiDigestsQueryOptions {
    pub limit: Option<usize>,
    pub since: Option<String>,
    pub until: Option<String>,
}

/// Stores the digest-list payload for one project-scoped wiki query.
#[derive(Debug, Clone, Serialize)]
pub struct WikiDigestsQueryData {
    pub project_id: String,
    pub since: Option<String>,
    pub until: Option<String>,
    pub digests: Vec<WikiDigestListItem>,
}

/// Stores one project-scoped wiki digest detail payload for the read protocol.
#[derive(Debug, Clone, Serialize)]
pub struct WikiDigestQueryData {
    pub project_id: String,
    pub digest: WikiDigestDetailItem,
}

/// Stores the supported filters for one project-scoped wiki run list query.
#[derive(Debug, Clone, Default)]
pub struct WikiRunsQueryOptions {
    pub status: Option<RunStatus>,
    pub limit: Option<usize>,
    pub since: Option<String>,
    pub until: Option<String>,
}

/// Stores the run-list payload for one project-scoped wiki query.
#[derive(Debug, Clone, Serialize)]
pub struct WikiRunsQueryData {
    pub project_id: String,
    pub since: Option<String>,
    pub until: Option<String>,
    pub runs: Vec<WikiRunListItem>,
}

/// Stores the run-detail payload for one project-scoped wiki query.
#[derive(Debug, Clone, Serialize)]
pub struct WikiRunQueryData {
    pub project_id: String,
    pub run: WikiRunDetailItem,
}

/// Stores one API-shaped wiki entry row for the read protocol.
#[derive(Debug, Clone, Serialize)]
pub struct WikiEntryListItem {
    pub entry_id: EntryId,
    pub display_id: Option<String>,
    pub entry_type: EntryType,
    pub title: String,
    pub category: String,
    pub domains: Vec<String>,
    pub status: EntryStatus,
    pub created_at: String,
    pub updated_at: String,
}

impl From<EntrySummary> for WikiEntryListItem {
    fn from(summary: EntrySummary) -> Self {
        Self {
            entry_id: summary.entry_id,
            display_id: summary.display_id,
            entry_type: summary.entry_type,
            title: summary.title,
            category: summary.category,
            domains: summary.domains,
            status: summary.status,
            created_at: summary.created_at,
            updated_at: summary.updated_at,
        }
    }
}

impl WikiEntryDetailItem {
    /// Builds one entry-detail payload from one canonical entry document detail.
    fn from_document(document: darc_wiki::EntryDetailDocument) -> Self {
        Self {
            entry_id: document.frontmatter.entry_id,
            display_id: document.frontmatter.display_id,
            entry_type: document.frontmatter.entry_type,
            title: document.frontmatter.title,
            category: document.frontmatter.category,
            domains: document.frontmatter.domains,
            status: document.frontmatter.status,
            created_at: document.frontmatter.created_at,
            updated_at: document.frontmatter.updated_at,
            decision_date: document.frontmatter.decision_date,
            evidence: document.frontmatter.evidence,
            created_by_run_id: document.frontmatter.created_by_run_id,
            updated_by_run_id: document.frontmatter.updated_by_run_id,
            supersedes: document.frontmatter.supersedes,
            body_markdown: document.body_markdown,
        }
    }
}

/// Stores one API-shaped wiki entry detail payload for the read protocol.
#[derive(Debug, Clone, Serialize)]
pub struct WikiEntryDetailItem {
    pub entry_id: EntryId,
    pub display_id: Option<String>,
    pub entry_type: EntryType,
    pub title: String,
    pub category: String,
    pub domains: Vec<String>,
    pub status: EntryStatus,
    pub created_at: String,
    pub updated_at: String,
    pub decision_date: Option<String>,
    pub evidence: Vec<String>,
    pub created_by_run_id: RunId,
    pub updated_by_run_id: RunId,
    pub supersedes: Vec<EntryId>,
    pub body_markdown: String,
}

/// Stores one API-shaped wiki digest row for the read protocol.
#[derive(Debug, Clone, Serialize)]
pub struct WikiDigestListItem {
    pub digest_id: DigestId,
    pub run_id: RunId,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub extracted_decision_count: usize,
}

impl From<DigestSummary> for WikiDigestListItem {
    fn from(summary: DigestSummary) -> Self {
        Self {
            digest_id: summary.digest_id,
            run_id: summary.run_id,
            title: summary.title,
            created_at: summary.created_at,
            updated_at: summary.updated_at,
            extracted_decision_count: summary.extracted_decision_count,
        }
    }
}

impl WikiDigestDetailItem {
    /// Builds one digest-detail payload from one canonical digest document detail.
    fn from_document(document: darc_wiki::DigestDetailDocument) -> Self {
        Self {
            digest_id: document.frontmatter.digest_id,
            run_id: document.frontmatter.run_id,
            title: document.frontmatter.title,
            created_at: document.frontmatter.created_at,
            updated_at: document.frontmatter.updated_at,
            extracted_decision_count: document.frontmatter.extracted_decision_count,
            body_markdown: document.body_markdown,
        }
    }
}

/// Stores one API-shaped wiki digest detail payload for the read protocol.
#[derive(Debug, Clone, Serialize)]
pub struct WikiDigestDetailItem {
    pub digest_id: DigestId,
    pub run_id: RunId,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub extracted_decision_count: usize,
    pub body_markdown: String,
}

/// Stores one API-shaped wiki run row for the read protocol.
#[derive(Debug, Clone, Serialize)]
pub struct WikiRunListItem {
    pub run_id: RunId,
    pub status: RunStatus,
    pub phase: RunPhase,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    pub headline: Option<String>,
}

impl From<RunSummary> for WikiRunListItem {
    fn from(summary: RunSummary) -> Self {
        Self {
            run_id: summary.run_id,
            status: summary.status,
            phase: summary.phase,
            created_at: summary.created_at,
            updated_at: summary.updated_at,
            finished_at: summary.finished_at,
            headline: summary.headline,
        }
    }
}

/// Stores one API-shaped wiki run detail payload for the read protocol.
#[derive(Debug, Clone, Serialize)]
pub struct WikiRunDetailItem {
    pub run_id: RunId,
    pub status: RunStatus,
    pub phase: RunPhase,
    pub created_at: String,
    pub started_at: Option<String>,
    pub updated_at: String,
    pub heartbeat_at: Option<String>,
    pub finished_at: Option<String>,
    pub requested_by: Option<String>,
    pub request_source: Option<String>,
    pub attempt: u32,
    pub cancel_requested: bool,
    pub pid: Option<u32>,
    pub agent_id: Option<String>,
    pub runtime: Option<String>,
    pub model: Option<String>,
    pub auth_profile: Option<String>,
    pub selected_sessions: Vec<String>,
    pub target_categories: Vec<String>,
    pub target_domains: Vec<String>,
    pub progress_percent: Option<u8>,
    pub headline: Option<String>,
    pub created_entry_ids: Vec<String>,
    pub updated_entry_ids: Vec<String>,
    pub digest_id: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub result: Option<WikiRunResultItem>,
}

impl WikiRunDetailItem {
    /// Builds one run-detail payload from one durable run state plus optional result artifact.
    fn from_state(state: RunState, result: Option<DigestResultArtifact>) -> Self {
        Self {
            run_id: state.run_id,
            status: state.status,
            phase: state.phase,
            created_at: state.created_at,
            started_at: state.started_at,
            updated_at: state.updated_at,
            heartbeat_at: state.heartbeat_at,
            finished_at: state.finished_at,
            requested_by: state.requested_by,
            request_source: state.request_source,
            attempt: state.attempt,
            cancel_requested: state.cancel_requested,
            pid: state.pid,
            agent_id: state.agent_id,
            runtime: state.runtime,
            model: state.model,
            auth_profile: state.auth_profile,
            selected_sessions: state.selected_sessions,
            target_categories: state.target_categories,
            target_domains: state.target_domains,
            progress_percent: state.progress_percent,
            headline: state.headline,
            created_entry_ids: state.created_entry_ids,
            updated_entry_ids: state.updated_entry_ids,
            digest_id: state.digest_id,
            error_code: state.error_code,
            error_message: state.error_message,
            result: result.map(WikiRunResultItem::from),
        }
    }
}

/// Stores the parsed terminal result detail for one wiki run.
#[derive(Debug, Clone, Serialize)]
pub struct WikiRunResultItem {
    pub status: RunStatus,
    pub completed_at: String,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub runtime: WikiRunRuntimeResultItem,
    pub validation: WikiRunValidationResultItem,
    pub note: Option<String>,
}

impl From<DigestResultArtifact> for WikiRunResultItem {
    fn from(result: DigestResultArtifact) -> Self {
        Self {
            status: result.status,
            completed_at: result.completed_at,
            error_code: result.error_code,
            error_message: result.error_message,
            runtime: WikiRunRuntimeResultItem::from(result.runtime),
            validation: WikiRunValidationResultItem::from(result.validation),
            note: result.note,
        }
    }
}

/// Stores the runtime section parsed from one wiki run result artifact.
#[derive(Debug, Clone, Serialize)]
pub struct WikiRunRuntimeResultItem {
    pub agent_id: Option<String>,
    pub runtime: Option<String>,
    pub model: Option<String>,
    pub auth_profile: Option<String>,
    pub display_name: Option<String>,
    pub exit_code: Option<i32>,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub proposal_source: Option<String>,
    pub proposal_captured: bool,
}

impl From<DigestRuntimeArtifact> for WikiRunRuntimeResultItem {
    fn from(runtime: DigestRuntimeArtifact) -> Self {
        Self {
            agent_id: runtime.agent_id,
            runtime: runtime.runtime,
            model: runtime.model,
            auth_profile: runtime.auth_profile,
            display_name: runtime.display_name,
            exit_code: runtime.exit_code,
            stdout_bytes: runtime.stdout_bytes,
            stderr_bytes: runtime.stderr_bytes,
            proposal_source: runtime.proposal_source,
            proposal_captured: runtime.proposal_captured,
        }
    }
}

/// Stores the validation section parsed from one wiki run result artifact.
#[derive(Debug, Clone, Serialize)]
pub struct WikiRunValidationResultItem {
    pub attempted: bool,
    pub valid: bool,
    pub entry_count: Option<usize>,
    pub run_summary_title: Option<String>,
    pub extracted_decision_count: Option<usize>,
    pub errors: Vec<darc_wiki::ProposalValidationError>,
}

impl From<DigestValidationArtifact> for WikiRunValidationResultItem {
    fn from(validation: DigestValidationArtifact) -> Self {
        Self {
            attempted: validation.attempted,
            valid: validation.valid,
            entry_count: validation.entry_count,
            run_summary_title: validation.run_summary_title,
            extracted_decision_count: validation.extracted_decision_count,
            errors: validation.errors,
        }
    }
}

/// Queries the wiki registry payload for one configured project.
pub fn query_wiki_registry(
    root: Option<PathBuf>,
    project_id: &str,
) -> Result<WikiRegistryQueryData> {
    let context = load_project_config_context(root, project_id)?;
    let layout = load_project_wiki_layout(&context)?;
    let registry = load_registry(&layout)?;
    Ok(WikiRegistryQueryData {
        project_id: context.project.id,
        schema_version: registry.schema_version,
        categories: registry.categories,
        domains: registry.domains,
    })
}

/// Queries the wiki entry-list payload for one configured project.
pub fn query_wiki_entries(
    root: Option<PathBuf>,
    project_id: &str,
    options: &WikiEntriesQueryOptions,
) -> Result<WikiEntriesQueryData> {
    let context = load_project_config_context(root, project_id)?;
    let layout = load_project_wiki_layout(&context)?;
    let entries = list_entries(&layout)?
        .into_iter()
        .filter(|entry| {
            options
                .category
                .as_ref()
                .is_none_or(|category| entry.category == *category)
        })
        .filter(|entry| {
            options.domain.as_ref().is_none_or(|domain| {
                entry
                    .domains
                    .iter()
                    .any(|entry_domain| entry_domain == domain)
            })
        })
        .filter(|entry| options.status.is_none_or(|status| entry.status == status))
        .map(WikiEntryListItem::from)
        .collect();
    Ok(WikiEntriesQueryData {
        project_id: context.project.id,
        entries,
    })
}

/// Queries the wiki entry-detail payload for one configured project and entry id.
pub fn query_wiki_entry(
    root: Option<PathBuf>,
    project_id: &str,
    entry_id: &EntryId,
) -> Result<WikiEntryQueryData> {
    let context = load_project_config_context(root, project_id)?;
    let layout = load_project_wiki_layout(&context)?;
    let entry = list_entries(&layout)?
        .into_iter()
        .find(|entry| &entry.entry_id == entry_id)
        .with_context(|| format!("wiki entry `{entry_id}` was not found"))?;
    let document = load_entry_detail(&entry.path)?;
    Ok(WikiEntryQueryData {
        project_id: context.project.id,
        entry: WikiEntryDetailItem::from_document(document),
    })
}

/// Queries the wiki digest-list payload for one configured project.
pub fn query_wiki_digests(
    root: Option<PathBuf>,
    project_id: &str,
    options: &WikiDigestsQueryOptions,
) -> Result<WikiDigestsQueryData> {
    let context = load_project_config_context(root, project_id)?;
    let layout = load_project_wiki_layout(&context)?;
    let bounds = QueryTimeBounds::new(options.since.as_deref(), options.until.as_deref())?;
    let digests = apply_limit(
        sort_items_by_created_at_desc(filter_by_created_at_bounds(list_digests(&layout)?, bounds))
            .into_iter()
            .map(WikiDigestListItem::from)
            .collect(),
        options.limit,
    );
    Ok(WikiDigestsQueryData {
        project_id: context.project.id,
        since: options.since.clone(),
        until: options.until.clone(),
        digests,
    })
}

/// Queries the wiki digest-detail payload for one configured project and digest id.
pub fn query_wiki_digest(
    root: Option<PathBuf>,
    project_id: &str,
    digest_id: &DigestId,
) -> Result<WikiDigestQueryData> {
    let context = load_project_config_context(root, project_id)?;
    let layout = load_project_wiki_layout(&context)?;
    let digest = list_digests(&layout)?
        .into_iter()
        .find(|digest| &digest.digest_id == digest_id)
        .with_context(|| format!("wiki digest `{digest_id}` was not found"))?;
    let document = load_digest_detail(&digest.path)?;
    Ok(WikiDigestQueryData {
        project_id: context.project.id,
        digest: WikiDigestDetailItem::from_document(document),
    })
}

/// Queries the wiki run-list payload for one configured project.
pub fn query_wiki_runs(
    root: Option<PathBuf>,
    project_id: &str,
    options: &WikiRunsQueryOptions,
) -> Result<WikiRunsQueryData> {
    let context = load_project_config_context(root, project_id)?;
    let layout = load_project_wiki_layout(&context)?;
    let bounds = QueryTimeBounds::new(options.since.as_deref(), options.until.as_deref())?;
    let runs = apply_limit(
        sort_items_by_created_at_desc(filter_by_created_at_bounds(
            load_visible_run_summaries(&layout)?,
            bounds,
        ))
        .into_iter()
        .filter(|run| options.status.is_none_or(|status| run.status == status))
        .map(WikiRunListItem::from)
        .collect(),
        options.limit,
    );
    Ok(WikiRunsQueryData {
        project_id: context.project.id,
        since: options.since.clone(),
        until: options.until.clone(),
        runs,
    })
}

/// Queries the wiki run-detail payload for one configured project and run id.
pub fn query_wiki_run(
    root: Option<PathBuf>,
    project_id: &str,
    run_id: &RunId,
) -> Result<WikiRunQueryData> {
    let context = load_project_config_context(root, project_id)?;
    let layout = load_project_wiki_layout(&context)?;
    let run = load_project_wiki_run_from_layout(&layout, run_id)?;
    let result = load_run_result_artifact(&layout.run_result_path(run_id))?;
    Ok(WikiRunQueryData {
        project_id: context.project.id,
        run: WikiRunDetailItem::from_state(run, result),
    })
}

/// Applies one optional list limit while preserving deterministic ordering.
fn apply_limit<T>(items: Vec<T>, limit: Option<usize>) -> Vec<T> {
    match limit {
        Some(limit) => items.into_iter().take(limit).collect(),
        None => items,
    }
}

/// Stores one parsed inclusive/exclusive timestamp filter pair for in-memory wiki queries.
#[derive(Debug, Clone)]
struct QueryTimeBounds {
    since: Option<String>,
    until: Option<String>,
}

impl QueryTimeBounds {
    /// Parses one optional `--since` and `--until` pair into comparable UTC timestamps.
    fn new(since: Option<&str>, until: Option<&str>) -> Result<Self> {
        Ok(Self {
            since: parse_optional_query_time_bound("since", since)?,
            until: parse_optional_query_time_bound("until", until)?,
        })
    }

    /// Returns whether any timestamp filters are active for the current query.
    fn has_filters(&self) -> bool {
        self.since.is_some() || self.until.is_some()
    }

    /// Returns whether one created-at timestamp satisfies the configured bounds.
    fn matches(&self, created_at: &str) -> bool {
        if !self.has_filters() {
            return true;
        }
        if parse_utc_timestamp(created_at).is_none() {
            return false;
        }
        self.since
            .as_deref()
            .is_none_or(|since| created_at >= since)
            && self.until.as_deref().is_none_or(|until| created_at < until)
    }
}

/// Parses one optional query time bound using the shared CLI/read-side semantics.
fn parse_optional_query_time_bound(label: &str, value: Option<&str>) -> Result<Option<String>> {
    value
        .map(|value| {
            resolve_shared_query_time_bound(value)
                .map_err(|error| anyhow::anyhow!(error))
                .with_context(|| format!("invalid {label} query time bound `{value}`"))
        })
        .transpose()
}

/// Filters one in-memory list by `created_at` using the shared inclusive/exclusive semantics.
fn filter_by_created_at_bounds<T>(items: Vec<T>, bounds: QueryTimeBounds) -> Vec<T>
where
    T: CreatedAtTimestamp,
{
    items
        .into_iter()
        .filter(|item| bounds.matches(item.created_at()))
        .collect()
}

/// Sorts one wiki list recency-first by `created_at`, pushing malformed timestamps last.
fn sort_items_by_created_at_desc<T>(mut items: Vec<T>) -> Vec<T>
where
    T: CreatedAtTimestamp,
{
    items.sort_by(|left, right| {
        parse_utc_timestamp(right.created_at())
            .cmp(&parse_utc_timestamp(left.created_at()))
            .then_with(|| right.created_at().cmp(left.created_at()))
            .then_with(|| left.stable_id().cmp(right.stable_id()))
    });
    items
}

/// Exposes one created-at timestamp for shared in-memory wiki list filtering.
trait CreatedAtTimestamp {
    /// Returns the canonical UTC `created_at` timestamp for the current row.
    fn created_at(&self) -> &str;

    /// Returns one deterministic stable id used to break ordering ties.
    fn stable_id(&self) -> &str;
}

impl CreatedAtTimestamp for DigestSummary {
    fn created_at(&self) -> &str {
        &self.created_at
    }

    fn stable_id(&self) -> &str {
        self.digest_id.as_str()
    }
}

impl CreatedAtTimestamp for RunSummary {
    fn created_at(&self) -> &str {
        &self.created_at
    }

    fn stable_id(&self) -> &str {
        self.run_id.as_str()
    }
}

/// Inspects one darc root without requiring the workspace to be initialized.
pub fn inspect_root(root: Option<PathBuf>) -> RootInfo {
    let default_root_path = default_root_path();
    let requested_root_path = root.unwrap_or_else(|| default_root_path.clone());
    let resolved_root_path = resolve_root_path(&requested_root_path);
    let config_path = resolved_root_path.join(CONFIG_FILE_NAME);
    let database_path = resolved_root_path.join(INDEX_DB_FILE_NAME);
    let root_exists = resolved_root_path.exists();
    let config_exists = config_path.exists();
    let database_exists = database_path.exists();
    let mut issues = Vec::new();

    if !root_exists {
        issues.push(format!(
            "Darc root was not found at {}.",
            resolved_root_path.display()
        ));
    }
    if root_exists && !config_exists {
        issues.push(format!(
            "Darc config.toml was not found at {}.",
            config_path.display()
        ));
    }
    if root_exists && !database_exists {
        issues.push(format!(
            "Darc index.sqlite was not found at {}.",
            database_path.display()
        ));
    }

    RootInfo {
        default_root_path,
        requested_root_path,
        resolved_root_path,
        config_path,
        database_path,
        available: RootAvailability {
            root_exists,
            config_exists,
            database_exists,
        },
        issues,
    }
}

/// Loads one configured project context for project-scoped query commands.
fn load_project_query_context(
    root: Option<PathBuf>,
    project_id: &str,
) -> Result<ProjectQueryContext> {
    let context = load_project_config_context(root, project_id)?;
    ensure_database_exists(&context.root)?;
    Ok(context)
}

/// Loads one configured project context for project-scoped config-only queries.
fn load_project_config_context(
    root: Option<PathBuf>,
    project_id: &str,
) -> Result<ProjectQueryContext> {
    let root = inspect_root(root);
    ensure_config_exists(&root)?;

    let config = load_normalized_config(&root.config_path)?;
    let project = config
        .projects
        .into_iter()
        .find(|project| project.id == project_id)
        .with_context(|| {
            format!(
                "project `{project_id}` was not found in {}",
                root.config_path.display()
            )
        })?;

    Ok(ProjectQueryContext { root, project })
}

/// Loads and normalizes one shared config file for query responses.
fn load_normalized_config(path: &Path) -> Result<SharedConfig> {
    let mut config = load_config(path)?;
    config.projects = config
        .projects
        .into_iter()
        .map(normalize_project_config)
        .collect::<Result<Vec<_>>>()?;
    Ok(config)
}

/// Returns the best-effort resolved filesystem path for one darc root input.
fn resolve_root_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    }

    let joined = env::current_dir()
        .map(|current_dir| current_dir.join(path))
        .unwrap_or_else(|_| path.to_path_buf());
    fs::canonicalize(&joined).unwrap_or(joined)
}

/// Returns one project aggregate lookup keyed by project id.
fn aggregate_map(
    aggregates: Vec<ProjectIndexAggregate>,
) -> std::collections::BTreeMap<String, ProjectIndexAggregate> {
    aggregates
        .into_iter()
        .map(|aggregate| (aggregate.project_id.clone(), aggregate))
        .collect()
}

/// Returns an error when the shared config file is unavailable.
fn ensure_config_exists(root: &RootInfo) -> Result<()> {
    if root.available.config_exists {
        return Ok(());
    }
    bail!(
        "{}",
        root.issues.first().cloned().unwrap_or_else(|| format!(
            "Darc config.toml was not found at {}.",
            root.config_path.display()
        ))
    )
}

/// Returns an error when the shared index database is unavailable.
fn ensure_database_exists(root: &RootInfo) -> Result<()> {
    if root.available.database_exists {
        return Ok(());
    }
    bail!(
        "{}",
        root.issues
            .iter()
            .find(|issue| issue.contains("index.sqlite"))
            .cloned()
            .unwrap_or_else(|| format!(
                "Darc index.sqlite was not found at {}.",
                root.database_path.display()
            ))
    )
}

/// Resolves one project-scoped wiki layout under the configured Darc root.
fn load_project_wiki_layout(context: &ProjectQueryContext) -> Result<darc_wiki::ProjectLayout> {
    ContextWikiLayout::new(context.root.resolved_root_path.clone())
        .project_layout(context.project.id.clone())
        .context("failed to resolve project wiki layout")
}

/// Loads one parsed run result artifact when the durable result file exists.
fn load_run_result_artifact(path: &Path) -> Result<Option<DigestResultArtifact>> {
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let artifact = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(artifact))
}

/// Stores the validated root and project context for one project-scoped query.
struct ProjectQueryContext {
    root: RootInfo,
    project: ProjectConfig,
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use anyhow::Result;
    use darc_test_utils::unique_test_dir;

    use super::{
        WikiDigestsQueryOptions, WikiEntriesQueryOptions, WikiRunsQueryOptions, query_wiki_digest,
        query_wiki_digests, query_wiki_entries, query_wiki_entry, query_wiki_runs,
    };
    use crate::{
        DigestId, EntryId, EntryStatus, RunId, RunPhase, RunState, RunStatus,
        config::{ProjectConfig, SharedConfig, SourcesConfig},
        constants::CONFIG_FILE_NAME,
        wiki::{ensure_project_wiki, store_project_wiki_run},
    };

    /// Writes one minimal shared config fixture for one query test root.
    fn write_config(root: &Path, project_id: &str) -> Result<()> {
        let project_root = root.join("repo");
        fs::create_dir_all(&project_root)?;
        fs::write(
            root.join(CONFIG_FILE_NAME),
            toml::to_string_pretty(&SharedConfig::new(
                root.to_path_buf(),
                vec![ProjectConfig {
                    id: project_id.to_owned(),
                    name: "repo".to_owned(),
                    local_path: project_root,
                    git_upstream: None,
                    sessions_root: root.join(format!("projects/{project_id}/sessions")),
                    known_paths: Vec::new(),
                }],
                SourcesConfig::default(),
            ))?,
        )?;
        Ok(())
    }

    /// Writes one canonical entry fixture under the provided project layout.
    fn write_entry(
        layout: &darc_wiki::ProjectLayout,
        entry_id: &EntryId,
        category: &str,
        domains: &[&str],
        status: &str,
        body_markdown: &str,
    ) -> Result<()> {
        let path = layout.entry_path(category, entry_id);
        let domains = format!(
            "[{}]",
            domains
                .iter()
                .map(|domain| format!("\"{domain}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        fs::create_dir_all(path.parent().expect("entry path should have a parent"))?;
        fs::write(
            path,
            format!(
                concat!(
                    "+++\n",
                    "schema_version = 1\n",
                    "entry_id = \"{entry_id}\"\n",
                    "entry_type = \"decision_trace\"\n",
                    "display_id = \"DT-1\"\n",
                    "project_id = \"{project_id}\"\n",
                    "title = \"{title}\"\n",
                    "category = \"{category}\"\n",
                    "domains = {domains}\n",
                    "status = \"{status}\"\n",
                    "created_at = \"2026-04-13T10:31:22Z\"\n",
                    "updated_at = \"2026-04-13T10:31:22Z\"\n",
                    "decision_date = \"2026-04-13\"\n",
                    "evidence = [\"codex:session-1#2\"]\n",
                    "created_by_run_id = \"cwrun_01write\"\n",
                    "updated_by_run_id = \"cwrun_01write\"\n",
                    "supersedes = []\n",
                    "+++\n\n",
                    "{body_markdown}\n"
                ),
                entry_id = entry_id,
                project_id = layout.project_id,
                title = format!("Entry {entry_id}"),
                category = category,
                domains = domains,
                status = status,
                body_markdown = body_markdown,
            ),
        )?;
        Ok(())
    }

    /// Writes one canonical digest fixture under the provided project layout.
    fn write_digest(
        layout: &darc_wiki::ProjectLayout,
        digest_id: &DigestId,
        run_id: &RunId,
        title: &str,
        created_at: &str,
        body_markdown: &str,
    ) -> Result<()> {
        fs::write(
            layout.digest_path(digest_id),
            format!(
                concat!(
                    "+++\n",
                    "schema_version = 1\n",
                    "digest_id = \"{digest_id}\"\n",
                    "project_id = \"{project_id}\"\n",
                    "run_id = \"{run_id}\"\n",
                    "title = \"{title}\"\n",
                    "created_at = \"{created_at}\"\n",
                    "updated_at = \"{created_at}\"\n",
                    "extracted_decision_count = 1\n",
                    "+++\n\n",
                    "{body_markdown}\n"
                ),
                digest_id = digest_id,
                project_id = layout.project_id,
                run_id = run_id,
                title = title,
                created_at = created_at,
                body_markdown = body_markdown,
            ),
        )?;
        Ok(())
    }

    /// Builds one minimal persisted run-state fixture for wiki run-list queries.
    fn build_run_state(
        project_id: &str,
        run_id: &RunId,
        status: RunStatus,
        created_at: &str,
    ) -> RunState {
        RunState {
            schema_version: 1,
            run_id: run_id.clone(),
            project_id: project_id.to_owned(),
            status,
            phase: RunPhase::WritingArtifacts,
            created_at: created_at.to_owned(),
            started_at: Some(created_at.to_owned()),
            updated_at: created_at.to_owned(),
            finished_at: Some(created_at.to_owned()),
            heartbeat_at: Some(created_at.to_owned()),
            requested_by: Some("desktop".to_owned()),
            request_source: Some("darc-desktop/0.1.0".to_owned()),
            attempt: 1,
            cancel_requested: false,
            pid: None,
            agent_id: Some("codex".to_owned()),
            runtime: Some("external_cli".to_owned()),
            model: Some("gpt-5.4".to_owned()),
            auth_profile: None,
            selected_sessions: vec!["codex:session-1".to_owned()],
            target_categories: vec!["product".to_owned()],
            target_domains: vec!["query".to_owned()],
            progress_percent: Some(100),
            headline: Some(format!("Run {run_id}")),
            proposal_path: Some("proposal.json".to_owned()),
            result_path: Some("result.json".to_owned()),
            events_path: Some("events.jsonl".to_owned()),
            stdout_log_path: Some("agent.stdout.log".to_owned()),
            stderr_log_path: Some("agent.stderr.log".to_owned()),
            created_entry_ids: Vec::new(),
            updated_entry_ids: Vec::new(),
            digest_id: None,
            error_code: None,
            error_message: None,
        }
    }

    #[test]
    fn wiki_entry_queries_support_filters_and_detail_payloads() -> Result<()> {
        let root = unique_test_dir("query-wiki-entry");
        let project_id = "repo-123";
        write_config(&root, project_id)?;
        let layout = ensure_project_wiki(Some(root.clone()), project_id)?;
        let active_entry_id = EntryId::new("cw_01active")?;
        let discarded_entry_id = EntryId::new("cw_01discarded")?;
        write_entry(
            &layout,
            &active_entry_id,
            "product",
            &["query", "desktop"],
            "active",
            "## Final Decision\n\nKeep the backend contract stable.",
        )?;
        write_entry(
            &layout,
            &discarded_entry_id,
            "architecture",
            &["storage"],
            "discarded",
            "## Final Decision\n\nDiscard the old idea.",
        )?;

        let entries = query_wiki_entries(
            Some(root.clone()),
            project_id,
            &WikiEntriesQueryOptions {
                category: Some("product".to_owned()),
                domain: Some("query".to_owned()),
                status: Some(EntryStatus::Active),
            },
        )?;
        assert_eq!(entries.entries.len(), 1);
        assert_eq!(entries.entries[0].entry_id, active_entry_id);

        let detail = query_wiki_entry(Some(root.clone()), project_id, &active_entry_id)?;
        assert_eq!(detail.entry.entry_id, active_entry_id);
        assert_eq!(detail.entry.evidence, vec!["codex:session-1#2".to_owned()]);
        assert!(detail.entry.body_markdown.contains("## Final Decision"));

        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn wiki_digest_and_run_queries_support_limits_and_detail_payloads() -> Result<()> {
        let root = unique_test_dir("query-wiki-digest");
        let project_id = "repo-123";
        write_config(&root, project_id)?;
        let layout = ensure_project_wiki(Some(root.clone()), project_id)?;
        let first_run_id = RunId::new("cwrun_01digesta")?;
        let second_run_id = RunId::new("cwrun_01digestb")?;
        let first_digest_id = DigestId::new("dg_01digesta")?;
        let second_digest_id = DigestId::new("dg_01digestb")?;
        write_digest(
            &layout,
            &first_digest_id,
            &first_run_id,
            "First digest",
            "2026-04-13T11:00:00Z",
            "## Summary\n\nFirst body.",
        )?;
        write_digest(
            &layout,
            &second_digest_id,
            &second_run_id,
            "Second digest",
            "2026-04-14T11:00:00Z",
            "## Summary\n\nSecond body.",
        )?;
        store_project_wiki_run(
            Some(root.clone()),
            project_id,
            &build_run_state(
                project_id,
                &first_run_id,
                RunStatus::Running,
                "2026-04-13T12:00:00Z",
            ),
        )?;
        store_project_wiki_run(
            Some(root.clone()),
            project_id,
            &build_run_state(
                project_id,
                &second_run_id,
                RunStatus::Succeeded,
                "2026-04-14T12:00:00Z",
            ),
        )?;

        let digests = query_wiki_digests(
            Some(root.clone()),
            project_id,
            &WikiDigestsQueryOptions {
                limit: Some(1),
                since: Some("2026-04-13T00:00:00Z".to_owned()),
                until: Some("2026-04-14T00:00:00Z".to_owned()),
            },
        )?;
        assert_eq!(digests.digests.len(), 1);
        assert_eq!(digests.digests[0].digest_id, first_digest_id);
        assert_eq!(digests.since.as_deref(), Some("2026-04-13T00:00:00Z"));
        assert_eq!(digests.until.as_deref(), Some("2026-04-14T00:00:00Z"));

        let digest = query_wiki_digest(Some(root.clone()), project_id, &second_digest_id)?;
        assert_eq!(digest.digest.digest_id, second_digest_id);
        assert!(digest.digest.body_markdown.contains("Second body."));

        let runs = query_wiki_runs(
            Some(root.clone()),
            project_id,
            &WikiRunsQueryOptions {
                status: Some(RunStatus::Succeeded),
                limit: Some(1),
                since: Some("2026-04-14T00:00:00Z".to_owned()),
                until: Some("2026-04-15T00:00:00Z".to_owned()),
            },
        )?;
        assert_eq!(runs.runs.len(), 1);
        assert_eq!(runs.runs[0].run_id, second_run_id);
        assert_eq!(runs.runs[0].status, RunStatus::Succeeded);
        assert_eq!(runs.since.as_deref(), Some("2026-04-14T00:00:00Z"));
        assert_eq!(runs.until.as_deref(), Some("2026-04-15T00:00:00Z"));

        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn wiki_digest_and_run_queries_sort_recency_first_and_tolerate_malformed_created_at()
    -> Result<()> {
        let root = unique_test_dir("query-wiki-recency");
        let project_id = "repo-123";
        write_config(&root, project_id)?;
        let layout = ensure_project_wiki(Some(root.clone()), project_id)?;
        let early_run_id = RunId::new("cwrun_01early")?;
        let late_run_id = RunId::new("cwrun_01late")?;
        let malformed_run_id = RunId::new("cwrun_01broken")?;
        let early_digest_id = DigestId::new("dg_01early")?;
        let late_digest_id = DigestId::new("dg_01late")?;
        let malformed_digest_id = DigestId::new("dg_01broken")?;
        write_digest(
            &layout,
            &early_digest_id,
            &early_run_id,
            "Early digest",
            "2026-04-13T11:00:00Z",
            "early",
        )?;
        write_digest(
            &layout,
            &late_digest_id,
            &late_run_id,
            "Late digest",
            "2026-04-15T11:00:00Z",
            "late",
        )?;
        write_digest(
            &layout,
            &malformed_digest_id,
            &malformed_run_id,
            "Broken digest",
            "not-a-timestamp",
            "broken",
        )?;
        store_project_wiki_run(
            Some(root.clone()),
            project_id,
            &build_run_state(
                project_id,
                &early_run_id,
                RunStatus::Succeeded,
                "2026-04-13T12:00:00Z",
            ),
        )?;
        store_project_wiki_run(
            Some(root.clone()),
            project_id,
            &build_run_state(
                project_id,
                &late_run_id,
                RunStatus::Succeeded,
                "2026-04-15T12:00:00Z",
            ),
        )?;
        store_project_wiki_run(
            Some(root.clone()),
            project_id,
            &build_run_state(
                project_id,
                &malformed_run_id,
                RunStatus::Succeeded,
                "not-a-timestamp",
            ),
        )?;

        let digests = query_wiki_digests(
            Some(root.clone()),
            project_id,
            &WikiDigestsQueryOptions {
                limit: Some(1),
                since: None,
                until: None,
            },
        )?;
        let runs = query_wiki_runs(
            Some(root.clone()),
            project_id,
            &WikiRunsQueryOptions {
                status: Some(RunStatus::Succeeded),
                limit: Some(1),
                since: None,
                until: None,
            },
        )?;
        assert_eq!(digests.digests[0].digest_id, late_digest_id);
        assert_eq!(runs.runs[0].run_id, late_run_id);

        let filtered_digests = query_wiki_digests(
            Some(root.clone()),
            project_id,
            &WikiDigestsQueryOptions {
                limit: None,
                since: Some("2026-04-14T00:00:00Z".to_owned()),
                until: Some("2026-04-16T00:00:00Z".to_owned()),
            },
        )?;
        let filtered_runs = query_wiki_runs(
            Some(root.clone()),
            project_id,
            &WikiRunsQueryOptions {
                status: Some(RunStatus::Succeeded),
                limit: None,
                since: Some("2026-04-14T00:00:00Z".to_owned()),
                until: Some("2026-04-16T00:00:00Z".to_owned()),
            },
        )?;
        assert_eq!(
            filtered_digests
                .digests
                .iter()
                .map(|digest| digest.digest_id.as_str())
                .collect::<Vec<_>>(),
            vec![late_digest_id.as_str()]
        );
        assert_eq!(
            filtered_runs
                .runs
                .iter()
                .map(|run| run.run_id.as_str())
                .collect::<Vec<_>>(),
            vec![late_run_id.as_str()]
        );

        fs::remove_dir_all(root)?;
        Ok(())
    }
}
