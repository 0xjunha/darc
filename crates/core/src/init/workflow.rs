use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use darc_store::ensure_index_database;
use directories::BaseDirs;

use super::{
    config_io::{build_config, create_parent, load_existing_config},
    detect::{codex_home, detect_sources},
    project_config::{detect_project, merge_project_with_existing},
    types::InitDraft,
};
use crate::constants::{CLAUDE_DEFAULT_DIR, CONFIG_FILE_NAME, DEFAULT_ROOT_DIR_NAME};

/// Resolves the default shared root path under the current user's home directory.
pub fn default_root_path() -> PathBuf {
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().join(DEFAULT_ROOT_DIR_NAME))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT_DIR_NAME))
}

/// Builds the init draft and merged config content without writing anything yet.
pub fn prepare_init(root: Option<PathBuf>) -> Result<InitDraft> {
    let base_dirs = BaseDirs::new().context("unable to resolve user home directory")?;
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    let root_path = root.unwrap_or_else(default_root_path);
    let config_path = root_path.join(CONFIG_FILE_NAME);
    let projects_root = root_path.join("projects");
    let sources = detect_sources(&base_dirs)?;
    let global_config_exists = config_path.exists();

    if sources.is_empty() {
        bail!(
            "no Codex or Claude home directories detected\nchecked: {}\nchecked: {}",
            codex_home(base_dirs.home_dir()).display(),
            base_dirs.home_dir().join(CLAUDE_DEFAULT_DIR).display()
        );
    }

    let existing_config = load_existing_config(&config_path)?;
    let detected_project = detect_project(&current_dir, &projects_root)?;
    let project_exists = existing_config
        .projects
        .iter()
        .any(|existing| existing.id == detected_project.id);
    let project = merge_project_with_existing(&existing_config.projects, detected_project);
    let config = build_config(
        existing_config,
        project.clone(),
        &sources,
        root_path.clone(),
    );
    Ok(InitDraft {
        global_config_exists,
        project_exists,
        sources,
        project,
        config,
    })
}

/// Creates the shared directories and writes the merged config file to disk.
pub fn write_init(draft: &InitDraft) -> Result<()> {
    let config_path = draft.config_path();
    let index_db_path = draft.index_db_path();
    let config_toml = draft.config_toml()?;

    fs::create_dir_all(draft.root())
        .with_context(|| format!("failed to create {}", draft.root().display()))?;
    fs::create_dir_all(&draft.project.sessions_root)
        .with_context(|| format!("failed to create {}", draft.project.sessions_root.display()))?;
    create_parent(&config_path, "config path")?;
    fs::write(&config_path, config_toml.as_bytes())
        .with_context(|| format!("failed to write {}", config_path.display()))?;
    ensure_index_database(&index_db_path)?;
    Ok(())
}
