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
    ShellCommandSummary, ToolUsageStat, TurnDetail, TurnDetailInsights, TurnInsights, TurnSummary,
    TurnsQueryData, WorkspaceDailyTimeStat, WorkspaceInsights, WorkspaceQueryData,
};
use darc_query::{
    ProjectIndexAggregate, list_project_index_aggregates, query_project_insights,
    query_project_sessions, query_search_turns as query_project_search_turns, query_session_turns,
    query_turn_detail, query_turn_insights, query_workspace_insights,
};

use crate::{
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

/// Queries the session-list payload for one configured project.
pub fn query_sessions(root: Option<PathBuf>, project_id: &str) -> Result<SessionsQueryData> {
    let context = load_project_query_context(root, project_id)?;
    query_project_sessions(&context.root.database_path, &context.project.id)
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
    include_raw: bool,
    include_insights: bool,
) -> Result<TurnDetail> {
    let context = load_project_query_context(root, project_id)?;
    query_turn_detail(
        &context.root.database_path,
        &context.project.id,
        provider,
        session_id,
        turn_ordinal,
        include_raw,
        include_insights,
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
    let root = inspect_root(root);
    ensure_config_exists(&root)?;
    ensure_database_exists(&root)?;

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

/// Stores the validated root and project context for one project-scoped query.
struct ProjectQueryContext {
    root: RootInfo,
    project: ProjectConfig,
}
