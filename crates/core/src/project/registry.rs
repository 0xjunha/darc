use std::{fs, path::Path};

use anyhow::{Context, Result, bail};

use crate::{
    config::{ProjectConfig, SharedConfig, load_config},
    constants::CONFIG_FILE_NAME,
    init::normalize_project_config,
};

/// Loads the shared config and normalizes legacy project entries in memory.
pub(crate) fn load_normalized_shared_config(config_path: &Path) -> Result<SharedConfig> {
    let mut config = load_config(config_path)?;
    config.projects = config
        .projects
        .into_iter()
        .map(normalize_project_config)
        .collect::<Result<Vec<_>>>()?;
    Ok(config)
}

/// Loads the registered project list from the shared config.
pub(crate) fn registered_projects(root: &Path) -> Result<Vec<ProjectConfig>> {
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
pub(super) fn sort_projects(projects: &mut [ProjectConfig]) {
    projects.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.local_path.cmp(&right.local_path))
    });
}

/// Finds exactly one project by display name or fails with a specific error.
pub(super) fn find_unique_project_index_by_name(
    projects: &[ProjectConfig],
    name: &str,
) -> Result<usize> {
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
pub(super) fn find_project_index_by_id(
    projects: &[ProjectConfig],
    project_id: &str,
) -> Result<usize> {
    projects
        .iter()
        .position(|project| project.id == project_id)
        .with_context(|| format!("project id `{project_id}` was not found in the shared config"))
}
