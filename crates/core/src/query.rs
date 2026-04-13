use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use darc_index::INDEX_DB_FILE_NAME;
use darc_paths::SourceKind;
pub use darc_query::{
    DailyTimeStat, FileUsageStat, HardDebuggingTurn, ProjectInsights, ProjectSummary,
    ProjectTimeStat, RootAvailability, RootInfo, SearchMode, SearchTurnHit, SearchTurnsQueryData,
    SearchTurnsRequest, SessionKind, SessionRuntimeStat, SessionSummary, SessionsQueryData,
    ShellCommandSummary, ToolUsageStat, TurnDetail, TurnDetailInsights, TurnDetailOptions,
    TurnInsights, TurnSummary, TurnsQueryData, WorkspaceDailyTimeStat, WorkspaceInsights,
    WorkspaceQueryData,
};
use darc_query::{
    ProjectIndexAggregate, list_project_index_aggregates, query_project_insights,
    query_project_sessions, query_search_turns as query_project_search_turns, query_session_turns,
    query_turn_detail, query_turn_insights, query_workspace_insights,
};
use darc_wiki::{
    ContextWikiLayout, DigestId, DigestSummary, EntryId, EntryStatus, EntrySummary, EntryType,
    RunId, RunPhase, RunStatus, RunSummary, list_digests, list_entries, list_runs, load_registry,
};
use serde::Serialize;

use crate::{
    config::{ProjectConfig, SharedConfig, load_config},
    constants::CONFIG_FILE_NAME,
    default_root_path,
    init::normalize_project_config,
    wiki::visible_run_summary,
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
) -> Result<SessionsQueryData> {
    let context = load_project_query_context(root, project_id)?;
    query_project_sessions(
        &context.root.database_path,
        &context.project.id,
        since,
        until,
    )
}

/// Queries the turn-list payload for one configured provider session.
pub fn query_turns(
    root: Option<PathBuf>,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
) -> Result<TurnsQueryData> {
    let context = load_project_query_context(root, project_id)?;
    query_session_turns(
        &context.root.database_path,
        &context.project.id,
        provider,
        session_id,
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

/// Stores the entry-list payload for one project-scoped wiki query.
#[derive(Debug, Clone, Serialize)]
pub struct WikiEntriesQueryData {
    pub project_id: String,
    pub entries: Vec<WikiEntryListItem>,
}

/// Stores the digest-list payload for one project-scoped wiki query.
#[derive(Debug, Clone, Serialize)]
pub struct WikiDigestsQueryData {
    pub project_id: String,
    pub digests: Vec<WikiDigestListItem>,
}

/// Stores the run-list payload for one project-scoped wiki query.
#[derive(Debug, Clone, Serialize)]
pub struct WikiRunsQueryData {
    pub project_id: String,
    pub runs: Vec<WikiRunListItem>,
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
pub fn query_wiki_entries(root: Option<PathBuf>, project_id: &str) -> Result<WikiEntriesQueryData> {
    let context = load_project_config_context(root, project_id)?;
    let layout = load_project_wiki_layout(&context)?;
    Ok(WikiEntriesQueryData {
        project_id: context.project.id,
        entries: list_entries(&layout)?
            .into_iter()
            .map(WikiEntryListItem::from)
            .collect(),
    })
}

/// Queries the wiki digest-list payload for one configured project.
pub fn query_wiki_digests(root: Option<PathBuf>, project_id: &str) -> Result<WikiDigestsQueryData> {
    let context = load_project_config_context(root, project_id)?;
    let layout = load_project_wiki_layout(&context)?;
    Ok(WikiDigestsQueryData {
        project_id: context.project.id,
        digests: list_digests(&layout)?
            .into_iter()
            .map(WikiDigestListItem::from)
            .collect(),
    })
}

/// Queries the wiki run-list payload for one configured project.
pub fn query_wiki_runs(root: Option<PathBuf>, project_id: &str) -> Result<WikiRunsQueryData> {
    let context = load_project_config_context(root, project_id)?;
    let layout = load_project_wiki_layout(&context)?;
    Ok(WikiRunsQueryData {
        project_id: context.project.id,
        runs: list_runs(&layout)?
            .into_iter()
            .map(|summary| visible_run_summary(&summary))
            .map(WikiRunListItem::from)
            .collect(),
    })
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

/// Stores the validated root and project context for one project-scoped query.
struct ProjectQueryContext {
    root: RootInfo,
    project: ProjectConfig,
}
