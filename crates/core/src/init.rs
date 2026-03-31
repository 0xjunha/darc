use std::fmt::Display;
use std::{
    env, fmt, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use walkdir::WalkDir;

use crate::config::*;
use crate::constants::*;
use crate::project_paths::{normalized_known_paths, seed_known_paths};

/// Describes the shared config and project directories that `init` will create.
#[derive(Debug, Clone)]
pub struct InitDraft {
    pub config_exists: bool,
    pub sources: Vec<DetectedRolloutSource>,
    pub project: ProjectConfig,
    config: SharedConfig,
}

impl InitDraft {
    /// Returns the shared root path stored in the config.
    pub fn root(&self) -> &Path {
        &self.config.root
    }

    /// Returns the shared config path derived from the root path.
    fn config_path(&self) -> PathBuf {
        self.root().join(CONFIG_FILE_NAME)
    }

    /// Returns the shared index database path derived from the root path.
    fn index_db_path(&self) -> PathBuf {
        self.root().join(INDEX_DB_FILE_NAME)
    }

    /// Serializes the shared config derived during init preparation.
    pub fn config_toml(&self) -> Result<String> {
        toml::to_string_pretty(&self.config).context("failed to serialize config TOML")
    }
}

impl Display for InitDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Detected sources:")?;
        for source in &self.sources {
            writeln!(f, "- {}", format_source(source))?;
        }

        writeln!(f, "\nProject Name: {}", self.project.name)?;
        writeln!(f, "Local Path: {}", self.project.local_path.display())?;
        if let Some(upstream) = &self.project.git_upstream {
            writeln!(f, "Upstream: {upstream}")?;
        }
        writeln!(f, "Root Path: {}", self.root().display())?;
        writeln!(f, "Config Path: {}", self.config_path().display())?;
        write!(f, "Index Path: {}", self.index_db_path().display())
    }
}

/// Summarizes one detected upstream rollout source.
#[derive(Debug, Clone)]
pub struct DetectedRolloutSource {
    pub home: PathBuf,
    pub kind: SourceKind,
    pub root: PathBuf,
    pub rollout_files: usize,
    pub subagent_rollout_files: usize,
}

fn format_source(source: &DetectedRolloutSource) -> String {
    match source.kind {
        SourceKind::Codex => format!(
            "{}: {} ({} rollouts)",
            source.kind.title(),
            source.root.display(),
            source.rollout_files,
        ),
        SourceKind::Claude if source.subagent_rollout_files > 0 => format!(
            "{}: {} ({} sessions, including {} subagents)",
            source.kind.title(),
            source.root.display(),
            source.rollout_files,
            source.subagent_rollout_files,
        ),
        SourceKind::Claude => format!(
            "{}: {} ({} sessions)",
            source.kind.title(),
            source.root.display(),
            source.rollout_files,
        ),
    }
}

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

    if sources.is_empty() {
        bail!(
            "no Codex or Claude home directories detected\nchecked: {}\nchecked: {}",
            codex_home(base_dirs.home_dir()).display(),
            base_dirs.home_dir().join(CLAUDE_DEFAULT_DIR).display()
        );
    }

    let existing_projects = load_existing_projects(&config_path)?;
    let project = merge_project_with_existing(
        &existing_projects,
        detect_project(&current_dir, &projects_root)?,
    );
    let config = build_config(
        existing_projects,
        project.clone(),
        &sources,
        root_path.clone(),
    );
    Ok(InitDraft {
        config_exists: config_path.exists(),
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
    create_parent(&index_db_path, "index database path")?;
    fs::write(&config_path, config_toml.as_bytes())
        .with_context(|| format!("failed to write {}", config_path.display()))?;
    Ok(())
}

/// Merges the current project into the shared config model before serialization.
fn build_config(
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
                .find(|source| source.kind == SourceKind::Claude)
                .map(|source| ClaudeSourceConfig {
                    enabled: true,
                    home: source.home.clone(),
                    include_subagents: true,
                    projects_root: source.root.clone(),
                }),
            codex: sources
                .iter()
                .find(|source| source.kind == SourceKind::Codex)
                .map(|source| CodexSourceConfig {
                    enabled: true,
                    home: source.home.clone(),
                    sessions_root: source.root.clone(),
                }),
        },
    )
}

/// Loads any existing project entries from the shared config file.
fn load_existing_projects(config_path: &Path) -> Result<Vec<ProjectConfig>> {
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

/// Detects all supported upstream rollout sources on the local machine.
fn detect_sources(base_dirs: &BaseDirs) -> Result<Vec<DetectedRolloutSource>> {
    Ok([detect_claude(base_dirs)?, detect_codex(base_dirs)?]
        .into_iter()
        .flatten()
        .collect())
}

/// Detects the local Claude projects tree and counts matching session files.
fn detect_claude(base_dirs: &BaseDirs) -> Result<Option<DetectedRolloutSource>> {
    let home = base_dirs.home_dir().join(CLAUDE_DEFAULT_DIR);
    if !home.exists() {
        return Ok(None);
    }

    let root = home.join("projects");
    let (rollout_files, subagent_rollout_files) = count_rollouts(&root, SourceKind::Claude)?;

    Ok(Some(DetectedRolloutSource {
        home,
        kind: SourceKind::Claude,
        root,
        rollout_files,
        subagent_rollout_files,
    }))
}

/// Detects the local Codex sessions tree and counts matching rollout files.
fn detect_codex(base_dirs: &BaseDirs) -> Result<Option<DetectedRolloutSource>> {
    let home = codex_home(base_dirs.home_dir());
    if !home.exists() {
        return Ok(None);
    }

    let root = home.join("sessions");
    let (rollout_files, subagent_rollout_files) = count_rollouts(&root, SourceKind::Codex)?;

    Ok(Some(DetectedRolloutSource {
        home,
        kind: SourceKind::Codex,
        root,
        rollout_files,
        subagent_rollout_files,
    }))
}

/// Resolves the current project identity and target shared sessions path.
fn detect_project(current_dir: &Path, projects_root: &Path) -> Result<ProjectConfig> {
    let path = try_git_output(current_dir, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or_else(|| current_dir.to_path_buf());
    let path = fs::canonicalize(&path)
        .with_context(|| format!("unable to canonicalize {}", path.display()))?;
    let git_upstream = try_git_output(&path, &["config", "--get", "remote.origin.url"]);

    project_config_from_path(path, projects_root, git_upstream)
}

/// Executes a git command and returns trimmed stdout when it succeeds.
pub(crate) fn try_git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    Some(value.to_owned())
}

/// Returns the effective Codex home directory, honoring `CODEX_HOME` when set.
fn codex_home(home_dir: &Path) -> PathBuf {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir.join(CODEX_DEFAULT_DIR))
}

/// Builds a project config from a resolved local project root.
fn project_config_from_path(
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
fn merge_project_with_existing(
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
fn normalize_project_config(mut project: ProjectConfig) -> Result<ProjectConfig> {
    project.local_path = canonicalize_if_exists(project.local_path);
    project.known_paths = normalized_known_paths(&project.local_path, &project.known_paths)
        .into_iter()
        .collect();
    if project.id.is_empty() {
        project.id = project_id_from_path(&project.local_path)?;
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
fn canonicalize_if_exists(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

/// Counts rollout files for a source kind and tracks Claude subagent files separately.
fn count_rollouts(root: &Path, kind: SourceKind) -> Result<(usize, usize)> {
    let mut rollout_files = 0;
    let mut subagent_rollout_files = 0;
    if !root.exists() {
        return Ok((rollout_files, subagent_rollout_files));
    }

    for entry in WalkDir::new(root) {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }

        match kind {
            SourceKind::Codex => {
                let is_rollout = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"));
                if is_rollout {
                    rollout_files += 1;
                }
            }
            SourceKind::Claude => {
                let is_subagent = path
                    .components()
                    .any(|component| component.as_os_str() == "subagents");
                if is_subagent {
                    subagent_rollout_files += 1;
                }
                rollout_files += 1;
            }
        }
    }

    Ok((rollout_files, subagent_rollout_files))
}

/// Creates the parent directory for a target file path when needed.
fn create_parent(path: &Path, label: &str) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{label} is missing a parent directory"))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("test-{prefix}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn project_ids_do_not_collide_for_same_repo_name() -> Result<()> {
        let projects_root = unique_test_dir("projects-root");
        let left_root = unique_test_dir("api-left").join("api");
        let right_root = unique_test_dir("api-right").join("api");
        fs::create_dir_all(&left_root)?;
        fs::create_dir_all(&right_root)?;

        let left = project_config_from_path(left_root, &projects_root, None)?;
        let right = project_config_from_path(right_root, &projects_root, None)?;

        assert_ne!(left.id, right.id);
        assert_ne!(left.sessions_root, right.sessions_root);
        assert_eq!(left.name, "api");
        assert_eq!(right.name, "api");

        Ok(())
    }

    #[test]
    fn load_existing_projects_assigns_ids_without_moving_sessions_root() -> Result<()> {
        let workspace_root = unique_test_dir("legacy-config");
        let projects_root = workspace_root.join("projects");
        let project_root = workspace_root.join("repo");
        fs::create_dir_all(&projects_root)?;
        fs::create_dir_all(&project_root)?;

        let config_path = workspace_root.join(CONFIG_FILE_NAME);
        let legacy_sessions_root = projects_root.join("repo").join("sessions");
        let config_toml = format!(
            "[[projects]]\nname = \"repo\"\nlocal_path = \"{}\"\nsessions_root = \"{}\"\n",
            project_root.display(),
            legacy_sessions_root.display()
        );
        fs::write(&config_path, config_toml)?;

        let projects = load_existing_projects(&config_path)?;
        let project = &projects[0];

        assert!(!project.id.is_empty());
        assert_eq!(project.local_path, canonicalize_if_exists(project_root));
        assert_eq!(project.sessions_root, legacy_sessions_root);

        Ok(())
    }

    #[test]
    fn load_existing_projects_drops_local_path_from_known_paths() -> Result<()> {
        let workspace_root = unique_test_dir("legacy-known-paths");
        let project_root = workspace_root.join("repo");
        let worktree_root = workspace_root.join("wt1");
        fs::create_dir_all(&project_root)?;
        fs::create_dir_all(&worktree_root)?;

        let config = SharedConfig::new(
            workspace_root.clone(),
            vec![ProjectConfig {
                id: "repo-abc123".into(),
                name: "repo".into(),
                local_path: project_root.clone(),
                git_upstream: None,
                sessions_root: workspace_root.join("projects/repo-abc123/sessions"),
                known_paths: vec![project_root.clone(), worktree_root.clone()],
            }],
            SourcesConfig::default(),
        );
        let config_path = workspace_root.join(CONFIG_FILE_NAME);
        fs::write(&config_path, toml::to_string_pretty(&config)?)?;

        let projects = load_existing_projects(&config_path)?;

        assert_eq!(projects.len(), 1);
        assert_eq!(
            projects[0].known_paths,
            vec![canonicalize_if_exists(worktree_root)]
        );

        Ok(())
    }

    #[test]
    fn merge_project_with_existing_keeps_existing_sessions_root() -> Result<()> {
        let projects_root = unique_test_dir("merge-projects");
        let project_root = unique_test_dir("merge-repo");
        fs::create_dir_all(&projects_root)?;
        fs::create_dir_all(&project_root)?;

        let detected = project_config_from_path(project_root.clone(), &projects_root, None)?;
        let existing_sessions_root = projects_root.join("legacy-repo").join("sessions");
        let existing = ProjectConfig {
            sessions_root: existing_sessions_root.clone(),
            ..detected.clone()
        };

        let merged = merge_project_with_existing(&[existing], detected);

        assert_eq!(merged.sessions_root, existing_sessions_root);
        assert_eq!(merged.id, project_id_from_path(&project_root)?);

        Ok(())
    }

    #[test]
    fn config_round_trips_with_known_paths() -> Result<()> {
        let dir = unique_test_dir("round-trip");
        fs::create_dir_all(&dir)?;

        let config = SharedConfig::new(
            dir.clone(),
            vec![ProjectConfig {
                id: "test-abc123".into(),
                name: "test".into(),
                local_path: dir.join("repo"),
                git_upstream: None,
                sessions_root: dir.join("sessions"),
                known_paths: vec![dir.join("wt1"), dir.join("wt2")],
            }],
            SourcesConfig::default(),
        );

        let toml_str = toml::to_string_pretty(&config)?;
        let loaded: SharedConfig = toml::from_str(&toml_str)?;

        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].known_paths.len(), 2);
        assert_eq!(loaded.projects[0].known_paths[0], dir.join("wt1"));

        Ok(())
    }

    #[test]
    fn config_deserializes_without_known_paths() -> Result<()> {
        let dir = unique_test_dir("no-known-paths");
        fs::create_dir_all(&dir)?;

        let toml_str = format!(
            "version = 1\nroot = \"{dir}\"\n\n\
             [[projects]]\nid = \"x-123\"\nname = \"x\"\n\
             local_path = \"{dir}/repo\"\nsessions_root = \"{dir}/sessions\"\n",
            dir = dir.display()
        );
        let loaded: SharedConfig = toml::from_str(&toml_str)?;

        assert_eq!(loaded.projects.len(), 1);
        assert!(loaded.projects[0].known_paths.is_empty());

        Ok(())
    }
}
