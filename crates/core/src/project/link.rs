use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use darc_paths::{
    current_project_root, normalize_project_path, normalized_known_paths, project_path_set,
    seed_known_paths, try_git_output,
};

use super::registry::{
    find_unique_project_index_by_name, load_normalized_shared_config, sort_projects,
};
use crate::{
    config::{ProjectConfig, SharedConfig},
    constants::CONFIG_FILE_NAME,
    init::project_id_from_path,
};

/// Stores the config updates needed before linking or renaming one project.
#[derive(Debug, Clone)]
pub(super) struct PreparedLink {
    pub(super) config_path: PathBuf,
    pub(super) config: SharedConfig,
    pub(super) current_root: PathBuf,
    pub(super) target_project: ProjectConfig,
    pub(super) source_project: ProjectConfig,
    pub(super) new_known_paths: Vec<PathBuf>,
    pub(super) total_known_paths: usize,
    pub(super) config_written: bool,
}

/// Prepares the config changes that link one source project into the current checkout target.
pub(super) fn prepare_link(
    current_dir: &Path,
    root: &Path,
    source_name: &str,
) -> Result<PreparedLink> {
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
