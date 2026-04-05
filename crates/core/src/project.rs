use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use darc_index::{INDEX_DB_FILE_NAME, open_index_database};
use darc_paths::{
    current_project_root, normalize_project_path, normalized_known_paths, project_path_set,
    seed_known_paths, try_git_output,
};
use rusqlite::params;

use crate::{
    config::{ProjectConfig, SharedConfig, SourceKind, load_config},
    constants::CONFIG_FILE_NAME,
    default_root_path,
    index::{IndexReport, index_project_sessions_from, selected_index_providers},
    init::{normalize_project_config, project_id_from_path},
    sync::{SyncOptions, SyncReport, execute_sync, prepare_sync_from},
};

/// Reports one completed config-only project link operation.
#[derive(Debug, Clone)]
pub struct LinkReport {
    pub target_project_name: String,
    pub target_project_id: String,
    pub target_project_root: PathBuf,
    pub source_project_name: String,
    pub source_project_id: String,
    pub new_known_paths: Vec<PathBuf>,
    pub total_known_paths: usize,
    pub config_written: bool,
}

/// Reports one completed destructive project removal.
#[derive(Debug, Clone)]
pub struct RemoveReport {
    pub project_name: String,
    pub project_id: String,
    pub sessions_root: PathBuf,
    pub archive_deleted: bool,
    pub indexed_sessions_removed: usize,
    pub indexed_turns_removed: usize,
    pub config_written: bool,
}

/// Collects optional provider filters for the refresh workflow.
#[derive(Debug, Clone, Default)]
pub struct RefreshOptions {
    pub provider_filter: Vec<SourceKind>,
}

/// Reports one completed refresh workflow across sync and index.
#[derive(Debug, Clone)]
pub struct RefreshReport {
    pub sync: SyncReport,
    pub index: IndexReport,
}

/// Reports one completed multi-project refresh workflow.
#[derive(Debug, Clone)]
pub struct RefreshAllReport {
    pub projects: Vec<RefreshReport>,
}

/// Reports one completed rename workflow across config, archive sync, indexing, and cleanup.
#[derive(Debug, Clone)]
pub struct RenameReport {
    pub link: LinkReport,
    pub sync: SyncReport,
    pub index: IndexReport,
    pub remove: RemoveReport,
}

/// Stores the config updates needed before linking or renaming one project.
#[derive(Debug, Clone)]
struct PreparedLink {
    config_path: PathBuf,
    config: SharedConfig,
    current_root: PathBuf,
    target_project: ProjectConfig,
    source_project: ProjectConfig,
    new_known_paths: Vec<PathBuf>,
    total_known_paths: usize,
    config_written: bool,
}

/// Links one named project's historical paths into the active project.
pub fn link_project(root: Option<PathBuf>, source_name: &str) -> Result<LinkReport> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    link_project_from(
        &current_dir,
        root.unwrap_or_else(default_root_path),
        source_name,
    )
}

/// Removes one named project from config, archive storage, and SQLite.
pub fn remove_project(root: Option<PathBuf>, project_name: &str) -> Result<RemoveReport> {
    remove_project_named(&root.unwrap_or_else(default_root_path), project_name)
}

/// Renames one historical project into the active project by rebuilding under the active id.
pub fn rename_project(root: Option<PathBuf>, source_name: &str) -> Result<RenameReport> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    rename_project_from(
        &current_dir,
        root.unwrap_or_else(default_root_path),
        source_name,
    )
}

/// Refreshes one active project by running sync and then index.
pub fn refresh_project(root: Option<PathBuf>, options: RefreshOptions) -> Result<RefreshReport> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    refresh_project_from(
        &current_dir,
        root.unwrap_or_else(default_root_path),
        &options,
    )
}

/// Refreshes every registered project by running sync and then index for each one.
pub fn refresh_all_projects(
    root: Option<PathBuf>,
    options: RefreshOptions,
) -> Result<RefreshAllReport> {
    let root = root.unwrap_or_else(default_root_path);
    let projects = registered_projects(&root)?;
    if projects.is_empty() {
        bail!("no configured darc projects found under {}", root.display());
    }

    let mut reports = Vec::with_capacity(projects.len());
    for project in projects {
        reports.push(
            refresh_project_from(&project.local_path, root.clone(), &options)
                .with_context(|| format!("failed to refresh project `{}`", project.name))?,
        );
    }

    Ok(RefreshAllReport { projects: reports })
}

/// Links one named project's historical paths into one explicit active project.
pub(crate) fn link_project_from(
    current_dir: &Path,
    root: PathBuf,
    source_name: &str,
) -> Result<LinkReport> {
    let prepared = prepare_link(current_dir, &root, source_name)?;
    if prepared.config_written {
        write_shared_config(&prepared.config_path, &prepared.config)?;
    }

    Ok(LinkReport {
        target_project_name: prepared.target_project.name,
        target_project_id: prepared.target_project.id,
        target_project_root: prepared.current_root,
        source_project_name: prepared.source_project.name,
        source_project_id: prepared.source_project.id,
        total_known_paths: prepared.total_known_paths,
        new_known_paths: prepared.new_known_paths,
        config_written: prepared.config_written,
    })
}

/// Runs the full rename workflow from one explicit active project directory.
pub(crate) fn rename_project_from(
    current_dir: &Path,
    root: PathBuf,
    source_name: &str,
) -> Result<RenameReport> {
    let prepared = prepare_link(current_dir, &root, source_name)?;
    if prepared.config_written {
        write_shared_config(&prepared.config_path, &prepared.config)?;
    }

    let link = LinkReport {
        target_project_name: prepared.target_project.name.clone(),
        target_project_id: prepared.target_project.id.clone(),
        target_project_root: prepared.current_root.clone(),
        source_project_name: prepared.source_project.name.clone(),
        source_project_id: prepared.source_project.id.clone(),
        total_known_paths: prepared.total_known_paths,
        new_known_paths: prepared.new_known_paths.clone(),
        config_written: prepared.config_written,
    };
    let refresh = refresh_project_from(current_dir, root.clone(), &RefreshOptions::default())?;
    let remove = remove_project_by_id(&root, &link.source_project_id)?;

    Ok(RenameReport {
        link,
        sync: refresh.sync,
        index: refresh.index,
        remove,
    })
}

/// Runs sync and index for one explicit active project directory.
pub(crate) fn refresh_project_from(
    current_dir: &Path,
    root: PathBuf,
    options: &RefreshOptions,
) -> Result<RefreshReport> {
    let sync = execute_sync(prepare_sync_from(
        current_dir,
        root.clone(),
        SyncOptions {
            provider_filter: options.provider_filter.clone(),
        },
    )?)?;
    let index = index_project_sessions_from(
        current_dir,
        root,
        &selected_index_providers(&options.provider_filter),
    )?;

    Ok(RefreshReport { sync, index })
}

/// Prepares the config changes that link one source project into the current checkout target.
fn prepare_link(current_dir: &Path, root: &Path, source_name: &str) -> Result<PreparedLink> {
    let config_path = root.join(CONFIG_FILE_NAME);
    if !config_path.exists() {
        bail!(
            "shared config not found at {}\nrun `darc init --root {}` from your project root first",
            config_path.display(),
            root.display()
        );
    }

    let mut config = load_normalized_shared_config(&config_path)?;
    let source_index = find_unique_project_index_by_name(&config.projects, source_name)?;
    let source_project = config
        .projects
        .get(source_index)
        .cloned()
        .with_context(|| format!("missing source project index {source_index}"))?;
    let current_root = current_project_root(current_dir)?;
    if normalize_project_path(&source_project.local_path) == current_root {
        bail!(
            "current directory still matches project `{source_name}`\nrun this command from the renamed project root"
        );
    }

    let target_index =
        find_target_project_index(&config.projects, &source_project.id, &current_root)?;
    let mut target_project = target_index
        .and_then(|index| config.projects.get(index).cloned())
        .unwrap_or(build_project_config(root, current_root.clone())?);
    let target_owned_paths =
        project_path_set(&target_project.local_path, &target_project.known_paths)?;
    let previous_known_paths =
        normalized_known_paths(&target_project.local_path, &target_project.known_paths);
    let merged_known_paths = linked_known_paths(&target_project, &source_project);
    let new_known_paths = merged_known_paths
        .difference(&previous_known_paths)
        .cloned()
        .collect::<Vec<_>>();
    target_project.known_paths = merged_known_paths.iter().cloned().collect();

    if let Some(index) = target_index {
        config.projects[index] = target_project.clone();
    } else {
        config.projects.push(target_project.clone());
    }

    let source_known_paths =
        normalized_known_paths(&source_project.local_path, &source_project.known_paths);
    let trimmed_source_known_paths = source_known_paths
        .difference(&target_owned_paths)
        .cloned()
        .collect::<BTreeSet<_>>();
    let refreshed_source = config
        .projects
        .get_mut(source_index)
        .with_context(|| format!("missing source project index {source_index}"))?;
    refreshed_source.known_paths = trimmed_source_known_paths.iter().cloned().collect();
    sort_projects(&mut config.projects);

    let config_written = target_index.is_none()
        || merged_known_paths != previous_known_paths
        || trimmed_source_known_paths != source_known_paths;

    Ok(PreparedLink {
        config_path,
        config,
        current_root,
        target_project,
        source_project,
        new_known_paths,
        total_known_paths: merged_known_paths.len(),
        config_written,
    })
}

/// Loads the shared config and normalizes legacy project entries in memory.
fn load_normalized_shared_config(config_path: &Path) -> Result<SharedConfig> {
    let mut config = load_config(config_path)?;
    config.projects = config
        .projects
        .into_iter()
        .map(normalize_project_config)
        .collect::<Result<Vec<_>>>()?;
    Ok(config)
}

/// Loads the registered project list from the shared config.
fn registered_projects(root: &Path) -> Result<Vec<ProjectConfig>> {
    let config_path = root.join(CONFIG_FILE_NAME);
    if !config_path.exists() {
        bail!(
            "shared config not found at {}\nrun `darc init --root {}` from a project root first",
            config_path.display(),
            root.display()
        );
    }

    Ok(load_normalized_shared_config(&config_path)?.projects)
}

/// Writes one full shared config back to disk.
pub(crate) fn write_shared_config(config_path: &Path, config: &SharedConfig) -> Result<()> {
    let content =
        toml::to_string_pretty(config).context("failed to serialize updated shared config")?;
    fs::write(config_path, content.as_bytes())
        .with_context(|| format!("failed to write {}", config_path.display()))
}

/// Sorts project entries by display name and local path before persistence.
fn sort_projects(projects: &mut [ProjectConfig]) {
    projects.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.local_path.cmp(&right.local_path))
    });
}

/// Finds the unique target project for the current checkout, excluding the source project.
fn find_target_project_index(
    projects: &[ProjectConfig],
    source_project_id: &str,
    current_root: &Path,
) -> Result<Option<usize>> {
    let target_id = project_id_from_path(current_root)?;
    let matches = projects
        .iter()
        .enumerate()
        .filter(|(_, project)| {
            project.id != source_project_id
                && (project.id == target_id
                    || normalize_project_path(&project.local_path) == current_root)
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [] => Ok(None),
        [index] => Ok(Some(*index)),
        _ => {
            let details = matches
                .iter()
                .map(|index| {
                    let project = &projects[*index];
                    format!("{} ({})", project.name, project.local_path.display())
                })
                .collect::<Vec<_>>()
                .join(", ");
            bail!("current directory matched multiple configured target projects: {details}")
        }
    }
}

/// Finds exactly one project by display name or fails with a specific error.
fn find_unique_project_index_by_name(projects: &[ProjectConfig], name: &str) -> Result<usize> {
    let matches = projects
        .iter()
        .enumerate()
        .filter(|(_, project)| project.name == name)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [] => bail!("project `{name}` was not found in the shared config"),
        [index] => Ok(*index),
        _ => {
            let details = matches
                .iter()
                .map(|index| {
                    let project = &projects[*index];
                    format!("{} ({})", project.id, project.local_path.display())
                })
                .collect::<Vec<_>>()
                .join(", ");
            bail!("project `{name}` is ambiguous: {details}")
        }
    }
}

/// Finds one project by stable id inside the normalized shared config.
fn find_project_index_by_id(projects: &[ProjectConfig], project_id: &str) -> Result<usize> {
    projects
        .iter()
        .position(|project| project.id == project_id)
        .with_context(|| format!("project id `{project_id}` was not found in the shared config"))
}

/// Merges the source project's historical path evidence into the target project's known paths.
fn linked_known_paths(
    target_project: &ProjectConfig,
    source_project: &ProjectConfig,
) -> BTreeSet<PathBuf> {
    let mut linked_paths =
        normalized_known_paths(&target_project.local_path, &target_project.known_paths);
    linked_paths.insert(normalize_project_path(&source_project.local_path));
    linked_paths.extend(normalized_known_paths(
        &source_project.local_path,
        &source_project.known_paths,
    ));
    linked_paths
}

/// Builds one project config for the current checkout path.
fn build_project_config(root: &Path, local_path: PathBuf) -> Result<ProjectConfig> {
    let name = local_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .with_context(|| {
            format!(
                "unable to determine project name from {}",
                local_path.display()
            )
        })?;
    let id = project_id_from_path(&local_path)?;
    let git_upstream = try_git_output(&local_path, &["config", "--get", "remote.origin.url"]);

    Ok(ProjectConfig {
        id: id.clone(),
        name,
        local_path: local_path.clone(),
        git_upstream,
        sessions_root: root.join("projects").join(&id).join("sessions"),
        known_paths: seed_known_paths(&local_path)?,
    })
}

/// Removes one named project from config, archive storage, and SQLite.
fn remove_project_named(root: &Path, project_name: &str) -> Result<RemoveReport> {
    let config_path = root.join(CONFIG_FILE_NAME);
    if !config_path.exists() {
        bail!(
            "shared config not found at {}\nrun `darc init --root {}` from your project root first",
            config_path.display(),
            root.display()
        );
    }

    let config = load_normalized_shared_config(&config_path)?;
    let project_index = find_unique_project_index_by_name(&config.projects, project_name)?;
    let project_id = config.projects[project_index].id.clone();
    remove_project_by_id(root, &project_id)
}

/// Removes one project by stable id from config, archive storage, and SQLite.
fn remove_project_by_id(root: &Path, project_id: &str) -> Result<RemoveReport> {
    let config_path = root.join(CONFIG_FILE_NAME);
    if !config_path.exists() {
        bail!(
            "shared config not found at {}\nrun `darc init --root {}` from your project root first",
            config_path.display(),
            root.display()
        );
    }

    let mut config = load_normalized_shared_config(&config_path)?;
    let project_index = find_project_index_by_id(&config.projects, project_id)?;
    let project = config.projects.remove(project_index);
    let (indexed_sessions_removed, indexed_turns_removed) =
        delete_project_index_rows(&root.join(INDEX_DB_FILE_NAME), &project.id)?;
    let archive_deleted = delete_project_archive(&project.sessions_root)?;
    write_shared_config(&config_path, &config)?;

    Ok(RemoveReport {
        project_name: project.name,
        project_id: project.id,
        sessions_root: project.sessions_root,
        archive_deleted,
        indexed_sessions_removed,
        indexed_turns_removed,
        config_written: true,
    })
}

/// Deletes one archived project sessions directory when it exists.
fn delete_project_archive(sessions_root: &Path) -> Result<bool> {
    if !sessions_root.exists() {
        return Ok(false);
    }

    fs::remove_dir_all(sessions_root)
        .with_context(|| format!("failed to remove {}", sessions_root.display()))?;
    Ok(true)
}

/// Deletes one project's indexed SQLite rows and returns the removed row counts.
fn delete_project_index_rows(index_db_path: &Path, project_id: &str) -> Result<(usize, usize)> {
    if !index_db_path.exists() {
        return Ok((0, 0));
    }

    let mut connection = open_index_database(index_db_path)?;
    let transaction = connection
        .transaction()
        .context("failed to begin SQLite removal transaction")?;
    let indexed_sessions_removed =
        count_project_rows(&transaction, "sessions", project_id, "sessions")?;
    let indexed_turns_removed = count_project_rows(&transaction, "turns", project_id, "turns")?;
    transaction
        .execute(
            "DELETE FROM sessions WHERE project_id = ?1",
            params![project_id],
        )
        .with_context(|| format!("failed to delete indexed sessions for project `{project_id}`"))?;
    transaction
        .commit()
        .context("failed to commit SQLite project removal transaction")?;

    Ok((indexed_sessions_removed, indexed_turns_removed))
}

/// Counts one project's rows in a selected normalized SQLite table.
fn count_project_rows(
    connection: &rusqlite::Transaction<'_>,
    table_name: &str,
    project_id: &str,
    label: &str,
) -> Result<usize> {
    let count: i64 = connection
        .query_row(
            &format!("SELECT COUNT(*) FROM {table_name} WHERE project_id = ?1"),
            params![project_id],
            |row| row.get(0),
        )
        .with_context(|| format!("failed to count indexed {label} for project `{project_id}`"))?;
    usize::try_from(count).with_context(|| format!("indexed {label} count exceeds usize range"))
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use darc_index::open_index_database;

    use super::*;
    use crate::{
        active_project::load_active_project,
        config::{CodexSourceConfig, SourcesConfig},
    };

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Builds one unique temporary directory for project-management tests.
    fn unique_test_dir(label: &str) -> PathBuf {
        let suffix = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "darc-project-{label}-{}-{suffix}",
            std::process::id()
        ))
    }

    /// Writes one UTF-8 file after creating its parent directory.
    fn write_file(path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
        Ok(())
    }

    /// Stores one minimal shared config used by project-management tests.
    fn write_config(root: &Path, config: &SharedConfig) -> Result<()> {
        fs::create_dir_all(root)?;
        fs::write(root.join(CONFIG_FILE_NAME), toml::to_string_pretty(config)?)?;
        Ok(())
    }

    /// Writes one minimal live Codex rollout fixture for refresh tests.
    fn write_codex_rollout(
        sessions_root: &Path,
        rollout_name: &str,
        session_id: &str,
        cwd: &Path,
        user_message: &str,
        assistant_reply: &str,
    ) -> Result<()> {
        write_file(
            &sessions_root.join(format!("2026/04/01/{rollout_name}")),
            &format!(
                concat!(
                    "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"cwd\":\"{cwd}\",\"cli_version\":\"0.118.0\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"{user_message}\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{assistant_reply}\"}}]}}}}\n"
                ),
                cwd = cwd.display(),
                session_id = session_id,
                user_message = user_message,
                assistant_reply = assistant_reply,
            ),
        )
    }

    /// Creates one stable test timestamp seed to keep temp paths distinct.
    fn timestamp_seed() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn link_project_merges_source_paths_without_duplicates() -> Result<()> {
        let root = unique_test_dir(&format!("link-{}", timestamp_seed()));
        let target_root = root.join("darc");
        let target_worktree = root.join("darc-wt");
        let source_root = root.join("memstack");
        let source_worktree = root.join("memstack-wt");
        fs::create_dir_all(&target_root)?;
        fs::create_dir_all(&target_worktree)?;
        fs::create_dir_all(&source_root)?;
        fs::create_dir_all(&source_worktree)?;

        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![
                    ProjectConfig {
                        id: "darc-123".into(),
                        name: "darc".into(),
                        local_path: target_root.clone(),
                        git_upstream: None,
                        sessions_root: root.join("projects/darc-123/sessions"),
                        known_paths: vec![target_worktree.clone()],
                    },
                    ProjectConfig {
                        id: "memstack-456".into(),
                        name: "memstack".into(),
                        local_path: source_root.clone(),
                        git_upstream: None,
                        sessions_root: root.join("projects/memstack-456/sessions"),
                        known_paths: vec![source_worktree.clone(), target_worktree.clone()],
                    },
                ],
                SourcesConfig::default(),
            ),
        )?;

        let report = link_project_from(&target_root, root.clone(), "memstack")?;

        assert_eq!(report.target_project_name, "darc");
        assert_eq!(report.source_project_name, "memstack");
        assert_eq!(report.new_known_paths.len(), 2);
        assert_eq!(report.total_known_paths, 3);
        assert!(report.config_written);

        let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
        let target_project = config
            .projects
            .iter()
            .find(|project| project.name == "darc")
            .context("missing target project")?;
        let source_project = config
            .projects
            .iter()
            .find(|project| project.name == "memstack")
            .context("missing source project")?;
        assert_eq!(
            target_project.known_paths,
            vec![
                fs::canonicalize(&target_worktree)?,
                fs::canonicalize(&source_root)?,
                fs::canonicalize(&source_worktree)?,
            ]
        );
        assert_eq!(
            source_project.known_paths,
            vec![fs::canonicalize(&source_worktree)?]
        );
        let active_project = load_active_project(&target_worktree, &root)?;
        assert_eq!(active_project.project.name, "darc");

        Ok(())
    }

    #[test]
    fn link_project_cleans_source_overlap_when_target_already_knows_paths() -> Result<()> {
        let root = unique_test_dir(&format!("link-cleanup-{}", timestamp_seed()));
        let target_root = root.join("darc");
        let source_root = root.join("memstack");
        let source_worktree = root.join("memstack-wt");
        fs::create_dir_all(&target_root)?;
        fs::create_dir_all(&source_root)?;
        fs::create_dir_all(&source_worktree)?;

        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![
                    ProjectConfig {
                        id: "darc-123".into(),
                        name: "darc".into(),
                        local_path: target_root.clone(),
                        git_upstream: None,
                        sessions_root: root.join("projects/darc-123/sessions"),
                        known_paths: vec![source_root.clone(), source_worktree.clone()],
                    },
                    ProjectConfig {
                        id: "memstack-456".into(),
                        name: "memstack".into(),
                        local_path: source_root.clone(),
                        git_upstream: None,
                        sessions_root: root.join("projects/memstack-456/sessions"),
                        known_paths: vec![source_worktree.clone()],
                    },
                ],
                SourcesConfig::default(),
            ),
        )?;

        let report = link_project_from(&target_root, root.clone(), "memstack")?;

        assert!(report.config_written);
        assert!(report.new_known_paths.is_empty());
        assert_eq!(report.total_known_paths, 2);
        let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
        let source_project = config
            .projects
            .iter()
            .find(|project| project.name == "memstack")
            .context("missing source project")?;
        assert!(source_project.known_paths.is_empty());
        let active_project = load_active_project(&source_worktree, &root)?;
        assert_eq!(active_project.project.name, "darc");

        Ok(())
    }

    #[test]
    fn link_project_supports_old_only_config_after_directory_rename() -> Result<()> {
        let root = unique_test_dir(&format!("link-old-only-{}", timestamp_seed()));
        let target_root = root.join("darc");
        let source_root = root.join("memstack");
        let source_worktree = root.join("memstack-wt");
        fs::create_dir_all(&target_root)?;
        fs::create_dir_all(&source_root)?;
        fs::create_dir_all(&source_worktree)?;

        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![ProjectConfig {
                    id: "memstack-456".into(),
                    name: "memstack".into(),
                    local_path: source_root.clone(),
                    git_upstream: None,
                    sessions_root: root.join("projects/memstack-456/sessions"),
                    known_paths: vec![source_worktree.clone()],
                }],
                SourcesConfig::default(),
            ),
        )?;

        let report = link_project_from(&target_root, root.clone(), "memstack")?;

        assert_eq!(report.target_project_name, "darc");
        assert_eq!(report.source_project_name, "memstack");
        assert_eq!(report.total_known_paths, 2);
        assert!(report.config_written);

        let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
        assert_eq!(config.projects.len(), 2);
        let target_project = config
            .projects
            .iter()
            .find(|project| project.name == "darc")
            .context("missing target project")?;
        assert_eq!(
            target_project.known_paths,
            vec![
                fs::canonicalize(&source_root)?,
                fs::canonicalize(&source_worktree)?
            ]
        );

        let active_project = load_active_project(&target_root, &root)?;
        assert_eq!(active_project.project.name, "darc");

        Ok(())
    }

    #[test]
    fn remove_project_deletes_archive_and_index_rows() -> Result<()> {
        let root = unique_test_dir(&format!("remove-{}", timestamp_seed()));
        let target_root = root.join("darc");
        let source_root = root.join("memstack");
        let source_sessions = root.join("projects/memstack-456/sessions");
        let target_sessions = root.join("projects/darc-123/sessions");
        fs::create_dir_all(&target_root)?;
        fs::create_dir_all(&source_root)?;
        write_file(
            &source_sessions.join("codex/rollout.jsonl"),
            "{\"type\":\"session_meta\"}\n",
        )?;

        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![
                    ProjectConfig {
                        id: "darc-123".into(),
                        name: "darc".into(),
                        local_path: target_root,
                        git_upstream: None,
                        sessions_root: target_sessions,
                        known_paths: Vec::new(),
                    },
                    ProjectConfig {
                        id: "memstack-456".into(),
                        name: "memstack".into(),
                        local_path: source_root,
                        git_upstream: None,
                        sessions_root: source_sessions.clone(),
                        known_paths: Vec::new(),
                    },
                ],
                SourcesConfig::default(),
            ),
        )?;

        let index_db_path = root.join(INDEX_DB_FILE_NAME);
        let connection = open_index_database(&index_db_path)?;
        connection.execute(
            "INSERT INTO sessions (project_id, provider, session_id, parent_session_id, session_kind, archive_path, cwd) VALUES (?1, 'codex', 'source-session', NULL, 'primary', 'codex/rollout.jsonl', '/tmp/memstack')",
            params!["memstack-456"],
        )?;
        connection.execute(
            "INSERT INTO turns (project_id, provider, session_id, turn_ordinal, started_at, status, user_message, steps_json) VALUES (?1, 'codex', 'source-session', 0, '2026-04-01T10:00:00Z', 'completed', 'Inspect', '[]')",
            params!["memstack-456"],
        )?;
        connection.execute(
            "INSERT INTO sessions (project_id, provider, session_id, parent_session_id, session_kind, archive_path, cwd) VALUES (?1, 'codex', 'target-session', NULL, 'primary', 'codex/rollout.jsonl', '/tmp/darc')",
            params!["darc-123"],
        )?;

        let report = remove_project(Some(root.clone()), "memstack")?;

        assert_eq!(report.project_name, "memstack");
        assert_eq!(report.indexed_sessions_removed, 1);
        assert_eq!(report.indexed_turns_removed, 1);
        assert!(report.archive_deleted);
        assert!(report.config_written);
        assert!(!source_sessions.exists());

        let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
        assert_eq!(config.projects.len(), 1);
        assert_eq!(config.projects[0].name, "darc");

        let connection = open_index_database(&index_db_path)?;
        let remaining_source_sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE project_id = 'memstack-456'",
            [],
            |row| row.get(0),
        )?;
        let remaining_target_sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE project_id = 'darc-123'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(remaining_source_sessions, 0);
        assert_eq!(remaining_target_sessions, 1);

        Ok(())
    }

    #[test]
    fn remove_project_rejects_ambiguous_names() -> Result<()> {
        let root = unique_test_dir(&format!("remove-ambiguous-{}", timestamp_seed()));
        let left_root = root.join("repo-a");
        let right_root = root.join("repo-b");
        fs::create_dir_all(&left_root)?;
        fs::create_dir_all(&right_root)?;

        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![
                    ProjectConfig {
                        id: "same-111".into(),
                        name: "same".into(),
                        local_path: left_root,
                        git_upstream: None,
                        sessions_root: root.join("projects/same-111/sessions"),
                        known_paths: Vec::new(),
                    },
                    ProjectConfig {
                        id: "same-222".into(),
                        name: "same".into(),
                        local_path: right_root,
                        git_upstream: None,
                        sessions_root: root.join("projects/same-222/sessions"),
                        known_paths: Vec::new(),
                    },
                ],
                SourcesConfig::default(),
            ),
        )?;

        let error = remove_project(Some(root), "same").expect_err("expected ambiguity error");
        assert!(error.to_string().contains("project `same` is ambiguous"));

        Ok(())
    }

    #[test]
    fn remove_project_handles_missing_archive_and_index() -> Result<()> {
        let root = unique_test_dir(&format!("remove-empty-{}", timestamp_seed()));
        let project_root = root.join("repo");
        fs::create_dir_all(&project_root)?;

        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![ProjectConfig {
                    id: "repo-123".into(),
                    name: "repo".into(),
                    local_path: project_root,
                    git_upstream: None,
                    sessions_root: root.join("projects/repo-123/sessions"),
                    known_paths: Vec::new(),
                }],
                SourcesConfig::default(),
            ),
        )?;

        let report = remove_project(Some(root.clone()), "repo")?;

        assert_eq!(report.project_name, "repo");
        assert!(!report.archive_deleted);
        assert_eq!(report.indexed_sessions_removed, 0);
        assert_eq!(report.indexed_turns_removed, 0);
        let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
        assert!(config.projects.is_empty());

        Ok(())
    }

    #[test]
    fn refresh_project_syncs_and_indexes_active_project() -> Result<()> {
        let root = unique_test_dir(&format!("refresh-{}", timestamp_seed()));
        let project_root = root.join("repo");
        let codex_home = root.join(".codex");
        let codex_sessions_root = codex_home.join("sessions");
        let project_sessions_root = root.join("projects/repo-123/sessions");
        let rollout_name = "rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl";
        fs::create_dir_all(&project_root)?;
        write_codex_rollout(
            &codex_sessions_root,
            rollout_name,
            "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f",
            &project_root,
            "Refresh me",
            "Done",
        )?;

        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![ProjectConfig {
                    id: "repo-123".into(),
                    name: "repo".into(),
                    local_path: project_root.clone(),
                    git_upstream: None,
                    sessions_root: project_sessions_root.clone(),
                    known_paths: Vec::new(),
                }],
                SourcesConfig {
                    claude: None,
                    codex: Some(CodexSourceConfig {
                        enabled: true,
                        home: codex_home,
                        sessions_root: codex_sessions_root,
                    }),
                },
            ),
        )?;

        let report = refresh_project_from(&project_root, root.clone(), &RefreshOptions::default())?;

        assert_eq!(report.sync.project_name, "repo");
        assert_eq!(report.sync.sessions_copied, 1);
        assert_eq!(report.index.project_name, "repo");
        assert_eq!(report.index.sessions_currently_indexed, 1);
        assert!(
            project_sessions_root
                .join(format!("codex/{rollout_name}"))
                .exists()
        );

        let connection = open_index_database(&root.join(INDEX_DB_FILE_NAME))?;
        let indexed_sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE project_id = 'repo-123'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(indexed_sessions, 1);

        Ok(())
    }

    #[test]
    fn refresh_all_projects_refreshes_each_registered_project() -> Result<()> {
        let root = unique_test_dir(&format!("refresh-all-{}", timestamp_seed()));
        let left_root = root.join("repo-a");
        let right_root = root.join("repo-b");
        let codex_home = root.join(".codex");
        let codex_sessions_root = codex_home.join("sessions");
        fs::create_dir_all(&left_root)?;
        fs::create_dir_all(&right_root)?;
        write_codex_rollout(
            &codex_sessions_root,
            "rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl",
            "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f",
            &left_root,
            "Inspect repo-a",
            "Indexed repo-a",
        )?;
        write_codex_rollout(
            &codex_sessions_root,
            "rollout-2026-04-01T10-05-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e40.jsonl",
            "019d3415-0b9c-7dc3-88e0-e9cb7a789e40",
            &right_root,
            "Inspect repo-b",
            "Indexed repo-b",
        )?;

        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![
                    ProjectConfig {
                        id: "repo-a-123".into(),
                        name: "repo-a".into(),
                        local_path: left_root.clone(),
                        git_upstream: None,
                        sessions_root: root.join("projects/repo-a-123/sessions"),
                        known_paths: Vec::new(),
                    },
                    ProjectConfig {
                        id: "repo-b-456".into(),
                        name: "repo-b".into(),
                        local_path: right_root.clone(),
                        git_upstream: None,
                        sessions_root: root.join("projects/repo-b-456/sessions"),
                        known_paths: Vec::new(),
                    },
                ],
                SourcesConfig {
                    claude: None,
                    codex: Some(CodexSourceConfig {
                        enabled: true,
                        home: codex_home,
                        sessions_root: codex_sessions_root,
                    }),
                },
            ),
        )?;

        let report = refresh_all_projects(Some(root.clone()), RefreshOptions::default())?;

        assert_eq!(report.projects.len(), 2);
        assert!(
            report
                .projects
                .iter()
                .all(|project| project.sync.sessions_copied == 1)
        );
        assert!(
            report
                .projects
                .iter()
                .all(|project| project.index.sessions_currently_indexed == 1)
        );
        assert_eq!(
            report
                .projects
                .iter()
                .map(|project| project.sync.project_name.as_str())
                .collect::<Vec<_>>(),
            vec!["repo-a", "repo-b"]
        );

        let connection = open_index_database(&root.join(INDEX_DB_FILE_NAME))?;
        let repo_a_sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE project_id = 'repo-a-123'",
            [],
            |row| row.get(0),
        )?;
        let repo_b_sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE project_id = 'repo-b-456'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(repo_a_sessions, 1);
        assert_eq!(repo_b_sessions, 1);

        Ok(())
    }

    #[test]
    fn rename_project_links_syncs_indexes_and_removes_source() -> Result<()> {
        let root = unique_test_dir(&format!("rename-{}", timestamp_seed()));
        let target_root = root.join("darc");
        let source_root = root.join("memstack");
        let codex_home = root.join(".codex");
        let codex_sessions_root = codex_home.join("sessions");
        let source_sessions_root = root.join("projects/memstack-456/sessions");
        let target_sessions_root = root.join("projects/darc-123/sessions");
        let rollout_name = "rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl";
        fs::create_dir_all(&target_root)?;
        write_file(
            &codex_sessions_root.join(format!("2026/04/01/{rollout_name}")),
            &format!(
                concat!(
                    "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"First task\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"First reply\"}}]}}}}\n"
                ),
                source_root.display()
            ),
        )?;
        write_file(
            &source_sessions_root.join("codex/stale.jsonl"),
            "{\"type\":\"session_meta\"}\n",
        )?;

        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![
                    ProjectConfig {
                        id: "darc-123".into(),
                        name: "darc".into(),
                        local_path: target_root.clone(),
                        git_upstream: None,
                        sessions_root: target_sessions_root.clone(),
                        known_paths: Vec::new(),
                    },
                    ProjectConfig {
                        id: "memstack-456".into(),
                        name: "memstack".into(),
                        local_path: source_root.clone(),
                        git_upstream: None,
                        sessions_root: source_sessions_root.clone(),
                        known_paths: Vec::new(),
                    },
                ],
                SourcesConfig {
                    claude: None,
                    codex: Some(CodexSourceConfig {
                        enabled: true,
                        home: codex_home.clone(),
                        sessions_root: codex_sessions_root.clone(),
                    }),
                },
            ),
        )?;

        let index_db_path = root.join(INDEX_DB_FILE_NAME);
        let connection = open_index_database(&index_db_path)?;
        connection.execute(
            "INSERT INTO sessions (project_id, provider, session_id, parent_session_id, session_kind, archive_path, cwd) VALUES (?1, 'codex', 'stale-session', NULL, 'primary', 'codex/stale.jsonl', '/tmp/memstack')",
            params!["memstack-456"],
        )?;

        let report = rename_project_from(&target_root, root.clone(), "memstack")?;

        assert_eq!(report.link.source_project_name, "memstack");
        assert_eq!(report.sync.sessions_copied, 1);
        assert_eq!(report.index.project_name, "darc");
        assert_eq!(report.index.sessions_currently_indexed, 1);
        assert_eq!(report.remove.project_name, "memstack");
        assert!(
            target_sessions_root
                .join(format!("codex/{rollout_name}"))
                .exists()
        );
        assert!(!source_sessions_root.exists());

        let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
        assert_eq!(config.projects.len(), 1);
        assert_eq!(config.projects[0].name, "darc");
        assert_eq!(config.projects[0].known_paths, vec![source_root]);

        let connection = open_index_database(&index_db_path)?;
        let target_sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE project_id = 'darc-123'",
            [],
            |row| row.get(0),
        )?;
        let source_sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE project_id = 'memstack-456'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(target_sessions, 1);
        assert_eq!(source_sessions, 0);

        Ok(())
    }

    #[test]
    fn rename_project_supports_old_only_config_after_directory_rename() -> Result<()> {
        let root = unique_test_dir(&format!("rename-old-only-{}", timestamp_seed()));
        let target_root = root.join("darc");
        let source_root = root.join("memstack");
        let codex_home = root.join(".codex");
        let codex_sessions_root = codex_home.join("sessions");
        let source_sessions_root = root.join("projects/memstack-456/sessions");
        let rollout_name = "rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl";
        let shared_worktree = root.join("shared-worktree");
        fs::create_dir_all(&target_root)?;
        fs::create_dir_all(&shared_worktree)?;
        write_file(
            &codex_sessions_root.join(format!("2026/04/01/{rollout_name}")),
            &format!(
                concat!(
                    "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Rename me\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Linked\"}}]}}}}\n"
                ),
                source_root.display()
            ),
        )?;
        write_file(
            &source_sessions_root.join("codex/stale.jsonl"),
            "{\"type\":\"session_meta\"}\n",
        )?;

        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![ProjectConfig {
                    id: "memstack-456".into(),
                    name: "memstack".into(),
                    local_path: source_root.clone(),
                    git_upstream: None,
                    sessions_root: source_sessions_root.clone(),
                    known_paths: vec![shared_worktree.clone()],
                }],
                SourcesConfig {
                    claude: None,
                    codex: Some(CodexSourceConfig {
                        enabled: true,
                        home: codex_home.clone(),
                        sessions_root: codex_sessions_root.clone(),
                    }),
                },
            ),
        )?;

        let connection = open_index_database(&root.join(INDEX_DB_FILE_NAME))?;
        connection.execute(
            "INSERT INTO sessions (project_id, provider, session_id, parent_session_id, session_kind, archive_path, cwd) VALUES (?1, 'codex', 'stale-session', NULL, 'primary', 'codex/stale.jsonl', '/tmp/memstack')",
            params!["memstack-456"],
        )?;

        let report = rename_project_from(&target_root, root.clone(), "memstack")?;
        let target_sessions_root = root
            .join("projects")
            .join(&report.link.target_project_id)
            .join("sessions");

        assert_eq!(report.link.target_project_name, "darc");
        assert_eq!(report.link.source_project_name, "memstack");
        assert_eq!(report.index.project_name, "darc");
        assert_eq!(report.remove.project_name, "memstack");
        assert!(
            target_sessions_root
                .join(format!("codex/{rollout_name}"))
                .exists()
        );
        assert!(!source_sessions_root.exists());

        let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
        assert_eq!(config.projects.len(), 1);
        assert_eq!(config.projects[0].name, "darc");
        assert_eq!(config.projects[0].id, report.link.target_project_id);
        assert!(config.projects[0].known_paths.contains(&source_root));

        Ok(())
    }

    #[test]
    fn rename_project_keeps_source_when_sync_fails() -> Result<()> {
        let root = unique_test_dir(&format!("rename-sync-fail-{}", timestamp_seed()));
        let target_root = root.join("darc");
        let source_root = root.join("memstack");
        let source_sessions_root = root.join("projects/memstack-456/sessions");
        let broken_sessions_root = root.join("broken");
        let codex_home = root.join(".codex");
        let codex_sessions_root = codex_home.join("sessions");
        let rollout_name = "rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl";
        fs::create_dir_all(&target_root)?;
        fs::create_dir_all(&source_root)?;
        write_file(
            &codex_sessions_root.join(format!("2026/04/01/{rollout_name}")),
            &format!(
                concat!(
                    "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Copy me\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Done\"}}]}}}}\n"
                ),
                source_root.display()
            ),
        )?;
        write_file(
            &source_sessions_root.join("codex/previous.jsonl"),
            "{\"type\":\"session_meta\"}\n",
        )?;
        write_file(&broken_sessions_root, "not-a-directory")?;

        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![
                    ProjectConfig {
                        id: "darc-123".into(),
                        name: "darc".into(),
                        local_path: target_root.clone(),
                        git_upstream: None,
                        sessions_root: broken_sessions_root.clone(),
                        known_paths: Vec::new(),
                    },
                    ProjectConfig {
                        id: "memstack-456".into(),
                        name: "memstack".into(),
                        local_path: source_root.clone(),
                        git_upstream: None,
                        sessions_root: source_sessions_root.clone(),
                        known_paths: Vec::new(),
                    },
                ],
                SourcesConfig {
                    claude: None,
                    codex: Some(CodexSourceConfig {
                        enabled: true,
                        home: codex_home,
                        sessions_root: codex_sessions_root,
                    }),
                },
            ),
        )?;

        let error = rename_project_from(&target_root, root.clone(), "memstack")
            .expect_err("expected sync failure");
        assert!(error.to_string().contains("failed to create"));

        let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
        assert_eq!(config.projects.len(), 2);
        assert!(
            config
                .projects
                .iter()
                .any(|project| project.name == "memstack")
        );
        assert!(source_sessions_root.exists());

        Ok(())
    }
}
