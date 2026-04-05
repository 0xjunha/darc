use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use super::{project_config::normalize_project_config, types::DetectedRolloutSource};
use crate::config::{
    ClaudeSourceConfig, CodexSourceConfig, ProjectConfig, SharedConfig, SourcesConfig, load_config,
};

/// Merges the current project into the shared config model before serialization.
pub(super) fn build_config(
    existing_projects: Vec<ProjectConfig>,
    project: ProjectConfig,
    sources: &[DetectedRolloutSource],
    root: PathBuf,
) -> SharedConfig {
    let mut projects: Vec<_> = existing_projects
        .into_iter()
        .filter(|existing| existing.id != project.id)
        .collect();
    projects.push(project);
    projects.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.local_path.cmp(&right.local_path))
    });

    SharedConfig::new(
        root,
        projects,
        SourcesConfig {
            claude: sources
                .iter()
                .find(|source| source.kind == crate::SourceKind::Claude)
                .map(|source| ClaudeSourceConfig {
                    enabled: true,
                    home: source.home.clone(),
                    include_subagents: true,
                    projects_root: source.root.clone(),
                }),
            codex: sources
                .iter()
                .find(|source| source.kind == crate::SourceKind::Codex)
                .map(|source| CodexSourceConfig {
                    enabled: true,
                    home: source.home.clone(),
                    sessions_root: source.root.clone(),
                }),
        },
    )
}

/// Loads any existing project entries from the shared config file.
pub(super) fn load_existing_projects(config_path: &Path) -> Result<Vec<ProjectConfig>> {
    if !config_path.exists() {
        return Ok(Vec::new());
    }

    let config = load_config(config_path)?;
    config
        .projects
        .into_iter()
        .map(normalize_project_config)
        .collect()
}

/// Creates the parent directory for a target file path when needed.
pub(super) fn create_parent(path: &Path, label: &str) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{label} is missing a parent directory"))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    Ok(())
}
