use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use darc_paths::{
    normalize_project_path, normalized_known_paths, project_path_set,
    project_path_set_text_aliases, project_path_text_aliases, try_git_output,
};
pub(crate) use darc_sync::SyncProgress;
use darc_sync::{ClaudeSource, CodexSource, SyncRequest};

use crate::{
    active_project::{ActiveProject, load_active_project},
    config::{ProjectConfig, SharedConfig, SourceKind},
    default_root_path,
    project::write_shared_config,
};

/// Collects optional filters for the `sync` workflow.
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    pub provider_filter: Vec<SourceKind>,
}

/// Describes a prepared sync before any writes happen.
#[derive(Debug, Clone)]
pub struct SyncPlan {
    pub project_name: String,
    pub project_root: PathBuf,
    pub sessions_root: PathBuf,
    pub sources: Vec<SourceKind>,
    pub sessions_unchanged: usize,
    pub auxiliary_unchanged: usize,
    pub new_known_paths: Vec<PathBuf>,
    pub warnings: Vec<String>,
    writes: PreparedSyncWrites,
}

impl SyncPlan {
    /// Returns how many parent session files this sync would copy.
    pub fn sessions_to_copy(&self) -> usize {
        self.writes.engine_plan.sessions_to_copy()
    }

    /// Returns how many auxiliary files this sync would copy.
    pub fn auxiliary_to_copy(&self) -> usize {
        self.writes.engine_plan.auxiliary_to_copy()
    }

    /// Returns whether executing this plan would rewrite the manifest.
    pub fn manifest_written(&self) -> bool {
        self.writes.engine_plan.manifest_written()
    }

    /// Returns whether executing this plan would rewrite the shared config.
    pub fn config_written(&self) -> bool {
        self.writes.config.is_some()
    }
}

/// Reports the results of an executed sync.
#[derive(Debug, Clone)]
pub struct SyncReport {
    pub project_name: String,
    pub project_root: PathBuf,
    pub sessions_root: PathBuf,
    pub sources: Vec<SourceKind>,
    pub sessions_copied: usize,
    pub sessions_unchanged: usize,
    pub auxiliary_copied: usize,
    pub auxiliary_unchanged: usize,
    pub new_known_paths: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub manifest_written: bool,
    pub config_written: bool,
}

/// Stores the private write operations captured during sync planning.
#[derive(Debug, Clone)]
struct PreparedSyncWrites {
    engine_plan: darc_sync::SyncPlan,
    config_path: PathBuf,
    config: Option<SharedConfig>,
}

/// Plans a project-scoped sync using the current working directory as the active project.
pub fn prepare_sync(root: Option<PathBuf>, options: SyncOptions) -> Result<SyncPlan> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    prepare_sync_from(
        &current_dir,
        root.unwrap_or_else(default_root_path),
        options,
    )
}

/// Executes a prepared sync by copying files and then persisting config updates.
pub fn execute_sync(plan: SyncPlan) -> Result<SyncReport> {
    let mut progress = |_| {};
    execute_sync_with_progress(plan, &mut progress)
}

/// Executes a prepared sync while reporting copied session progress.
pub(crate) fn execute_sync_with_progress(
    plan: SyncPlan,
    mut progress: impl FnMut(SyncProgress),
) -> Result<SyncReport> {
    let SyncPlan {
        project_name,
        project_root,
        sessions_root,
        sources,
        sessions_unchanged: _sessions_unchanged,
        auxiliary_unchanged: _auxiliary_unchanged,
        new_known_paths,
        warnings,
        writes,
    } = plan;
    let PreparedSyncWrites {
        engine_plan,
        config_path,
        config,
    } = writes;

    let report = darc_sync::execute_sync_with_progress(engine_plan, &mut progress)?;
    if let Some(config) = &config {
        write_shared_config(&config_path, config)?;
    }

    Ok(SyncReport {
        project_name,
        project_root,
        sessions_root,
        sources,
        sessions_copied: report.sessions_copied,
        sessions_unchanged: report.sessions_unchanged,
        auxiliary_copied: report.auxiliary_copied,
        auxiliary_unchanged: report.auxiliary_unchanged,
        new_known_paths,
        warnings,
        manifest_written: report.manifest_written,
        config_written: config.is_some(),
    })
}

/// Plans a sync from one explicit working directory and darc root.
pub(crate) fn prepare_sync_from(
    current_dir: &Path,
    root: PathBuf,
    options: SyncOptions,
) -> Result<SyncPlan> {
    let active_project = load_active_project(current_dir, &root)?;
    prepare_sync_for_active_project(active_project, options)
}

/// Plans a sync from one already-resolved active project.
pub(crate) fn prepare_sync_for_active_project(
    active_project: ActiveProject,
    options: SyncOptions,
) -> Result<SyncPlan> {
    let crate::active_project::ActiveProject {
        mut config,
        config_path,
        current_root,
        current_live_paths: _current_live_paths,
        project_index,
        project,
    } = active_project;
    let configured_project = config
        .projects
        .get(project_index)
        .cloned()
        .with_context(|| format!("missing configured project index {project_index}"))?;
    let primary_project_path = normalize_project_path(&project.local_path);
    let full_project_paths = project_path_set(&current_root, &project.known_paths)?;
    let project_path_aliases = project_path_aliases(&configured_project, &full_project_paths);
    let previous_known_paths = normalized_known_paths(&project.local_path, &project.known_paths);
    let other_project_paths = other_project_paths(&config.projects, project_index)?;
    let other_project_path_aliases =
        other_project_path_aliases(&config.projects, project_index, &other_project_paths);
    let sources = selected_sources(&config, &options.provider_filter)?;
    let (project_upstreams, project_path_upstreams) =
        codex_remote_evidence(&project, &current_root, &config.projects, &sources);
    let sync_plan = darc_sync::prepare_sync(SyncRequest {
        project_name: project.name.clone(),
        project_root: current_root.clone(),
        sessions_root: project.sessions_root.clone(),
        primary_project_path,
        stored_known_paths: previous_known_paths.clone(),
        project_paths: full_project_paths,
        project_path_aliases,
        other_project_paths,
        other_project_path_aliases,
        project_upstreams,
        project_path_upstreams,
        sources: sources.clone(),
        claude: config.sources.claude.as_ref().map(|source| ClaudeSource {
            include_subagents: source.include_subagents,
            projects_root: source.projects_root.clone(),
        }),
        codex: config.sources.codex.as_ref().map(|source| CodexSource {
            home: source.home.clone(),
            sessions_root: source.sessions_root.clone(),
        }),
    })?;

    let config = if sync_plan.new_known_paths.is_empty()
        && previous_known_paths
            .iter()
            .eq(config.projects[project_index].known_paths.iter())
    {
        None
    } else {
        config.projects[project_index].known_paths = sync_plan.persisted_known_paths().to_vec();
        Some(config)
    };

    Ok(SyncPlan {
        project_name: sync_plan.project_name.clone(),
        project_root: sync_plan.project_root.clone(),
        sessions_root: sync_plan.sessions_root.clone(),
        sources,
        sessions_unchanged: sync_plan.sessions_unchanged,
        auxiliary_unchanged: sync_plan.auxiliary_unchanged,
        new_known_paths: sync_plan.new_known_paths.clone(),
        warnings: sync_plan.warnings.clone(),
        writes: PreparedSyncWrites {
            engine_plan: sync_plan,
            config_path,
            config,
        },
    })
}

/// Returns remote evidence used only for matching Codex rollout metadata.
fn codex_remote_evidence(
    project: &ProjectConfig,
    current_root: &Path,
    projects: &[ProjectConfig],
    sources: &[SourceKind],
) -> (BTreeSet<String>, BTreeMap<PathBuf, BTreeSet<String>>) {
    if !sources.contains(&SourceKind::Codex) {
        return (BTreeSet::new(), BTreeMap::new());
    }
    let live_project_upstream =
        try_git_output(current_root, &["config", "--get", "remote.origin.url"]);
    (
        project_upstreams(project, live_project_upstream),
        project_path_upstreams(projects),
    )
}

/// Returns all known remote spellings for the active project.
fn project_upstreams(project: &ProjectConfig, live_upstream: Option<String>) -> BTreeSet<String> {
    live_upstream
        .into_iter()
        .chain(project.git_upstream.clone())
        .collect()
}

/// Returns configured remote spellings scoped to their configured project paths.
fn project_path_upstreams(projects: &[ProjectConfig]) -> BTreeMap<PathBuf, BTreeSet<String>> {
    let mut upstreams = BTreeMap::new();
    for project in projects {
        insert_project_path_upstreams(&mut upstreams, project, &project.local_path, true);
        for path in &project.known_paths {
            insert_project_path_upstreams(&mut upstreams, project, path, false);
        }
    }
    upstreams
}

/// Adds one configured path and its non-probing aliases to a remote evidence map.
fn insert_project_path_upstreams(
    upstreams: &mut BTreeMap<PathBuf, BTreeSet<String>>,
    project: &ProjectConfig,
    path: &Path,
    include_config_upstream: bool,
) {
    let project_upstreams = project_path_remote_evidence(project, path, include_config_upstream);
    if project_upstreams.is_empty() {
        return;
    }
    for alias in project_path_text_aliases(path) {
        upstreams
            .entry(alias)
            .or_default()
            .extend(project_upstreams.iter().cloned());
    }
}

/// Returns remote spellings known from one configured path and its project record.
fn project_path_remote_evidence(
    project: &ProjectConfig,
    path: &Path,
    include_config_upstream: bool,
) -> BTreeSet<String> {
    let config_upstream = include_config_upstream
        .then(|| project.git_upstream.clone())
        .flatten();
    try_git_output(path, &["config", "--get", "remote.origin.url"])
        .into_iter()
        .chain(config_upstream)
        .collect()
}

/// Returns all paths owned by projects other than the active project, including live worktrees.
fn other_project_paths(
    projects: &[ProjectConfig],
    active_index: usize,
) -> Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::new();
    for (index, project) in projects.iter().enumerate() {
        if index == active_index {
            continue;
        }
        let root = normalize_project_path(&project.local_path);
        paths.extend(project_path_set(&root, &project.known_paths)?);
    }
    Ok(paths)
}

/// Returns project path spellings from registered paths and discovered live paths.
fn project_path_aliases(
    project: &ProjectConfig,
    live_paths: &BTreeSet<PathBuf>,
) -> BTreeSet<PathBuf> {
    let mut aliases = registered_project_path_aliases(project);
    aliases.extend(project_path_set_text_aliases(live_paths));
    aliases
}

/// Returns text spellings for one project's configured paths.
fn registered_project_path_aliases(project: &ProjectConfig) -> BTreeSet<PathBuf> {
    std::iter::once(&project.local_path)
        .chain(project.known_paths.iter())
        .flat_map(|path| project_path_text_aliases(path).into_iter())
        .collect()
}

/// Returns path spellings for every inactive project and its discovered live paths.
fn other_project_path_aliases(
    projects: &[ProjectConfig],
    active_index: usize,
    live_paths: &BTreeSet<PathBuf>,
) -> BTreeSet<PathBuf> {
    let mut aliases = projects
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != active_index)
        .flat_map(|(_, project)| registered_project_path_aliases(project))
        .collect::<BTreeSet<_>>();
    aliases.extend(project_path_set_text_aliases(live_paths));
    aliases
}

/// Resolves the enabled source list after applying any CLI filter.
fn selected_sources(config: &SharedConfig, filter: &[SourceKind]) -> Result<Vec<SourceKind>> {
    let mut sources = Vec::new();

    if config
        .sources
        .claude
        .as_ref()
        .is_some_and(|source| source.enabled)
        && (filter.is_empty() || filter.contains(&SourceKind::Claude))
    {
        sources.push(SourceKind::Claude);
    }
    if config
        .sources
        .codex
        .as_ref()
        .is_some_and(|source| source.enabled)
        && (filter.is_empty() || filter.contains(&SourceKind::Codex))
    {
        sources.push(SourceKind::Codex);
    }

    if sources.is_empty() {
        if filter.is_empty() {
            bail!("no enabled rollout sources are configured");
        }
        let wanted = filter
            .iter()
            .copied()
            .map(SourceKind::title)
            .collect::<Vec<_>>()
            .join(", ");
        bail!("no enabled rollout sources matched the requested filter: {wanted}");
    }

    Ok(sources)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use std::collections::BTreeSet;
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use darc_test_utils::{init_git_repo, unique_test_dir, write_file};

    use super::*;
    use crate::{
        config::{
            ClaudeSourceConfig, CodexSourceConfig, ProjectConfig, SharedConfig, SourcesConfig,
            load_config,
        },
        constants::CONFIG_FILE_NAME,
    };

    fn sample_config(
        root: &Path,
        project_root: &Path,
        claude_home: &Path,
        claude_projects: &Path,
        codex_home: &Path,
        codex_sessions_root: &Path,
    ) -> SharedConfig {
        SharedConfig::new(
            root.to_path_buf(),
            vec![ProjectConfig {
                id: "darc-abc123".into(),
                name: "darc".into(),
                local_path: project_root.to_path_buf(),
                git_upstream: None,
                sessions_root: root.join("projects/darc-abc123/sessions"),
                known_paths: Vec::new(),
            }],
            SourcesConfig {
                claude: Some(ClaudeSourceConfig {
                    enabled: true,
                    home: claude_home.to_path_buf(),
                    include_subagents: true,
                    projects_root: claude_projects.to_path_buf(),
                }),
                codex: Some(CodexSourceConfig {
                    enabled: true,
                    home: codex_home.to_path_buf(),
                    sessions_root: codex_sessions_root.to_path_buf(),
                }),
            },
        )
    }

    /// Shared filesystem fixture for adapter tests that exercise config persistence.
    struct TestWorkspace {
        root: PathBuf,
        project_root: PathBuf,
        darc_root: PathBuf,
        claude_home: PathBuf,
        claude_projects: PathBuf,
        codex_home: PathBuf,
        codex_sessions_root: PathBuf,
        canonical_project_root: PathBuf,
    }

    impl TestWorkspace {
        fn new(prefix: &str) -> Result<Self> {
            let root = unique_test_dir(prefix);
            let project_root = root.join("repo");
            let darc_root = root.join("darc");
            let claude_home = root.join("claude");
            let claude_projects = claude_home.join("projects");
            let codex_home = root.join("codex");
            let codex_sessions_root = codex_home.join("sessions");
            fs::create_dir_all(&project_root)?;
            fs::create_dir_all(&claude_projects)?;
            fs::create_dir_all(&codex_sessions_root)?;
            let canonical_project_root = fs::canonicalize(&project_root)?;
            Ok(Self {
                root,
                project_root,
                darc_root,
                claude_home,
                claude_projects,
                codex_home,
                codex_sessions_root,
                canonical_project_root,
            })
        }

        fn default_config(&self) -> SharedConfig {
            sample_config(
                &self.darc_root,
                &self.project_root,
                &self.claude_home,
                &self.claude_projects,
                &self.codex_home,
                &self.codex_sessions_root,
            )
        }

        fn write_config(&self, config: &SharedConfig) -> Result<()> {
            fs::create_dir_all(&self.darc_root)?;
            write_file(
                &self.darc_root.join(CONFIG_FILE_NAME),
                &toml::to_string_pretty(config)?,
            )
        }
    }

    #[test]
    fn prepare_sync_rewrites_known_paths_without_primary_root() -> Result<()> {
        let ws = TestWorkspace::new("sync-known-path-cleanup")?;
        let mut config = ws.default_config();
        config.projects[0].known_paths = vec![ws.canonical_project_root.clone()];
        ws.write_config(&config)?;

        let plan = prepare_sync_from(
            &ws.project_root,
            ws.darc_root.clone(),
            SyncOptions::default(),
        )?;

        assert!(plan.new_known_paths.is_empty());
        assert!(plan.config_written());

        let report = execute_sync(plan)?;

        assert!(report.new_known_paths.is_empty());
        assert!(report.config_written);

        let config_after = load_config(&ws.darc_root.join(CONFIG_FILE_NAME))?;
        assert!(config_after.projects[0].known_paths.is_empty());

        Ok(())
    }

    #[test]
    fn codex_remote_evidence_is_empty_for_claude_only_sources() -> Result<()> {
        let ws = TestWorkspace::new("sync-claude-only-remote-evidence")?;
        let mut config = ws.default_config();
        config.projects[0].git_upstream = Some("https://example.com/acme/darc.git".into());
        config.projects[0].known_paths = vec![ws.root.join("Desktop").join("repo-copy")];

        let (project_upstreams, project_path_upstreams) = codex_remote_evidence(
            &config.projects[0],
            &ws.project_root,
            &config.projects,
            &[SourceKind::Claude],
        );

        assert!(project_upstreams.is_empty());
        assert!(project_path_upstreams.is_empty());

        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn project_path_aliases_include_live_path_text_aliases() {
        let project = ProjectConfig {
            id: "darc-abc123".into(),
            name: "darc".into(),
            local_path: PathBuf::from("/Users/example/repo"),
            git_upstream: None,
            sessions_root: PathBuf::from("/tmp/darc/sessions"),
            known_paths: Vec::new(),
        };
        let live_paths = BTreeSet::from([PathBuf::from("/private/var/folders/example/repo-wt")]);

        let aliases = project_path_aliases(&project, &live_paths);

        assert!(aliases.contains(Path::new("/private/var/folders/example/repo-wt")));
        assert!(aliases.contains(Path::new("/var/folders/example/repo-wt")));
    }

    #[test]
    fn execute_sync_matches_logged_upstream_without_learning_known_paths() -> Result<()> {
        let ws = TestWorkspace::new("sync-codex-logged-upstream-adapter")?;
        let remote = "https://example.com/acme/darc.git";
        let related_subdir = ws.root.join("repo-b").join("nested");
        let codex_sessions = ws.codex_sessions_root.join("2026/04/01");
        fs::create_dir_all(&codex_sessions)?;
        init_git_repo(&ws.project_root, remote)?;

        let mut config = ws.default_config();
        config.projects[0].git_upstream = Some(remote.into());
        ws.write_config(&config)?;

        let rollout_name = "rollout-2026-04-01T10-00-00-22222222-2222-4222-8222-22222222223f.jsonl";
        write_file(
            &codex_sessions.join(rollout_name),
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"22222222-2222-4222-8222-22222222223f\",\"cwd\":\"{}\",\"cli_version\":\"0.128.0\",\"git\":{{\"repository_url\":\"{remote}\"}}}}}}\n{{\"type\":\"message\"}}\n",
                related_subdir.display(),
            ),
        )?;

        let plan = prepare_sync_from(
            &ws.project_root,
            ws.darc_root.clone(),
            SyncOptions::default(),
        )?;

        assert_eq!(plan.project_root, ws.canonical_project_root);
        assert_eq!(plan.sessions_to_copy(), 1);
        assert!(plan.new_known_paths.is_empty());
        assert!(!plan.config_written());

        let report = execute_sync(plan)?;

        assert_eq!(report.sessions_copied, 1);
        assert!(report.new_known_paths.is_empty());
        assert!(!report.config_written);

        let config_after = load_config(&ws.darc_root.join(CONFIG_FILE_NAME))?;
        assert!(config_after.projects[0].known_paths.is_empty());

        Ok(())
    }
}
