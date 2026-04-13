use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use darc_paths::{is_valid_project_id, normalized_known_paths, seed_known_paths, try_git_output};

use crate::config::ProjectConfig;

/// Resolves the current project identity and target shared sessions path.
pub(super) fn detect_project(current_dir: &Path, projects_root: &Path) -> Result<ProjectConfig> {
    let path = try_git_output(current_dir, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or_else(|| current_dir.to_path_buf());
    let path = fs::canonicalize(&path)
        .with_context(|| format!("unable to canonicalize {}", path.display()))?;
    let git_upstream = try_git_output(&path, &["config", "--get", "remote.origin.url"]);

    project_config_from_path(path, projects_root, git_upstream)
}

/// Builds a project config from a resolved local project root.
pub(super) fn project_config_from_path(
    local_path: PathBuf,
    projects_root: &Path,
    git_upstream: Option<String>,
) -> Result<ProjectConfig> {
    let name = project_name_from_path(&local_path)?;
    let id = project_id_from_path(&local_path)?;
    let known_paths = seed_known_paths(&local_path)?;

    Ok(ProjectConfig {
        id: id.clone(),
        name,
        local_path,
        git_upstream,
        sessions_root: default_sessions_root(projects_root, &id),
        known_paths,
    })
}

/// Reuses the existing sessions directory for a known project while refreshing live metadata.
pub(super) fn merge_project_with_existing(
    existing_projects: &[ProjectConfig],
    project: ProjectConfig,
) -> ProjectConfig {
    existing_projects
        .iter()
        .find(|existing| existing.id == project.id)
        .map(|existing| ProjectConfig {
            sessions_root: existing.sessions_root.clone(),
            known_paths: existing.known_paths.clone(),
            ..project.clone()
        })
        .unwrap_or(project)
}

/// Normalizes a loaded project config so legacy entries gain a stable id.
pub(crate) fn normalize_project_config(mut project: ProjectConfig) -> Result<ProjectConfig> {
    project.local_path = canonicalize_if_exists(project.local_path);
    project.known_paths = normalized_known_paths(&project.local_path, &project.known_paths)
        .into_iter()
        .collect();
    if project.id.is_empty() {
        project.id = project_id_from_path(&project.local_path)?;
    } else if !is_valid_project_id(&project.id) {
        anyhow::bail!("configured project id `{}` is invalid", project.id);
    }
    Ok(project)
}

/// Resolves the display name from the local project root directory.
fn project_name_from_path(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("unable to determine project name from {}", path.display()))
}

/// Builds the stable project id from the canonical local project root path.
pub(crate) fn project_id_from_path(path: &Path) -> Result<String> {
    let name = project_name_from_path(path)?;
    Ok(format!(
        "{}-{}",
        slugify_project_name(&name),
        stable_path_hash(path)
    ))
}

/// Converts a display name into a filesystem-safe slug for directory naming.
fn slugify_project_name(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut previous_was_dash = false;

    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            previous_was_dash = false;
        } else if !previous_was_dash {
            slug.push('-');
            previous_was_dash = true;
        }
    }

    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "project".to_owned()
    } else {
        slug.to_owned()
    }
}

/// Computes a short stable hash from the normalized project path.
fn stable_path_hash(path: &Path) -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    const HASH_LEN: usize = 12;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }

    let hash = format!("{hash:016x}");
    hash[..HASH_LEN].to_owned()
}

/// Returns the default sessions directory for a project id.
fn default_sessions_root(projects_root: &Path, id: &str) -> PathBuf {
    projects_root.join(id).join("sessions")
}

/// Canonicalizes a path when it exists and falls back to the original value otherwise.
pub(super) fn canonicalize_if_exists(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}
