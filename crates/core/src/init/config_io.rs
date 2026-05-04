use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use super::{project_config::normalize_project_config, types::DetectedRolloutSource};
use crate::config::{
    ClaudeSourceConfig, CodexSourceConfig, ProjectConfig, SharedConfig, SourcesConfig, load_config,
};

/// Stores existing shared config fields that init must preserve.
#[derive(Default)]
pub(super) struct ExistingConfig {
    pub(super) projects: Vec<ProjectConfig>,
    pub(super) check_for_update_on_startup: bool,
}

/// Merges the current project into the shared config model before serialization.
pub(super) fn build_config(
    existing: ExistingConfig,
    project: ProjectConfig,
    sources: &[DetectedRolloutSource],
    root: PathBuf,
) -> SharedConfig {
    let ExistingConfig {
        projects: existing_projects,
        check_for_update_on_startup,
    } = existing;
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

    let mut config = SharedConfig::new(
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
    );
    config.check_for_update_on_startup = check_for_update_on_startup;
    config
}

/// Loads existing shared config fields that should survive init.
pub(super) fn load_existing_config(config_path: &Path) -> Result<ExistingConfig> {
    if !config_path.exists() {
        return Ok(ExistingConfig::default());
    }

    let config = load_config(config_path)?;
    let check_for_update_on_startup = config.check_for_update_on_startup;
    let projects = config
        .projects
        .into_iter()
        .map(normalize_project_config)
        .collect::<Result<Vec<_>>>()?;
    Ok(ExistingConfig {
        projects,
        check_for_update_on_startup,
    })
}

/// Creates the parent directory for a target file path when needed.
pub(super) fn create_parent(path: &Path, label: &str) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{label} is missing a parent directory"))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    Ok(())
}
