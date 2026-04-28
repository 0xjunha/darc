use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use darc_index::INDEX_DB_FILE_NAME;
use darc_paths::SourceKind;
pub use darc_query::{
    DEFAULT_RESOLVE_SESSION_MATCH_LIMIT, DailyTimeStat, FilePivotSummary, FileSessionSummary,
    FileUsageStat, FilesQueryData, FilesQueryMode, FilesQueryRequest, HardDebuggingTurn,
    ProjectInsights, ProjectSummary, ProjectTimeStat, ResolveSessionQueryData,
    ResolveSessionQueryRequest, ResolvedSessionMatch, RootAvailability, RootInfo,
    SearchEvidenceField, SearchMode, SearchTurnHit, SearchTurnMatch, SearchTurnsQueryData,
    SearchTurnsRequest, SessionBundleQueryData, SessionBundleQueryRequest, SessionBundleView,
    SessionFileSummary, SessionFilesQueryData, SessionKind, SessionRuntimeStat, SessionSummary,
    SessionsQueryData, SessionsQueryRequest, ShellCommandSummary, ToolUsageStat, TurnDetail,
    TurnDetailInsights, TurnDetailOptions, TurnInsights, TurnSummary, TurnsQueryData,
    TurnsQueryRequest, TurnsView, WorkspaceDailyTimeStat, WorkspaceInsights, WorkspaceQueryData,
};
use darc_query::{
    ProjectIndexAggregate, list_project_index_aggregates, lookup_project_session_matches,
    query_project_files, query_project_insights, query_project_session_bundle,
    query_project_session_files, query_project_sessions, query_project_turns as query_index_turns,
    query_resolve_sessions as query_index_resolve_sessions,
    query_search_turns as query_project_search_turns,
    query_session_turn_details as query_project_session_turn_details, query_turn_detail,
    query_turn_insights, query_workspace_insights,
};
use serde_json::{Value as JsonValue, json};
use thiserror::Error;

use crate::{
    active_project::load_active_project,
    config::{ProjectConfig, SharedConfig, load_config},
    constants::CONFIG_FILE_NAME,
    default_root_path,
    init::normalize_project_config,
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

/// Stores one resolved project-scoped query target plus its root metadata.
#[derive(Debug, Clone)]
pub struct ResolvedQueryProject {
    context: ProjectQueryContext,
}

/// Stores one resolved project-scoped provider/session identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedQuerySession {
    pub provider: SourceKind,
    pub session_id: String,
}

/// Resolves one database-backed project-scoped query target from an explicit id or the current directory.
pub fn resolve_query_project(
    root: Option<PathBuf>,
    project_id: Option<&str>,
) -> Result<ResolvedQueryProject> {
    resolve_query_project_with_scope(root, project_id, QueryProjectScope::Database)
}

/// Resolves one config-backed project-scoped query target from an explicit id or the current directory.
pub fn resolve_query_config_project(
    root: Option<PathBuf>,
    project_id: Option<&str>,
) -> Result<ResolvedQueryProject> {
    resolve_query_project_with_scope(root, project_id, QueryProjectScope::ConfigOnly)
}

/// Queries the session-list payload for one already-resolved configured project.
pub fn query_sessions_for_project(
    project: &ResolvedQueryProject,
    provider: Option<SourceKind>,
    since: Option<&str>,
    until: Option<&str>,
    touched_path: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<SessionsQueryData> {
    let context = &project.context;
    query_project_sessions(
        &context.root.database_path,
        SessionsQueryRequest {
            project_id: &context.project.id,
            project_root: Some(context.project.local_path.as_path()),
            provider,
            since,
            until,
            touched_path,
            limit,
            offset,
        },
    )
}

/// Queries the session-list payload for one configured project.
pub fn query_sessions(
    root: Option<PathBuf>,
    request: SessionsQueryRequest<'_>,
) -> Result<SessionsQueryData> {
    let project = resolve_query_project(root, Some(request.project_id))?;
    query_sessions_for_project(
        &project,
        request.provider,
        request.since,
        request.until,
        request.touched_path,
        request.limit,
        request.offset,
    )
}

/// Queries one file-pivot payload for one already-resolved configured project.
pub fn query_files_for_project(
    project: &ResolvedQueryProject,
    request: FilesQueryRequest<'_>,
) -> Result<FilesQueryData> {
    let context = &project.context;
    query_project_files(
        &context.root.database_path,
        FilesQueryRequest {
            project_id: &context.project.id,
            project_root: Some(context.project.local_path.as_path()),
            provider: request.provider,
            ..request
        },
    )
}

/// Queries one file-pivot payload for one configured project.
pub fn query_files(
    root: Option<PathBuf>,
    request: FilesQueryRequest<'_>,
) -> Result<FilesQueryData> {
    let project = resolve_query_project(root, Some(request.project_id))?;
    query_files_for_project(&project, request)
}

/// Resolves one full session id or UUID prefix across indexed projects and providers.
pub fn query_resolve_sessions(
    root: Option<PathBuf>,
    request: ResolveSessionQueryRequest<'_>,
) -> Result<ResolveSessionQueryData> {
    let root = inspect_root(root);
    ensure_database_exists(&root)?;
    let query = validate_resolve_session_query(request.query)?;
    query_index_resolve_sessions(
        &root.database_path,
        ResolveSessionQueryRequest { query, ..request },
    )
}

/// Resolves one strict `darc query` session id against one project and optional provider filter.
pub fn resolve_query_session_id(
    root: Option<PathBuf>,
    project_id: &str,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<String> {
    let project = resolve_query_project(root, Some(project_id))?;
    resolve_query_session_id_for_project(&project, provider, session_id)
}

/// Resolves one strict `darc query` session id against one already-resolved project.
pub fn resolve_query_session_id_for_project(
    project: &ResolvedQueryProject,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<String> {
    let context = &project.context;
    validate_project_session_id(
        &context.root.database_path,
        &context.project.id,
        provider,
        session_id,
    )
}

/// Resolves one strict search session-id filter without forcing cross-provider disambiguation.
pub fn resolve_query_search_session_id_for_project(
    project: &ResolvedQueryProject,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<String> {
    let context = &project.context;
    validate_project_session_filter_id(
        &context.root.database_path,
        &context.project.id,
        provider,
        session_id,
    )
}

/// Resolves one strict `darc query` session id plus provider against one project.
pub fn resolve_query_session_for_project(
    project: &ResolvedQueryProject,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<ResolvedQuerySession> {
    let context = &project.context;
    validate_project_session_ref(
        &context.root.database_path,
        &context.project.id,
        provider,
        session_id,
    )
}

/// Queries one session-scoped per-file access summary payload for one already-resolved configured project.
pub fn query_session_files_for_project(
    project: &ResolvedQueryProject,
    provider: SourceKind,
    session_id: &str,
) -> Result<SessionFilesQueryData> {
    let context = &project.context;
    query_project_session_files(
        &context.root.database_path,
        &context.project.id,
        provider,
        session_id,
        Some(context.project.local_path.as_path()),
    )
}

/// Queries one session-scoped per-file access summary payload.
pub fn query_session_files(
    root: Option<PathBuf>,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
) -> Result<SessionFilesQueryData> {
    let project = resolve_query_project(root, Some(project_id))?;
    query_session_files_for_project(&project, provider, session_id)
}

/// Queries one composite session bundle for one already-resolved configured provider session.
pub fn query_session_bundle_for_project(
    project: &ResolvedQueryProject,
    request: SessionBundleQueryRequest<'_>,
) -> Result<SessionBundleQueryData> {
    let context = &project.context;
    query_project_session_bundle(
        &context.root.database_path,
        SessionBundleQueryRequest {
            project_id: &context.project.id,
            project_root: Some(context.project.local_path.as_path()),
            ..request
        },
    )
}

/// Queries one composite session bundle for one configured provider session.
pub fn query_session_bundle(
    root: Option<PathBuf>,
    request: SessionBundleQueryRequest<'_>,
) -> Result<SessionBundleQueryData> {
    let project = resolve_query_project(root, Some(request.project_id))?;
    query_session_bundle_for_project(&project, request)
}

/// Queries the turn-list payload for one already-resolved configured provider session.
pub fn query_turns_for_project(
    project: &ResolvedQueryProject,
    request: TurnsQueryRequest<'_>,
) -> Result<TurnsQueryData> {
    let context = &project.context;
    query_index_turns(
        &context.root.database_path,
        TurnsQueryRequest {
            project_id: &context.project.id,
            ..request
        },
    )
}

/// Queries the turn-list payload for one configured provider session.
pub fn query_turns(
    root: Option<PathBuf>,
    request: TurnsQueryRequest<'_>,
) -> Result<TurnsQueryData> {
    let project = resolve_query_project(root, Some(request.project_id))?;
    query_turns_for_project(&project, request)
}

/// Queries one full turn-detail payload for one already-resolved configured provider session turn.
pub fn query_turn_for_project(
    project: &ResolvedQueryProject,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
    options: TurnDetailOptions,
) -> Result<TurnDetail> {
    let context = &project.context;
    query_turn_detail(
        &context.root.database_path,
        &context.project.id,
        provider,
        session_id,
        turn_ordinal,
        options,
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
    let project = resolve_query_project(root, Some(project_id))?;
    query_turn_for_project(&project, provider, session_id, turn_ordinal, options)
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

/// Queries the turn insights payload for one already-resolved configured provider session turn.
pub fn query_turn_insight_report_for_project(
    project: &ResolvedQueryProject,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
) -> Result<TurnInsights> {
    let context = &project.context;
    query_turn_insights(
        &context.root.database_path,
        &context.project.id,
        provider,
        session_id,
        turn_ordinal,
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
    let project = resolve_query_project(root, Some(project_id))?;
    query_turn_insight_report_for_project(&project, provider, session_id, turn_ordinal)
}

/// Queries one paginated turn-search payload for one already-resolved configured project.
pub fn query_search_turns_for_project(
    project: &ResolvedQueryProject,
    request: SearchTurnsRequest<'_>,
) -> Result<SearchTurnsQueryData> {
    let context = &project.context;
    query_project_search_turns(
        &context.root.database_path,
        SearchTurnsRequest {
            project_id: &context.project.id,
            project_root: Some(context.project.local_path.as_path()),
            ..request
        },
    )
}

/// Queries one paginated turn-search payload for one configured project.
pub fn query_search_turns(
    root: Option<PathBuf>,
    request: SearchTurnsRequest<'_>,
) -> Result<SearchTurnsQueryData> {
    let project = resolve_query_project(root, Some(request.project_id))?;
    query_search_turns_for_project(&project, request)
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

/// Queries the project insights payload for one already-resolved configured project.
pub fn query_project_insight_report_for_project(
    project: &ResolvedQueryProject,
    provider: Option<SourceKind>,
    limit: usize,
) -> Result<ProjectInsights> {
    let context = &project.context;
    query_project_insights(
        &context.root.database_path,
        &context.project.id,
        provider,
        limit,
    )
}

/// Queries the project insights payload for one configured project.
pub fn query_project_insight_report(
    root: Option<PathBuf>,
    project_id: &str,
    provider: Option<SourceKind>,
    limit: usize,
) -> Result<ProjectInsights> {
    let project = resolve_query_project(root, Some(project_id))?;
    query_project_insight_report_for_project(&project, provider, limit)
}

/// Stores the stable structured query errors that map onto `darc.error.v1`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum QueryProtocolError {
    #[error("{message}")]
    InvalidDataSessionId { input: String, message: String },
    #[error("{message}")]
    InvalidResolveSessionQuery { input: String, message: String },
    #[error("{message}")]
    UnknownDataSession {
        input: String,
        looks_like_prefix: bool,
        message: String,
    },
    #[error("{message}")]
    UnknownResolveSession {
        input: String,
        looks_like_prefix: bool,
        message: String,
    },
    #[error("{message}")]
    AmbiguousSession {
        query: String,
        matches: Vec<ResolvedSessionMatch>,
        truncated: bool,
        message: String,
    },
}

impl QueryProtocolError {
    /// Returns the stable machine-readable error code for the current query failure.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidDataSessionId { .. } | Self::InvalidResolveSessionQuery { .. } => {
                "invalid_session_id"
            }
            Self::UnknownDataSession { .. } | Self::UnknownResolveSession { .. } => {
                "unknown_session"
            }
            Self::AmbiguousSession { .. } => "ambiguous_session",
        }
    }

    /// Returns the optional structured JSON detail block for the current query failure.
    pub fn details(&self) -> JsonValue {
        match self {
            Self::InvalidDataSessionId { input, .. } => json!({ "session": input }),
            Self::InvalidResolveSessionQuery { input, .. } => json!({ "query": input }),
            Self::UnknownDataSession {
                input,
                looks_like_prefix,
                ..
            } => json!({ "session": input, "looks_like_prefix": looks_like_prefix }),
            Self::UnknownResolveSession {
                input,
                looks_like_prefix,
                ..
            } => json!({ "query": input, "looks_like_prefix": looks_like_prefix }),
            Self::AmbiguousSession {
                query,
                matches,
                truncated,
                ..
            } => json!({
                "query": query,
                "matches": matches,
                "truncated": truncated,
            }),
        }
    }

    /// Builds one structured invalid session-id error for a data command.
    pub fn invalid_data_session_id(input: &str) -> Self {
        Self::InvalidDataSessionId {
            input: input.to_owned(),
            message: format!("Session id `{input}` must be the full UUID."),
        }
    }

    /// Builds one structured invalid session-id error for `resolve-session`.
    pub fn invalid_resolve_session_query(input: &str) -> Self {
        Self::InvalidResolveSessionQuery {
            input: input.to_owned(),
            message: format!("Session query `{input}` must be a full UUID or UUID prefix."),
        }
    }

    /// Builds one structured unknown-session error for one strict data command.
    pub fn unknown_data_session(input: &str, looks_like_prefix: bool) -> Self {
        let message = if looks_like_prefix {
            format!(
                "No session found for id `{input}`. The session id must be the full UUID. Try `darc query resolve-session {input}` to expand a prefix."
            )
        } else {
            format!("No session found for id `{input}`.")
        };
        Self::UnknownDataSession {
            input: input.to_owned(),
            looks_like_prefix,
            message,
        }
    }

    /// Builds one structured unknown-session error for `resolve-session`.
    pub fn unknown_resolve_session(input: &str, looks_like_prefix: bool) -> Self {
        let message = if looks_like_prefix {
            format!("No session matched prefix `{input}`.")
        } else {
            format!("No session found for id `{input}`.")
        };
        Self::UnknownResolveSession {
            input: input.to_owned(),
            looks_like_prefix,
            message,
        }
    }

    /// Builds one structured ambiguity error for `resolve-session --pick-one`.
    pub fn ambiguous_session(
        query: &str,
        matches: Vec<ResolvedSessionMatch>,
        truncated: bool,
    ) -> Self {
        let provider_count = matches
            .iter()
            .map(|candidate| candidate.provider)
            .collect::<BTreeSet<_>>()
            .len();
        let project_count = matches
            .iter()
            .map(|candidate| candidate.project_id.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let match_count = matches.len();
        let message = if truncated {
            format!(
                "Prefix `{query}` matched at least {match_count} sessions across {project_count} projects and {provider_count} providers. Use a longer prefix or pass --project-id or --provider."
            )
        } else {
            format!(
                "Prefix `{query}` matched {match_count} sessions across {project_count} projects and {provider_count} providers. Use a longer prefix or pass --project-id or --provider."
            )
        };
        Self::AmbiguousSession {
            query: query.to_owned(),
            matches,
            truncated,
            message,
        }
    }

    /// Builds one structured ambiguity error for one strict data command.
    pub fn ambiguous_data_session(input: &str, matches: Vec<ResolvedSessionMatch>) -> Self {
        let provider_count = matches
            .iter()
            .map(|candidate| candidate.provider)
            .collect::<BTreeSet<_>>()
            .len();
        let message = format!(
            "Session id `{input}` matched {provider_count} providers in this project. Pass --provider to choose one."
        );
        Self::AmbiguousSession {
            query: input.to_owned(),
            matches,
            truncated: false,
            message,
        }
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

/// Identifies whether one resolved project query needs only config or a usable database.
#[derive(Debug, Clone, Copy)]
enum QueryProjectScope {
    ConfigOnly,
    Database,
}

/// Resolves one project-scoped query context from an explicit id or the current directory.
fn resolve_query_project_with_scope(
    root: Option<PathBuf>,
    project_id: Option<&str>,
    scope: QueryProjectScope,
) -> Result<ResolvedQueryProject> {
    let context = match project_id {
        Some(project_id) => load_project_config_context(root, project_id.trim())?,
        None => {
            let root = inspect_root(root);
            let current_dir =
                env::current_dir().context("unable to resolve the current working directory")?;
            let active_project = load_active_project(&current_dir, &root.resolved_root_path)?;
            ProjectQueryContext {
                root,
                project: active_project.project,
            }
        }
    };

    if matches!(scope, QueryProjectScope::Database) {
        ensure_database_exists(&context.root)?;
    }

    Ok(ResolvedQueryProject { context })
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

/// Identifies whether one session-id input is a full UUID, a plausible prefix, or invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionIdShape {
    FullUuid,
    Prefix,
    Invalid,
}

const UUID_TEXT_LEN: usize = 36;
const MIN_STRICT_SESSION_PREFIX_LEN: usize = 8;

/// Validates one resolver input and returns its trimmed canonical query text.
fn validate_resolve_session_query(query: &str) -> Result<&str> {
    let query = query.trim();
    match classify_resolve_session_input(query) {
        SessionIdShape::FullUuid | SessionIdShape::Prefix => Ok(query),
        SessionIdShape::Invalid => {
            Err(QueryProtocolError::invalid_resolve_session_query(query).into())
        }
    }
}

/// Validates one strict session id and resolves the canonical stored session id for the project.
fn validate_project_session_id(
    index_db_path: &Path,
    project_id: &str,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<String> {
    Ok(validate_project_session_ref(index_db_path, project_id, provider, session_id)?.session_id)
}

/// Validates one strict session-id filter without rejecting cross-provider matches.
fn validate_project_session_filter_id(
    index_db_path: &Path,
    project_id: &str,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<String> {
    let session_id = session_id.trim();
    match classify_strict_session_input(session_id) {
        SessionIdShape::Prefix => {
            return Err(QueryProtocolError::unknown_data_session(session_id, true).into());
        }
        SessionIdShape::Invalid => {
            return Err(QueryProtocolError::invalid_data_session_id(session_id).into());
        }
        SessionIdShape::FullUuid => {}
    }
    lookup_project_session_matches(index_db_path, project_id, provider, session_id, 1)?
        .into_iter()
        .next()
        .map(|resolved| resolved.session_id)
        .ok_or_else(|| QueryProtocolError::unknown_data_session(session_id, false).into())
}

/// Validates one strict session id and resolves its canonical provider/session identity.
fn validate_project_session_ref(
    index_db_path: &Path,
    project_id: &str,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<ResolvedQuerySession> {
    let session_id = session_id.trim();
    match classify_strict_session_input(session_id) {
        SessionIdShape::Prefix => {
            return Err(QueryProtocolError::unknown_data_session(session_id, true).into());
        }
        SessionIdShape::Invalid => {
            return Err(QueryProtocolError::invalid_data_session_id(session_id).into());
        }
        SessionIdShape::FullUuid => {}
    }
    let matches =
        lookup_project_session_matches(index_db_path, project_id, provider, session_id, 2)?;
    match matches.as_slice() {
        [] => Err(QueryProtocolError::unknown_data_session(session_id, false).into()),
        [resolved] => Ok(ResolvedQuerySession {
            provider: resolved.provider,
            session_id: resolved.session_id.clone(),
        }),
        _ => Err(QueryProtocolError::ambiguous_data_session(session_id, matches).into()),
    }
}

/// Classifies one data-command session id using the strict full-UUID contract.
fn classify_strict_session_input(input: &str) -> SessionIdShape {
    if is_full_uuid_text(input) {
        SessionIdShape::FullUuid
    } else if input.len() >= MIN_STRICT_SESSION_PREFIX_LEN && is_uuid_prefix_text(input) {
        SessionIdShape::Prefix
    } else {
        SessionIdShape::Invalid
    }
}

/// Classifies one resolver input, allowing any non-empty UUID prefix.
fn classify_resolve_session_input(input: &str) -> SessionIdShape {
    if is_full_uuid_text(input) {
        SessionIdShape::FullUuid
    } else if is_uuid_prefix_text(input) {
        SessionIdShape::Prefix
    } else {
        SessionIdShape::Invalid
    }
}

/// Returns whether one string is a full canonical UUID text value.
fn is_full_uuid_text(input: &str) -> bool {
    input.len() == UUID_TEXT_LEN
        && input
            .chars()
            .enumerate()
            .all(|(index, ch)| is_uuid_character_at(index, ch))
}

/// Returns whether one string is a non-empty prefix of canonical UUID text.
fn is_uuid_prefix_text(input: &str) -> bool {
    !input.is_empty()
        && input.len() < UUID_TEXT_LEN
        && input
            .chars()
            .enumerate()
            .all(|(index, ch)| is_uuid_character_at(index, ch))
}

/// Returns whether one character matches the canonical UUID grammar at one fixed position.
fn is_uuid_character_at(index: usize, ch: char) -> bool {
    match index {
        8 | 13 | 18 | 23 => ch == '-',
        _ => ch.is_ascii_hexdigit(),
    }
}

/// Stores the validated root and project context for one project-scoped query.
#[derive(Debug, Clone)]
struct ProjectQueryContext {
    root: RootInfo,
    project: ProjectConfig,
}
