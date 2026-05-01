use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use darc_query::{ProjectIndexAggregate, list_project_index_aggregates};
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::{
    active_project::load_active_project,
    config::{ProjectConfig, SharedConfig, SourceKind, load_config},
    init::normalize_project_config,
    query::{RootInfo, inspect_root},
    sync::{SyncOptions, SyncPlan, prepare_sync_from},
};

/// Stores the project-scoped status report shown by `darc status`.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectStatusReport {
    pub root: RootInfo,
    pub sources: Vec<StatusSource>,
    pub project: StatusProject,
}

impl ProjectStatusReport {
    /// Returns whether the optional sync dry-run failed.
    pub fn has_failed_check(&self) -> bool {
        self.project.has_failed_check()
    }
}

/// Stores the workspace-scoped status report shown by `darc status --workspace`.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceStatusReport {
    pub root: RootInfo,
    pub sources: Vec<StatusSource>,
    pub projects: Vec<StatusProject>,
}

impl WorkspaceStatusReport {
    /// Returns whether any optional sync dry-run failed.
    pub fn has_failed_check(&self) -> bool {
        self.projects.iter().any(StatusProject::has_failed_check)
    }

    /// Returns the total indexed session count across configured projects.
    pub fn total_session_count(&self) -> u64 {
        self.projects
            .iter()
            .map(|project| project.session_count)
            .sum()
    }

    /// Returns the total indexed turn count across configured projects.
    pub fn total_turn_count(&self) -> u64 {
        self.projects.iter().map(|project| project.turn_count).sum()
    }

    /// Returns the latest indexed activity timestamp across configured projects.
    pub fn latest_activity_at(&self) -> Option<&str> {
        self.projects
            .iter()
            .filter_map(|project| project.last_activity_at.as_deref())
            .max()
    }
}

/// Stores one configured source's availability for status output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusSource {
    pub kind: SourceKind,
    pub configured: bool,
    pub enabled: bool,
    pub path: Option<PathBuf>,
    pub path_exists: bool,
}

/// Stores one configured project's archive, index, and optional sync-check status.
#[derive(Debug, Clone, Serialize)]
pub struct StatusProject {
    pub id: String,
    pub name: String,
    pub local_path: PathBuf,
    pub resolved_project_root: Option<PathBuf>,
    pub sessions_root: PathBuf,
    pub git_upstream: Option<String>,
    pub known_path_count: usize,
    pub archive_exists: bool,
    pub manifest_path: PathBuf,
    pub last_sync_at: Option<String>,
    pub session_count: u64,
    pub turn_count: u64,
    pub last_activity_at: Option<String>,
    pub issues: Vec<String>,
    pub sync_check: Option<StatusSyncCheck>,
}

impl StatusProject {
    /// Returns whether the optional sync dry-run failed for this project.
    pub fn has_failed_check(&self) -> bool {
        matches!(self.sync_check, Some(StatusSyncCheck::Failed(_)))
    }
}

/// Stores one optional sync dry-run outcome for status output.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "data")]
pub enum StatusSyncCheck {
    Planned(StatusSyncPlan),
    Failed(StatusSyncFailure),
}

/// Stores the non-mutating sync plan summary used by `darc status --check`.
#[derive(Debug, Clone, Serialize)]
pub struct StatusSyncPlan {
    pub sources: Vec<SourceKind>,
    pub sessions_to_copy: usize,
    pub sessions_unchanged: usize,
    pub auxiliary_to_copy: usize,
    pub auxiliary_unchanged: usize,
    pub new_known_path_count: usize,
    pub manifest_written: bool,
    pub config_written: bool,
    pub warnings: Vec<String>,
}

/// Stores one failed sync dry-run for status output.
#[derive(Debug, Clone, Serialize)]
pub struct StatusSyncFailure {
    pub message: String,
}

/// Builds the project-scoped status report for the current working directory.
pub fn status_project(root: Option<PathBuf>, check: bool) -> Result<ProjectStatusReport> {
    let root_info = inspect_root(root);
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    let active_project = load_active_project(&current_dir, &root_info.resolved_root_path)?;
    let config = load_status_config(&root_info)?;
    let aggregates = load_project_aggregates(&root_info)?;
    let aggregate_map = aggregate_map(aggregates);
    let sync_check = check.then(|| run_sync_check(&current_dir, &root_info.resolved_root_path));
    let project_id = active_project.project.id.clone();
    let project = build_status_project(
        active_project.project,
        Some(active_project.current_root),
        aggregate_map.get(&project_id),
        sync_check,
    );

    Ok(ProjectStatusReport {
        root: root_info,
        sources: status_sources(&config),
        project,
    })
}

/// Builds the workspace-scoped status report for one darc root.
pub fn status_workspace(root: Option<PathBuf>, check: bool) -> Result<WorkspaceStatusReport> {
    let root_info = inspect_root(root);
    let config = load_status_config(&root_info)?;
    let aggregates = load_project_aggregates(&root_info)?;
    let aggregate_map = aggregate_map(aggregates);
    let mut projects = config
        .projects
        .iter()
        .map(|project| {
            let sync_check =
                check.then(|| run_sync_check(&project.local_path, &root_info.resolved_root_path));
            build_status_project(
                project.clone(),
                None,
                aggregate_map.get(&project.id),
                sync_check,
            )
        })
        .collect::<Vec<_>>();

    projects.sort_by(|left, right| {
        right
            .last_activity_at
            .cmp(&left.last_activity_at)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });

    Ok(WorkspaceStatusReport {
        root: root_info,
        sources: status_sources(&config),
        projects,
    })
}

/// Loads and normalizes the shared config required by status commands.
fn load_status_config(root: &RootInfo) -> Result<SharedConfig> {
    if !root.available.config_exists {
        bail!(
            "shared config not found at {}\nrun `darc init --root {}` from a project root first",
            root.config_path.display(),
            root.resolved_root_path.display()
        );
    }

    let mut config = load_config(&root.config_path)?;
    config.projects = config
        .projects
        .into_iter()
        .map(normalize_project_config)
        .collect::<Result<Vec<_>>>()?;
    Ok(config)
}

/// Loads indexed project aggregates when the workspace database exists.
fn load_project_aggregates(root: &RootInfo) -> Result<Vec<ProjectIndexAggregate>> {
    if !root.available.database_exists {
        return Ok(Vec::new());
    }
    list_project_index_aggregates(&root.database_path).with_context(|| {
        format!(
            "failed to query darc index database at {}",
            root.database_path.display()
        )
    })
}

/// Returns one aggregate lookup keyed by project id.
fn aggregate_map(
    aggregates: Vec<ProjectIndexAggregate>,
) -> BTreeMap<String, ProjectIndexAggregate> {
    aggregates
        .into_iter()
        .map(|aggregate| (aggregate.project_id.clone(), aggregate))
        .collect()
}

/// Builds one status row for a configured project.
fn build_status_project(
    project: ProjectConfig,
    resolved_project_root: Option<PathBuf>,
    aggregate: Option<&ProjectIndexAggregate>,
    sync_check: Option<StatusSyncCheck>,
) -> StatusProject {
    let archive_exists = project.sessions_root.exists();
    let manifest_path = project.sessions_root.join(".manifest.json");
    let mut issues = Vec::new();

    if !project.local_path.exists() {
        issues.push(format!(
            "project root is missing: {}",
            project.local_path.display()
        ));
    }
    if !archive_exists {
        issues.push(format!(
            "archive is missing: {}",
            project.sessions_root.display()
        ));
    }

    let last_sync_at = if manifest_path.exists() {
        match read_last_sync_at(&manifest_path) {
            Ok(last_sync_at) => last_sync_at,
            Err(error) => {
                issues.push(format!(
                    "sync manifest could not be read: {}",
                    error.root_cause()
                ));
                None
            }
        }
    } else {
        None
    };

    StatusProject {
        id: project.id,
        name: project.name,
        local_path: project.local_path,
        resolved_project_root,
        sessions_root: project.sessions_root,
        git_upstream: project.git_upstream,
        known_path_count: project.known_paths.len(),
        archive_exists,
        manifest_path,
        last_sync_at,
        session_count: aggregate.map_or(0, |aggregate| aggregate.session_count),
        turn_count: aggregate.map_or(0, |aggregate| aggregate.turn_count),
        last_activity_at: aggregate.and_then(|aggregate| aggregate.last_activity_at.clone()),
        issues,
        sync_check,
    }
}

/// Builds source availability rows for all supported source kinds.
fn status_sources(config: &SharedConfig) -> Vec<StatusSource> {
    let claude = config.sources.claude.as_ref().map(|source| {
        status_source(
            SourceKind::Claude,
            true,
            source.enabled,
            Some(source.projects_root.clone()),
        )
    });
    let codex = config.sources.codex.as_ref().map(|source| {
        status_source(
            SourceKind::Codex,
            true,
            source.enabled,
            Some(source.sessions_root.clone()),
        )
    });

    vec![
        claude.unwrap_or_else(|| status_source(SourceKind::Claude, false, false, None)),
        codex.unwrap_or_else(|| status_source(SourceKind::Codex, false, false, None)),
    ]
}

/// Builds one source availability row.
fn status_source(
    kind: SourceKind,
    configured: bool,
    enabled: bool,
    path: Option<PathBuf>,
) -> StatusSource {
    let path_exists = path.as_deref().is_some_and(Path::exists);
    StatusSource {
        kind,
        configured,
        enabled,
        path,
        path_exists,
    }
}

/// Runs the non-mutating sync planner for one project path.
fn run_sync_check(current_dir: &Path, root: &Path) -> StatusSyncCheck {
    match prepare_sync_from(
        current_dir,
        root.to_path_buf(),
        SyncOptions {
            provider_filter: Vec::new(),
        },
    ) {
        Ok(plan) => StatusSyncCheck::Planned(sync_plan_status(&plan)),
        Err(error) => StatusSyncCheck::Failed(StatusSyncFailure {
            message: format!("{error:#}"),
        }),
    }
}

/// Converts one prepared sync plan into a stable status summary.
fn sync_plan_status(plan: &SyncPlan) -> StatusSyncPlan {
    StatusSyncPlan {
        sources: plan.sources.clone(),
        sessions_to_copy: plan.sessions_to_copy(),
        sessions_unchanged: plan.sessions_unchanged,
        auxiliary_to_copy: plan.auxiliary_to_copy(),
        auxiliary_unchanged: plan.auxiliary_unchanged,
        new_known_path_count: plan.new_known_paths.len(),
        manifest_written: plan.manifest_written(),
        config_written: plan.config_written(),
        warnings: plan.warnings.clone(),
    }
}

/// Reads the latest sync timestamp from one project sync manifest.
fn read_last_sync_at(path: &Path) -> Result<Option<String>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let manifest: JsonValue =
        serde_json::from_str(&content).context("failed to parse sync manifest")?;
    let mut latest = None;

    for section in ["sessions", "auxiliary"] {
        let Some(entries) = manifest.get(section).and_then(JsonValue::as_object) else {
            continue;
        };
        for entry in entries.values() {
            let Some(synced_at) = entry.get("synced_at").and_then(JsonValue::as_str) else {
                continue;
            };
            if latest.as_deref().is_none_or(|current| synced_at > current) {
                latest = Some(synced_at.to_owned());
            }
        }
    }

    Ok(latest)
}
