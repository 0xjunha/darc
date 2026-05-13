use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use darc_paths::SourceKind;
pub use darc_query::{
    ActiveProjectSummary, DEFAULT_MATCHED_PATH_LIMIT, DEFAULT_QUERY_PAGE_LIMIT,
    DEFAULT_RESOLVE_SESSION_MATCH_LIMIT, DEFAULT_SEARCH_MATCH_LIMIT,
    DEFAULT_SESSION_BUNDLE_TURN_LIMIT, DEFAULT_TURN_STEP_LIMIT,
    DEFAULT_WORKSPACE_RECENT_SESSION_LIMIT, DailyTimeStat, FilePivotSummary, FileSessionSummary,
    FileUsageStat, FilesQueryData, FilesQueryMode, FilesQueryRequest, ProjectInsights,
    ProjectSummary, ProjectTimeStat, ResolveSessionQueryData, ResolveSessionQueryRequest,
    ResolvedSessionMatch, RootAvailability, RootInfo, SearchEvidenceField, SearchMode,
    SearchSnippetMatcher, SearchTurnHit, SearchTurnMatch, SearchTurnsQueryData, SearchTurnsRequest,
    SessionBundleQueryData, SessionBundleQueryRequest, SessionBundleView, SessionFileSummary,
    SessionFilesQueryData, SessionFilesQueryRequest, SessionKind, SessionRuntimeStat,
    SessionSummary, SessionsQueryData, SessionsQueryRequest, SessionsView, ShellCommandSummary,
    ToolUsageStat, TurnDetail, TurnDetailInsights, TurnDetailOptions, TurnInsights, TurnSummary,
    TurnsQueryData, TurnsQueryRequest, TurnsView, WorkspaceDailyTimeStat, WorkspaceInsights,
    WorkspaceQueryData, search_snippet_match_range,
};
use darc_query::{
    ProjectIndexAggregate, display_path_for_access, list_project_index_aggregates,
    lookup_project_session_matches, query_project_files, query_project_insights,
    query_project_session_bundle, query_project_session_files, query_project_sessions,
    query_project_turns as query_index_turns,
    query_resolve_sessions as query_index_resolve_sessions,
    query_search_turns as query_project_search_turns,
    query_session_turn_details as query_project_session_turn_details, query_turn_detail,
    query_turn_insights, query_workspace_insights,
};
use darc_store::{INDEX_DB_FILE_NAME, IndexDatabaseRebuildRecommendation};
use serde_json::{Value as JsonValue, json};
use thiserror::Error;

use crate::{
    active_project::{is_no_active_project_error, load_active_project},
    config::{ProjectConfig, SharedConfig, load_config},
    constants::CONFIG_FILE_NAME,
    default_root_path, index_rebuild_command,
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
            active_project: None,
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
                active_project: None,
                projects: Vec::new(),
            };
        }
    };

    let active_project = match env::current_dir()
        .context("unable to resolve the current working directory")
        .and_then(|current_dir| load_active_project(&current_dir, &root_info.resolved_root_path))
    {
        Ok(active_project) => Some(ActiveProjectSummary {
            project_id: active_project.project.id,
            project_name: active_project.project.name,
            current_root: active_project.current_root,
        }),
        Err(error) if is_no_active_project_error(&error) => None,
        Err(error) => {
            root_info.issues.push(format!(
                "Active project could not be resolved: {}",
                error.root_cause()
            ));
            None
        }
    };

    let aggregate_map = if root_info.available.database_exists {
        match list_project_index_aggregates(&root_info.database_path) {
            Ok(aggregates) => aggregate_map(aggregates),
            Err(error) => {
                root_info
                    .issues
                    .push(format_workspace_database_issue(&error));
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
        active_project,
        projects,
    }
}

/// Formats one workspace database issue with index rebuild guidance when possible.
fn format_workspace_database_issue(error: &anyhow::Error) -> String {
    if let Some(rebuild) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<IndexDatabaseRebuildRecommendation>())
    {
        return format!(
            "Darc database could not be queried: local SQLite index at {} needs to be rebuilt; run `{}` to rebuild the shared index cache from archived sessions",
            rebuild.path().display(),
            index_rebuild_command(rebuild.path())
        );
    }

    format!("Darc database could not be queried: {}", error.root_cause())
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
    request: SessionsQueryRequest<'_>,
) -> Result<SessionsQueryData> {
    let context = &project.context;
    let mut data = query_project_sessions(
        &context.root.database_path,
        SessionsQueryRequest {
            project_id: &context.project.id,
            project_root: Some(context.project.local_path.as_path()),
            provider: request.provider,
            ..request
        },
    )?;
    normalize_sessions_output_paths(&mut data, &context.project);
    Ok(data)
}

/// Queries the session-list payload for one configured project.
pub fn query_sessions(
    root: Option<PathBuf>,
    request: SessionsQueryRequest<'_>,
) -> Result<SessionsQueryData> {
    let project = resolve_query_project(root, Some(request.project_id))?;
    query_sessions_for_project(&project, request)
}

/// Queries one file-pivot payload for one already-resolved configured project.
pub fn query_files_for_project(
    project: &ResolvedQueryProject,
    request: FilesQueryRequest<'_>,
) -> Result<FilesQueryData> {
    let context = &project.context;
    let mut data = query_project_files(
        &context.root.database_path,
        FilesQueryRequest {
            project_id: &context.project.id,
            project_root: Some(context.project.local_path.as_path()),
            provider: request.provider,
            ..request
        },
    )?;
    normalize_files_output_paths(&mut data, &context.project);
    Ok(data)
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

/// Resolves one read-command session id or UUID prefix against one project and optional provider filter.
pub fn resolve_query_session_id(
    root: Option<PathBuf>,
    project_id: &str,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<String> {
    let project = resolve_query_project(root, Some(project_id))?;
    resolve_query_session_id_for_project(&project, provider, session_id)
}

/// Resolves one read-command session id or UUID prefix against one already-resolved project.
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

/// Resolves one search session-id filter, accepting an unambiguous UUID prefix.
pub fn resolve_query_search_session_id_for_project(
    project: &ResolvedQueryProject,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<String> {
    let context = &project.context;
    validate_project_search_session_filter_id(
        &context.root.database_path,
        &context.project.id,
        provider,
        session_id,
    )
}

/// Resolves one read-command session id or UUID prefix plus provider against one project.
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
    limit: usize,
    offset: usize,
) -> Result<SessionFilesQueryData> {
    let context = &project.context;
    let mut data = query_project_session_files(
        &context.root.database_path,
        SessionFilesQueryRequest {
            project_id: &context.project.id,
            project_root: Some(context.project.local_path.as_path()),
            provider,
            session_id,
            limit,
            offset,
        },
    )?;
    normalize_session_files_output_paths(&mut data, &context.project);
    Ok(data)
}

/// Queries one session-scoped per-file access summary payload.
pub fn query_session_files(
    root: Option<PathBuf>,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    limit: usize,
    offset: usize,
) -> Result<SessionFilesQueryData> {
    let project = resolve_query_project(root, Some(project_id))?;
    query_session_files_for_project(&project, provider, session_id, limit, offset)
}

/// Queries one composite session bundle for one already-resolved configured provider session.
pub fn query_session_bundle_for_project(
    project: &ResolvedQueryProject,
    request: SessionBundleQueryRequest<'_>,
) -> Result<SessionBundleQueryData> {
    let context = &project.context;
    let mut data = query_project_session_bundle(
        &context.root.database_path,
        SessionBundleQueryRequest {
            project_id: &context.project.id,
            project_root: Some(context.project.local_path.as_path()),
            ..request
        },
    )?;
    normalize_session_bundle_output_paths(&mut data, &context.project);
    Ok(data)
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
    let mut data = query_turn_detail(
        &context.root.database_path,
        &context.project.id,
        Some(context.project.local_path.as_path()),
        provider,
        session_id,
        turn_ordinal,
        options,
    )?;
    normalize_turn_detail_output_paths(&mut data, &context.project);
    Ok(data)
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
        Some(context.project.local_path.as_path()),
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
    let mut data = query_turn_insights(
        &context.root.database_path,
        &context.project.id,
        Some(context.project.local_path.as_path()),
        provider,
        session_id,
        turn_ordinal,
    )?;
    normalize_turn_insights_output_paths(&mut data, &context.project);
    Ok(data)
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
    let mut data = query_project_search_turns(
        &context.root.database_path,
        SearchTurnsRequest {
            project_id: &context.project.id,
            project_root: Some(context.project.local_path.as_path()),
            ..request
        },
    )?;
    normalize_search_output_paths(&mut data, &context.project);
    Ok(data)
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
    recent_session_limit: usize,
    recent_session_offset: usize,
) -> Result<WorkspaceInsights> {
    let root_info = inspect_root(root);
    ensure_database_exists(&root_info)?;
    query_workspace_insights(
        &root_info.database_path,
        window_days,
        recent_session_limit,
        recent_session_offset,
    )
}

/// Queries the project insights payload for one already-resolved configured project.
pub fn query_project_insight_report_for_project(
    project: &ResolvedQueryProject,
    provider: Option<SourceKind>,
    limit: usize,
) -> Result<ProjectInsights> {
    let context = &project.context;
    let mut data = query_project_insights(
        &context.root.database_path,
        &context.project.id,
        Some(context.project.local_path.as_path()),
        provider,
        limit,
    )?;
    normalize_project_insights_output_paths(&mut data, &context.project);
    Ok(data)
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
            message: format!("Session id `{input}` must be a UUID or UUID prefix."),
        }
    }

    /// Builds one structured invalid session-id error for `resolve-session`.
    pub fn invalid_resolve_session_query(input: &str) -> Self {
        Self::InvalidResolveSessionQuery {
            input: input.to_owned(),
            message: format!("Session query `{input}` must be a full UUID or UUID prefix."),
        }
    }

    /// Builds one structured unknown-session error for one data command.
    pub fn unknown_data_session(input: &str, looks_like_prefix: bool) -> Self {
        let message = if looks_like_prefix {
            format!("No session matched prefix `{input}`.")
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

    /// Builds one structured ambiguity error for one data command.
    pub fn ambiguous_data_session(
        input: &str,
        matches: Vec<ResolvedSessionMatch>,
        truncated: bool,
    ) -> Self {
        let match_count = matches.len();
        let provider_count = matches
            .iter()
            .map(|candidate| candidate.provider)
            .collect::<BTreeSet<_>>()
            .len();
        let message = if truncated {
            format!(
                "Session id or prefix `{input}` matched at least {match_count} sessions across {provider_count} providers in this project. Use a longer prefix or pass --provider."
            )
        } else {
            format!(
                "Session id or prefix `{input}` matched {match_count} sessions across {provider_count} providers in this project. Use a longer prefix or pass --provider."
            )
        };
        Self::AmbiguousSession {
            query: input.to_owned(),
            matches,
            truncated,
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

/// Normalizes session-list file paths against every configured project root.
fn normalize_sessions_output_paths(data: &mut SessionsQueryData, project: &ProjectConfig) {
    for session in &mut data.sessions {
        normalize_session_summary_paths(session, project);
    }
}

/// Normalizes one session summary's edited file paths against every configured project root.
fn normalize_session_summary_paths(session: &mut SessionSummary, project: &ProjectConfig) {
    session.edited_files = normalize_path_list(std::mem::take(&mut session.edited_files), project);
}

/// Normalizes file-query output paths against every configured project root.
fn normalize_files_output_paths(data: &mut FilesQueryData, project: &ProjectConfig) {
    for file in &mut data.files {
        file.path = normalize_known_project_path(&file.path, project);
    }
    for session in &mut data.sessions {
        normalize_file_session_summary_paths(session, project);
    }
}

/// Normalizes one file-session summary's matched paths against every configured project root.
fn normalize_file_session_summary_paths(session: &mut FileSessionSummary, project: &ProjectConfig) {
    session.matched_paths =
        normalize_path_list(std::mem::take(&mut session.matched_paths), project);
    if !session.matched_paths_truncated {
        session.matched_paths_count =
            u64::try_from(session.matched_paths.len()).unwrap_or(u64::MAX);
    }
}

/// Normalizes one session-files payload against every configured project root.
fn normalize_session_files_output_paths(data: &mut SessionFilesQueryData, project: &ProjectConfig) {
    for file in &mut data.files {
        normalize_session_file_summary_path(file, project);
    }
}

/// Normalizes one session file summary against every configured project root.
fn normalize_session_file_summary_path(file: &mut SessionFileSummary, project: &ProjectConfig) {
    let original = file.path.clone();
    let normalized = normalize_known_project_path(&original, project);
    if file.repo_relative_path.is_none() && normalized != original {
        file.repo_relative_path = Some(normalized.clone());
    }
    file.path = normalized;
    if let Some(repo_relative_path) = file.repo_relative_path.take() {
        file.repo_relative_path = Some(normalize_known_project_path(&repo_relative_path, project));
    }
}

/// Normalizes one session-bundle payload against every configured project root.
fn normalize_session_bundle_output_paths(
    data: &mut SessionBundleQueryData,
    project: &ProjectConfig,
) {
    normalize_session_summary_paths(&mut data.session, project);
    normalize_session_files_output_paths(&mut data.session_files, project);
    for turn in &mut data.turns {
        normalize_turn_detail_output_paths(turn, project);
    }
}

/// Normalizes one turn-detail payload against every configured project root.
fn normalize_turn_detail_output_paths(data: &mut TurnDetail, project: &ProjectConfig) {
    if let Some(insights) = data.insights.as_mut() {
        normalize_turn_detail_insights_paths(insights, project);
    }
}

/// Normalizes one embedded turn-insights payload against every configured project root.
fn normalize_turn_detail_insights_paths(
    insights: &mut TurnDetailInsights,
    project: &ProjectConfig,
) {
    normalize_file_usage_paths(&mut insights.files, project);
}

/// Normalizes one turn-insights payload against every configured project root.
fn normalize_turn_insights_output_paths(data: &mut TurnInsights, project: &ProjectConfig) {
    normalize_file_usage_paths(&mut data.files, project);
}

/// Normalizes search matched paths against every configured project root.
fn normalize_search_output_paths(data: &mut SearchTurnsQueryData, project: &ProjectConfig) {
    for hit in &mut data.hits {
        normalize_search_hit_paths(hit, project);
    }
}

/// Normalizes one search hit's matched paths against every configured project root.
fn normalize_search_hit_paths(hit: &mut SearchTurnHit, project: &ProjectConfig) {
    hit.matched_paths = normalize_path_list(std::mem::take(&mut hit.matched_paths), project);
    if !hit.matched_paths_truncated {
        hit.matched_paths_count = u64::try_from(hit.matched_paths.len()).unwrap_or(u64::MAX);
    }
}

/// Normalizes one project-insights payload against every configured project root.
fn normalize_project_insights_output_paths(data: &mut ProjectInsights, project: &ProjectConfig) {
    normalize_file_usage_paths(&mut data.most_read_files, project);
    normalize_file_usage_paths(&mut data.most_written_files, project);
}

/// Normalizes file-usage paths against every configured project root.
fn normalize_file_usage_paths(files: &mut [FileUsageStat], project: &ProjectConfig) {
    for file in files {
        file.path = normalize_known_project_path(&file.path, project);
    }
}

/// Normalizes and deduplicates one path list against every configured project root.
fn normalize_path_list(paths: Vec<String>, project: &ProjectConfig) -> Vec<String> {
    paths
        .into_iter()
        .map(|path| normalize_known_project_path(&path, project))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Converts an absolute path under one configured project root to project-relative text.
fn normalize_known_project_path(path: &str, project: &ProjectConfig) -> String {
    let trimmed = path.trim();
    project_path_roots(project)
        .find_map(|root| display_path_for_access(Some(root), None, trimmed))
        .unwrap_or_else(|| trimmed.to_owned())
}

/// Iterates over every configured root that can identify this project.
fn project_path_roots(project: &ProjectConfig) -> impl Iterator<Item = &Path> {
    std::iter::once(project.local_path.as_path())
        .chain(project.known_paths.iter().map(PathBuf::as_path))
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

/// Validates one session id or UUID prefix and resolves the canonical stored session id.
fn validate_project_session_id(
    index_db_path: &Path,
    project_id: &str,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<String> {
    Ok(validate_project_session_ref(index_db_path, project_id, provider, session_id)?.session_id)
}

/// Validates one search session-id filter and resolves a canonical stored session id.
fn validate_project_search_session_filter_id(
    index_db_path: &Path,
    project_id: &str,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<String> {
    let session_id = session_id.trim();
    match classify_resolve_session_input(session_id) {
        SessionIdShape::Invalid => {
            return Err(QueryProtocolError::invalid_data_session_id(session_id).into());
        }
        SessionIdShape::FullUuid | SessionIdShape::Prefix => {}
    }
    let is_full_uuid = is_full_uuid_text(session_id);
    let (matches, truncated) =
        lookup_project_session_matches_for_error(index_db_path, project_id, provider, session_id)?;
    match matches.as_slice() {
        [] => Err(QueryProtocolError::unknown_data_session(session_id, !is_full_uuid).into()),
        [resolved] => Ok(resolved.session_id.clone()),
        _ => {
            let distinct_session_ids = matches
                .iter()
                .map(|candidate| candidate.session_id.as_str())
                .collect::<BTreeSet<_>>();
            if distinct_session_ids.len() == 1 && (is_full_uuid || !truncated) {
                Ok(matches[0].session_id.clone())
            } else {
                Err(
                    QueryProtocolError::ambiguous_data_session(session_id, matches, truncated)
                        .into(),
                )
            }
        }
    }
}

/// Validates one session id or UUID prefix and resolves its canonical provider/session identity.
fn validate_project_session_ref(
    index_db_path: &Path,
    project_id: &str,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<ResolvedQuerySession> {
    let session_id = session_id.trim();
    match classify_resolve_session_input(session_id) {
        SessionIdShape::Invalid => {
            return Err(QueryProtocolError::invalid_data_session_id(session_id).into());
        }
        SessionIdShape::FullUuid | SessionIdShape::Prefix => {}
    }
    let (matches, truncated) =
        lookup_project_session_matches_for_error(index_db_path, project_id, provider, session_id)?;
    match matches.as_slice() {
        [] => Err(QueryProtocolError::unknown_data_session(
            session_id,
            !is_full_uuid_text(session_id),
        )
        .into()),
        [resolved] => Ok(ResolvedQuerySession {
            provider: resolved.provider,
            session_id: resolved.session_id.clone(),
        }),
        _ => Err(QueryProtocolError::ambiguous_data_session(session_id, matches, truncated).into()),
    }
}

/// Looks up a bounded session-match preview plus whether more matches were omitted.
fn lookup_project_session_matches_for_error(
    index_db_path: &Path,
    project_id: &str,
    provider: Option<SourceKind>,
    session_id: &str,
) -> Result<(Vec<ResolvedSessionMatch>, bool)> {
    let limit = DEFAULT_RESOLVE_SESSION_MATCH_LIMIT
        .checked_add(1)
        .context("session match limit exceeds usize range")?;
    let mut matches =
        lookup_project_session_matches(index_db_path, project_id, provider, session_id, limit)?;
    let truncated = matches.len() > DEFAULT_RESOLVE_SESSION_MATCH_LIMIT;
    if truncated {
        matches.truncate(DEFAULT_RESOLVE_SESSION_MATCH_LIMIT);
    }
    Ok((matches, truncated))
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

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Result;
    use darc_test_utils::{unique_test_dir, write_file};

    use super::query_workspace;
    use crate::{
        config::{ProjectConfig, SharedConfig, SourcesConfig},
        constants::CONFIG_FILE_NAME,
        index_rebuild_command,
    };

    #[test]
    fn query_workspace_issue_recommends_rebuild_for_unusable_index() -> Result<()> {
        let root = unique_test_dir("workspace-query-rebuild");
        let project_root = root.join("repo");
        let sessions_root = root.join("projects/repo-123/sessions");
        fs::create_dir_all(&project_root)?;
        let config = SharedConfig::new(
            root.clone(),
            vec![ProjectConfig {
                id: "repo-123".to_owned(),
                name: "repo".to_owned(),
                local_path: project_root,
                git_upstream: None,
                sessions_root,
                known_paths: Vec::new(),
            }],
            SourcesConfig::default(),
        );
        write_file(
            &root.join(CONFIG_FILE_NAME),
            &toml::to_string_pretty(&config)?,
        )?;
        let index_db_path = root.join(darc_store::INDEX_DB_FILE_NAME);
        write_file(&index_db_path, "not a sqlite database")?;

        let workspace = query_workspace(Some(root));
        let issue = workspace
            .root
            .issues
            .first()
            .expect("workspace should report database issue");

        assert!(issue.contains("Darc database could not be queried"));
        assert!(issue.contains("needs to be rebuilt"));
        assert!(issue.contains(&format!(
            "run `{}`",
            index_rebuild_command(&workspace.root.database_path)
        )));
        assert!(issue.contains(&workspace.root.database_path.display().to_string()));
        assert_eq!(workspace.projects.len(), 1);
        Ok(())
    }
}
