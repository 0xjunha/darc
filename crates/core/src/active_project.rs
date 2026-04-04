use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::{
    config::{ProjectConfig, SharedConfig, load_config},
    constants::CONFIG_FILE_NAME,
    project_paths::{current_project_root, normalize_project_path, project_path_set},
};

/// Stores the resolved darc project for the current working directory.
#[derive(Debug, Clone)]
pub(crate) struct ActiveProject {
    pub config: SharedConfig,
    pub config_path: PathBuf,
    pub current_root: PathBuf,
    pub current_live_paths: BTreeSet<PathBuf>,
    pub project_index: usize,
    pub project: ProjectConfig,
}

/// Loads the active darc project from the shared config.
pub(crate) fn load_active_project(current_dir: &Path, root: &Path) -> Result<ActiveProject> {
    let config_path = root.join(CONFIG_FILE_NAME);
    if !config_path.exists() {
        bail!(
            "shared config not found at {}\nrun `darc init --root {}` from your project root first",
            config_path.display(),
            root.display()
        );
    }

    let config = load_config(&config_path)?;
    let current_root = current_project_root(current_dir)?;
    let current_live_paths = project_path_set(&current_root, &[])?;
    let project_index = find_project_index(&config.projects, &current_live_paths)?;
    let project = config
        .projects
        .get(project_index)
        .cloned()
        .with_context(|| format!("missing project index {project_index}"))?;

    Ok(ActiveProject {
        config,
        config_path,
        current_root,
        current_live_paths,
        project_index,
        project,
    })
}

/// Matches the current repo or worktree against configured projects.
fn find_project_index(
    projects: &[ProjectConfig],
    current_paths: &BTreeSet<PathBuf>,
) -> Result<usize> {
    let mut matches = Vec::new();

    for (index, project) in projects.iter().enumerate() {
        let project_paths = configured_project_paths(project);
        if !project_paths.is_disjoint(current_paths) {
            matches.push(index);
        }
    }

    match matches.as_slice() {
        [] => bail!("current directory does not match any configured darc project"),
        [index] => Ok(*index),
        _ => {
            let names = matches
                .into_iter()
                .map(|index| projects[index].name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("current directory matched multiple configured projects: {names}")
        }
    }
}

/// Returns the configured primary and known paths for one project.
fn configured_project_paths(project: &ProjectConfig) -> BTreeSet<PathBuf> {
    let mut project_paths = project
        .known_paths
        .iter()
        .map(|path| normalize_project_path(path))
        .collect::<BTreeSet<_>>();
    project_paths.insert(normalize_project_path(&project.local_path));
    project_paths
}
