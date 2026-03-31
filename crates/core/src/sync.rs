use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::{
    config::{
        ClaudeSourceConfig, CodexSourceConfig, ProjectConfig, SharedConfig, SourceKind, load_config,
    },
    constants::CONFIG_FILE_NAME,
    default_root_path,
    project_paths::{
        current_project_root, encode_path_for_claude, normalize_project_path,
        normalized_known_paths, project_path_set, try_git_output,
    },
};

const MANIFEST_VERSION: u32 = 1;
static UNIQUE_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Collects optional filters for the `sync` workflow.
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    pub source_filter: Vec<SourceKind>,
}

/// Describes a prepared sync before any writes happen.
#[derive(Debug, Clone)]
pub struct SyncPlan {
    pub project_name: String,
    pub project_root: PathBuf,
    pub sessions_root: PathBuf,
    pub sources: Vec<SourceKind>,
    pub sessions_to_copy: usize,
    pub sessions_unchanged: usize,
    pub auxiliary_to_copy: usize,
    pub auxiliary_unchanged: usize,
    pub new_known_paths: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub manifest_written: bool,
    pub config_written: bool,
    manifest_path: PathBuf,
    config_path: PathBuf,
    manifest: Manifest,
    config_toml: Option<String>,
    session_copies: Vec<PendingCopy>,
    auxiliary_copies: Vec<PendingCopy>,
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

/// Executes a prepared sync by copying files and atomically updating metadata.
pub fn execute_sync(plan: SyncPlan) -> Result<SyncReport> {
    for copy in &plan.session_copies {
        copy_file_atomically(&copy.source_path, &copy.destination_path)?;
    }
    for copy in &plan.auxiliary_copies {
        copy_file_atomically(&copy.source_path, &copy.destination_path)?;
    }
    if plan.manifest_written {
        write_json_atomically(&plan.manifest_path, &plan.manifest)?;
    }
    if let Some(config_toml) = &plan.config_toml {
        write_text_atomically(&plan.config_path, config_toml)?;
    }

    Ok(SyncReport {
        project_name: plan.project_name,
        project_root: plan.project_root,
        sessions_root: plan.sessions_root,
        sources: plan.sources,
        sessions_copied: plan.session_copies.len(),
        sessions_unchanged: plan.sessions_unchanged,
        auxiliary_copied: plan.auxiliary_copies.len(),
        auxiliary_unchanged: plan.auxiliary_unchanged,
        new_known_paths: plan.new_known_paths,
        warnings: plan.warnings,
        manifest_written: plan.manifest_written,
        config_written: plan.config_written,
    })
}

/// Plans a sync from an explicit working directory for deterministic tests.
fn prepare_sync_from(current_dir: &Path, root: PathBuf, options: SyncOptions) -> Result<SyncPlan> {
    let config_path = root.join(CONFIG_FILE_NAME);
    if !config_path.exists() {
        bail!(
            "shared config not found at {}\nrun `memstack init --root {}` from your project root first",
            config_path.display(),
            root.display()
        );
    }

    let mut config = load_config(&config_path)?;
    let current_root = current_project_root(current_dir)?;
    let current_live_paths = project_path_set(&current_root, &[])?;
    let project_index = find_project_index(&config.projects, &current_live_paths)?;
    let project = config.projects[project_index].clone();
    let primary_project_path = normalize_project_path(&project.local_path);
    let full_project_paths = project_path_set(&current_root, &project.known_paths)?;
    let project_upstream = try_git_output(&current_root, &["config", "--get", "remote.origin.url"])
        .or(project.git_upstream.clone());
    let sources = selected_sources(&config, &options.source_filter)?;
    let manifest_path = project.sessions_root.join(".manifest.json");
    let mut manifest = load_manifest(&manifest_path)?;
    let synced_at = format_system_time_utc(SystemTime::now())?;
    let mut warnings = Vec::new();

    let mut discovered_sessions = Vec::new();
    let mut discovered_auxiliary = Vec::new();
    let mut persisted_known_paths = full_project_paths.clone();
    persisted_known_paths.remove(&primary_project_path);

    for source in &sources {
        match source {
            SourceKind::Claude => {
                if let Some(claude) = &config.sources.claude {
                    let discovery =
                        discover_claude_sessions(claude, &full_project_paths, &mut warnings)?;
                    discovered_sessions.extend(discovery.sessions);
                    discovered_auxiliary.extend(discovery.auxiliary);
                }
            }
            SourceKind::Codex => {
                if let Some(codex) = &config.sources.codex {
                    let discovery = discover_codex_sessions(
                        codex,
                        &full_project_paths,
                        project_upstream.as_deref(),
                        &mut warnings,
                    )?;
                    for session in &discovery.sessions {
                        if let Some(path) = &session.cwd
                            && path != &primary_project_path
                        {
                            persisted_known_paths.insert(path.clone());
                        }
                    }
                    discovered_sessions.extend(discovery.sessions);
                }
            }
        }
    }

    let mut manifest_written = false;
    let mut session_copies = Vec::new();
    let mut sessions_unchanged = 0;
    for session in discovered_sessions {
        let existing = manifest.sessions.get(&session.id);
        let should_copy = existing
            .is_none_or(|entry| entry.size != session.size || entry.mtime_ms != session.mtime_ms);
        if !should_copy {
            sessions_unchanged += 1;
        }
        if should_copy || existing.is_none_or(|entry| !entry.matches(&session)) {
            manifest_written = true;
            manifest
                .sessions
                .insert(session.id.clone(), session.manifest_entry(&synced_at));
        }
        if should_copy {
            session_copies.push(PendingCopy {
                source_path: session.source_path,
                destination_path: project.sessions_root.join(&session.archive_path),
            });
        }
    }

    let mut auxiliary_copies = Vec::new();
    let mut auxiliary_unchanged = 0;
    for auxiliary in discovered_auxiliary {
        let existing = manifest.auxiliary.get(&auxiliary.key);
        let should_copy = existing.is_none_or(|entry| {
            entry.size != auxiliary.size || entry.mtime_ms != auxiliary.mtime_ms
        });
        if !should_copy {
            auxiliary_unchanged += 1;
        }
        if should_copy || existing.is_none_or(|entry| !entry.matches(&auxiliary)) {
            manifest_written = true;
            manifest
                .auxiliary
                .insert(auxiliary.key.clone(), auxiliary.manifest_entry(&synced_at));
        }
        if should_copy {
            auxiliary_copies.push(PendingCopy {
                source_path: auxiliary.source_path,
                destination_path: project.sessions_root.join(&auxiliary.archive_path),
            });
        }
    }

    let previous_known_paths = normalized_known_paths(&project.local_path, &project.known_paths);
    let normalized_known_paths_vec: Vec<_> = previous_known_paths.iter().cloned().collect();
    let new_known_paths = persisted_known_paths
        .difference(&previous_known_paths)
        .cloned()
        .collect::<Vec<_>>();
    let config_toml = if new_known_paths.is_empty()
        && config.projects[project_index].known_paths == normalized_known_paths_vec
    {
        None
    } else {
        config.projects[project_index].known_paths = persisted_known_paths.into_iter().collect();
        Some(toml::to_string_pretty(&config).context("failed to serialize updated shared config")?)
    };

    Ok(SyncPlan {
        project_name: project.name,
        project_root: current_root,
        sessions_root: project.sessions_root,
        sources,
        sessions_to_copy: session_copies.len(),
        sessions_unchanged,
        auxiliary_to_copy: auxiliary_copies.len(),
        auxiliary_unchanged,
        new_known_paths,
        warnings,
        manifest_written,
        config_written: config_toml.is_some(),
        manifest_path,
        config_path,
        manifest,
        config_toml,
        session_copies,
        auxiliary_copies,
    })
}

/// Matches the current repo or worktree against configured projects.
fn find_project_index(
    projects: &[ProjectConfig],
    current_paths: &BTreeSet<PathBuf>,
) -> Result<usize> {
    let mut matches = Vec::new();

    for (index, project) in projects.iter().enumerate() {
        let mut project_paths = normalized_known_paths(&project.local_path, &project.known_paths);
        project_paths.insert(normalize_project_path(&project.local_path));
        if !project_paths.is_disjoint(current_paths) {
            matches.push(index);
        }
    }

    match matches.as_slice() {
        [] => bail!("current directory does not match any configured memstack project"),
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

/// Resolves the enabled source list after applying any CLI filter.
fn selected_sources(config: &SharedConfig, filter: &[SourceKind]) -> Result<Vec<SourceKind>> {
    let filter = filter.iter().copied().collect::<BTreeSet<_>>();
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
            .into_iter()
            .map(SourceKind::title)
            .collect::<Vec<_>>()
            .join(", ");
        bail!("no enabled rollout sources matched the requested filter: {wanted}");
    }

    Ok(sources)
}

/// Loads the on-disk manifest or returns an empty v1 manifest.
fn load_manifest(path: &Path) -> Result<Manifest> {
    if !path.exists() {
        return Ok(Manifest::default());
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let manifest: Manifest =
        serde_json::from_str(&content).context("failed to parse sync manifest")?;
    if manifest.version != MANIFEST_VERSION {
        bail!(
            "unsupported manifest version {} at {}",
            manifest.version,
            path.display()
        );
    }
    Ok(manifest)
}

/// Discovers Claude parent sessions and auxiliary files for the project path set.
fn discover_claude_sessions(
    source: &ClaudeSourceConfig,
    project_paths: &BTreeSet<PathBuf>,
    warnings: &mut Vec<String>,
) -> Result<ClaudeDiscovery> {
    let encoded_paths = project_paths
        .iter()
        .map(|path| encode_path_for_claude(path))
        .collect::<BTreeSet<_>>();
    let directories = matching_claude_dirs(&source.projects_root, &encoded_paths, warnings)?;
    let mut sessions = Vec::new();
    let mut auxiliary = Vec::new();

    for directory in directories {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                warnings.push(format!("skipped {}: {error}", directory.display()));
                continue;
            }
        };
        let mut session_ids = BTreeSet::new();
        let mut session_directories = Vec::new();

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warnings.push(format!(
                        "failed to read Claude entry in {}: {error}",
                        directory.display()
                    ));
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    warnings.push(format!("failed to inspect {}: {error}", path.display()));
                    continue;
                }
            };

            if file_type.is_file()
                && path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
            {
                let Some(file_name) = path.file_name() else {
                    warnings.push(format!(
                        "skipped Claude session with invalid filename: {}",
                        path.display()
                    ));
                    continue;
                };
                let Some(session_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    warnings.push(format!(
                        "skipped Claude session with invalid stem: {}",
                        path.display()
                    ));
                    continue;
                };
                let snapshot = match file_snapshot(&path) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        warnings.push(format!("skipped {}: {error:#}", path.display()));
                        continue;
                    }
                };

                session_ids.insert(session_id.to_owned());
                sessions.push(DiscoveredSession {
                    id: session_id.to_owned(),
                    provider: SourceKind::Claude,
                    source_path: path.clone(),
                    archive_path: PathBuf::from(SourceKind::Claude.directory_name())
                        .join(session_id)
                        .join(file_name),
                    cwd: None,
                    size: snapshot.size,
                    mtime_ms: snapshot.mtime_ms,
                });
                continue;
            }

            if file_type.is_dir() && source.include_subagents {
                session_directories.push(path);
            }
        }

        for path in session_directories {
            let Some(parent_session) = path.file_name().and_then(|name| name.to_str()) else {
                warnings.push(format!(
                    "skipped Claude auxiliary directory with invalid name: {}",
                    path.display()
                ));
                continue;
            };
            if !session_ids.contains(parent_session) {
                continue;
            }
            discover_claude_auxiliary_dir(&path, parent_session, &mut auxiliary, warnings)?;
        }
    }

    Ok(ClaudeDiscovery {
        sessions,
        auxiliary,
    })
}

/// Enumerates Claude project directories that exactly match the encoded project path set.
fn matching_claude_dirs(
    projects_root: &Path,
    encoded_set: &BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> Result<Vec<PathBuf>> {
    if !projects_root.exists() {
        return Ok(Vec::new());
    }

    let entries = fs::read_dir(projects_root)
        .with_context(|| format!("failed to read {}", projects_root.display()))?;
    let mut matches = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!("failed to read Claude projects entry: {error}"));
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                warnings.push(format!(
                    "failed to inspect {}: {error}",
                    entry.path().display()
                ));
                continue;
            }
        };
        if !file_type.is_dir() {
            continue;
        }

        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            warnings.push(format!(
                "skipped Claude project with invalid UTF-8 name: {}",
                entry.path().display()
            ));
            continue;
        };
        if encoded_set.contains(file_name) {
            matches.push(entry.path());
        }
    }

    Ok(matches)
}

/// Walks one Claude session directory and captures supported auxiliary artifacts.
fn discover_claude_auxiliary_dir(
    session_dir: &Path,
    parent_session: &str,
    auxiliary: &mut Vec<DiscoveredAuxiliary>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    for entry in WalkDir::new(session_dir).min_depth(1) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!("failed to walk {}: {error}", session_dir.display()));
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        if !is_supported_claude_auxiliary(path) {
            continue;
        }

        let snapshot = match file_snapshot(path) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warnings.push(format!("skipped {}: {error:#}", path.display()));
                continue;
            }
        };
        let relative = path.strip_prefix(session_dir).with_context(|| {
            format!(
                "failed to strip {} from {}",
                session_dir.display(),
                path.display()
            )
        })?;

        auxiliary.push(DiscoveredAuxiliary {
            key: path.to_string_lossy().into_owned(),
            source_path: path.to_path_buf(),
            archive_path: PathBuf::from(SourceKind::Claude.directory_name())
                .join(parent_session)
                .join(relative),
            parent_session: parent_session.to_owned(),
            size: snapshot.size,
            mtime_ms: snapshot.mtime_ms,
        });
    }

    Ok(())
}

/// Returns whether a Claude file should be archived as auxiliary metadata.
fn is_supported_claude_auxiliary(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
        || path.extension().and_then(|extension| extension.to_str()) == Some("txt")
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".meta.json"))
}

/// Discovers Codex rollout files, matches them by cwd, and resolves duplicates by logical session id.
fn discover_codex_sessions(
    source: &CodexSourceConfig,
    project_paths: &BTreeSet<PathBuf>,
    project_upstream: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<CodexDiscovery> {
    let archived_root = source.home.join("archived_sessions");
    let mut candidates = BTreeMap::<String, Vec<CodexCandidate>>::new();
    let mut repo_cache = CodexRepoCache::default();

    for root in [&source.sessions_root, &archived_root] {
        if !root.exists() {
            continue;
        }

        for entry in WalkDir::new(root) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warnings.push(format!("failed to walk {}: {error}", root.display()));
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                warnings.push(format!(
                    "skipped Codex rollout with invalid UTF-8 filename: {}",
                    path.display()
                ));
                continue;
            };
            let Some(session_id) = parse_codex_ulid(file_name) else {
                continue;
            };
            let snapshot = match file_snapshot(path) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    warnings.push(format!("skipped {}: {error:#}", path.display()));
                    continue;
                }
            };

            let (meta, matches_project) = match extract_codex_session_meta(path) {
                Ok(Some(meta)) => {
                    if meta.id != session_id {
                        warnings.push(format!(
                            "Codex session id mismatch in {}: filename={} payload={}",
                            path.display(),
                            session_id,
                            meta.id
                        ));
                    }
                    let repo_root = repo_cache.repo_root(&meta.cwd);
                    let matches_project = codex_session_matches_project(
                        &repo_root,
                        project_paths,
                        project_upstream,
                        &mut repo_cache,
                    );
                    (
                        Some(CodexSessionMeta {
                            id: meta.id,
                            cwd: repo_root,
                        }),
                        matches_project,
                    )
                }
                Ok(None) => {
                    warnings.push(format!(
                        "skipped Codex session meta match for {}: first line was not session_meta",
                        path.display()
                    ));
                    (None, false)
                }
                Err(error) => {
                    warnings.push(format!(
                        "failed to parse Codex session meta from {}: {error:#}",
                        path.display()
                    ));
                    (None, false)
                }
            };

            candidates
                .entry(session_id.clone())
                .or_default()
                .push(CodexCandidate {
                    session_id,
                    source_path: path.to_path_buf(),
                    archive_path: PathBuf::from(SourceKind::Codex.directory_name()).join(file_name),
                    meta,
                    matches_project,
                    size: snapshot.size,
                    mtime_ms: snapshot.mtime_ms,
                });
        }
    }

    let sessions = candidates
        .into_values()
        .filter_map(|group| select_codex_candidate(&group))
        .map(|candidate| DiscoveredSession {
            id: candidate.session_id.clone(),
            provider: SourceKind::Codex,
            source_path: candidate.source_path.clone(),
            archive_path: candidate.archive_path.clone(),
            cwd: candidate.meta.as_ref().map(|meta| meta.cwd.clone()),
            size: candidate.size,
            mtime_ms: candidate.mtime_ms,
        })
        .collect();

    Ok(CodexDiscovery { sessions })
}

/// Returns whether a Codex session belongs to the active project.
fn codex_session_matches_project(
    repo_root: &Path,
    project_paths: &BTreeSet<PathBuf>,
    project_upstream: Option<&str>,
    repo_cache: &mut CodexRepoCache,
) -> bool {
    project_paths.contains(repo_root)
        || project_upstream.is_some_and(|upstream| {
            repo_cache.remote_origin(repo_root).as_deref() == Some(upstream)
        })
}

/// Chooses the winning Codex copy for one logical session id.
fn select_codex_candidate(candidates: &[CodexCandidate]) -> Option<CodexCandidate> {
    if !candidates.iter().any(|candidate| candidate.matches_project) {
        return None;
    }

    candidates
        .iter()
        .filter(|candidate| candidate.matches_project && candidate.meta.is_some())
        .max_by(|left, right| {
            left.size
                .cmp(&right.size)
                .then_with(|| left.mtime_ms.cmp(&right.mtime_ms))
        })
        .cloned()
}

/// Extracts the Codex `session_meta` id and cwd from the first rollout line.
fn extract_codex_session_meta(path: &Path) -> Result<Option<CodexSessionMeta>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }

    let envelope: CodexSessionMetaEnvelope =
        serde_json::from_str(&line).context("failed to deserialize the first JSONL line")?;
    if envelope.event_type.as_deref() != Some("session_meta") {
        return Ok(None);
    }

    Ok(Some(CodexSessionMeta {
        id: envelope.payload.id,
        cwd: normalize_project_path(&envelope.payload.cwd),
    }))
}

/// Extracts the logical Codex session id from a rollout filename.
fn parse_codex_ulid(file_name: &str) -> Option<String> {
    let trimmed = file_name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let start = trimmed.len().checked_sub(36)?;
    (start > 0 && trimmed.as_bytes().get(start - 1) == Some(&b'-'))
        .then(|| trimmed[start..].to_owned())
}

/// Reads stable copy-comparison metadata from a source file.
fn file_snapshot(path: &Path) -> Result<FileSnapshot> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("failed to read modified time for {}", path.display()))?;

    Ok(FileSnapshot {
        size: metadata.len(),
        mtime_ms: system_time_to_millis(modified)?,
    })
}

/// Copies a file via a temp sibling path and renames it into place.
fn copy_file_atomically(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .with_context(|| format!("missing parent directory for {}", destination.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let temp_path = unique_sibling_path(destination, "tmp");
    fs::copy(source, &temp_path).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            temp_path.display()
        )
    })?;
    rename_atomically(&temp_path, destination)
}

/// Writes JSON content through a temp sibling path and renames it into place.
fn write_json_atomically<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let content = serde_json::to_vec_pretty(value).context("failed to serialize JSON")?;
    write_bytes_atomically(path, &content)
}

/// Writes UTF-8 text through a temp sibling path and renames it into place.
fn write_text_atomically(path: &Path, content: &str) -> Result<()> {
    write_bytes_atomically(path, content.as_bytes())
}

/// Writes raw bytes through a temp sibling path and renames it into place.
fn write_bytes_atomically(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("missing parent directory for {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let temp_path = unique_sibling_path(path, "tmp");
    fs::write(&temp_path, content)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    rename_atomically(&temp_path, path)
}

/// Renames a temp path into place, replacing an existing target when necessary.
fn rename_atomically(temp_path: &Path, destination: &Path) -> Result<()> {
    match fs::rename(temp_path, destination) {
        Ok(()) => Ok(()),
        Err(error) if destination.exists() => replace_existing_file(temp_path, destination, error),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to rename {} to {}",
                temp_path.display(),
                destination.display()
            )
        }),
    }
}

/// Replaces an existing file while keeping the original available until the swap succeeds.
fn replace_existing_file(
    temp_path: &Path,
    destination: &Path,
    rename_error: std::io::Error,
) -> Result<()> {
    let backup_path = unique_sibling_path(destination, "bak");
    fs::rename(destination, &backup_path).with_context(|| {
        format!(
            "failed to move {} aside after renaming {} to {} failed: {rename_error}",
            destination.display(),
            temp_path.display(),
            destination.display()
        )
    })?;

    match fs::rename(temp_path, destination) {
        Ok(()) => fs::remove_file(&backup_path)
            .with_context(|| format!("failed to remove backup {}", backup_path.display())),
        Err(error) => match fs::rename(&backup_path, destination) {
            Ok(()) => Err(error).with_context(|| {
                format!(
                    "failed to rename {} to {} after moving the existing file aside",
                    temp_path.display(),
                    destination.display()
                )
            }),
            Err(restore_error) => bail!(
                "failed to rename {} to {} after moving aside {}; also failed to restore backup {}: {error}; {restore_error}",
                temp_path.display(),
                destination.display(),
                destination.display(),
                backup_path.display()
            ),
        },
    }
}

/// Builds a unique sibling path for atomic replace temp and backup files.
fn unique_sibling_path(path: &Path, kind: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "memstack-temp".to_owned());
    path.with_file_name(format!(
        ".{file_name}.memstack-{kind}-{}-{}",
        std::process::id(),
        UNIQUE_PATH_COUNTER.fetch_add(1, Ordering::Relaxed),
    ))
}

/// Converts a `SystemTime` to Unix epoch milliseconds.
fn system_time_to_millis(time: SystemTime) -> Result<u64> {
    let millis = time
        .duration_since(UNIX_EPOCH)
        .context("time was before the Unix epoch")?
        .as_millis();
    u64::try_from(millis).context("millisecond timestamp overflowed u64")
}

/// Formats a `SystemTime` as a UTC RFC 3339 timestamp.
fn format_system_time_utc(time: SystemTime) -> Result<String> {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .context("time was before the Unix epoch")?
        .as_secs();
    let days = i64::try_from(seconds / 86_400).context("day count overflowed i64")?;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;

    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

/// Converts days since the Unix epoch into a UTC calendar date.
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// Captures supported Claude discovery results.
#[derive(Debug, Default)]
struct ClaudeDiscovery {
    sessions: Vec<DiscoveredSession>,
    auxiliary: Vec<DiscoveredAuxiliary>,
}

/// Captures supported Codex discovery results.
#[derive(Debug, Default)]
struct CodexDiscovery {
    sessions: Vec<DiscoveredSession>,
}

/// Stores source and destination metadata for a pending copy.
#[derive(Debug, Clone)]
struct PendingCopy {
    source_path: PathBuf,
    destination_path: PathBuf,
}

/// Stores one discovered parent session before manifest diffing.
#[derive(Debug, Clone)]
struct DiscoveredSession {
    id: String,
    provider: SourceKind,
    source_path: PathBuf,
    archive_path: PathBuf,
    cwd: Option<PathBuf>,
    size: u64,
    mtime_ms: u64,
}

impl DiscoveredSession {
    /// Builds the manifest entry for one discovered parent session.
    fn manifest_entry(&self, synced_at: &str) -> ManifestSessionEntry {
        ManifestSessionEntry {
            provider: self.provider,
            source_path: self.source_path.clone(),
            archive_path: self.archive_path.clone(),
            cwd: self.cwd.clone(),
            size: self.size,
            mtime_ms: self.mtime_ms,
            synced_at: synced_at.to_owned(),
        }
    }
}

/// Stores one discovered Claude auxiliary artifact before manifest diffing.
#[derive(Debug, Clone)]
struct DiscoveredAuxiliary {
    key: String,
    source_path: PathBuf,
    archive_path: PathBuf,
    parent_session: String,
    size: u64,
    mtime_ms: u64,
}

impl DiscoveredAuxiliary {
    /// Builds the manifest entry for one discovered auxiliary artifact.
    fn manifest_entry(&self, synced_at: &str) -> ManifestAuxiliaryEntry {
        ManifestAuxiliaryEntry {
            parent_session: self.parent_session.clone(),
            archive_path: self.archive_path.clone(),
            size: self.size,
            mtime_ms: self.mtime_ms,
            synced_at: synced_at.to_owned(),
        }
    }
}

/// Stores the first-line Codex metadata needed for project matching.
#[derive(Debug, Clone, Eq, PartialEq)]
struct CodexSessionMeta {
    id: String,
    cwd: PathBuf,
}

/// Stores one Codex rollout candidate before duplicate resolution.
#[derive(Debug, Clone)]
struct CodexCandidate {
    session_id: String,
    source_path: PathBuf,
    archive_path: PathBuf,
    meta: Option<CodexSessionMeta>,
    matches_project: bool,
    size: u64,
    mtime_ms: u64,
}

/// Caches Codex repo lookups for one sync run.
#[derive(Debug, Default)]
struct CodexRepoCache {
    repo_root_by_cwd: BTreeMap<PathBuf, PathBuf>,
    remote_origin_by_root: BTreeMap<PathBuf, Option<String>>,
}

impl CodexRepoCache {
    /// Resolves the repo root for one reported Codex cwd.
    fn repo_root(&mut self, cwd: &Path) -> PathBuf {
        let cwd = normalize_project_path(cwd);
        if let Some(repo_root) = self.repo_root_by_cwd.get(&cwd) {
            return repo_root.clone();
        }

        let repo_root = current_project_root(&cwd).unwrap_or_else(|_| cwd.clone());
        self.repo_root_by_cwd.insert(cwd, repo_root.clone());
        repo_root
    }

    /// Resolves the git origin for one repo root.
    fn remote_origin(&mut self, repo_root: &Path) -> Option<String> {
        if let Some(remote_origin) = self.remote_origin_by_root.get(repo_root) {
            return remote_origin.clone();
        }

        let remote_origin = try_git_output(repo_root, &["config", "--get", "remote.origin.url"]);
        self.remote_origin_by_root
            .insert(repo_root.to_path_buf(), remote_origin.clone());
        remote_origin
    }
}

/// Stores file metadata used for change detection.
#[derive(Debug, Clone, Copy)]
struct FileSnapshot {
    size: u64,
    mtime_ms: u64,
}

/// Mirrors the on-disk sync manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Manifest {
    version: u32,
    #[serde(default)]
    sessions: BTreeMap<String, ManifestSessionEntry>,
    #[serde(default)]
    auxiliary: BTreeMap<String, ManifestAuxiliaryEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            sessions: BTreeMap::new(),
            auxiliary: BTreeMap::new(),
        }
    }
}

/// Stores one parent-session manifest entry keyed by logical session id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ManifestSessionEntry {
    provider: SourceKind,
    source_path: PathBuf,
    archive_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<PathBuf>,
    size: u64,
    mtime_ms: u64,
    synced_at: String,
}

impl ManifestSessionEntry {
    /// Returns whether a discovered session matches the stored metadata except `synced_at`.
    fn matches(&self, session: &DiscoveredSession) -> bool {
        self.provider == session.provider
            && self.source_path == session.source_path
            && self.archive_path == session.archive_path
            && self.cwd == session.cwd
            && self.size == session.size
            && self.mtime_ms == session.mtime_ms
    }
}

/// Stores one auxiliary manifest entry keyed by source path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ManifestAuxiliaryEntry {
    parent_session: String,
    archive_path: PathBuf,
    size: u64,
    mtime_ms: u64,
    synced_at: String,
}

impl ManifestAuxiliaryEntry {
    /// Returns whether a discovered auxiliary file matches the stored metadata except `synced_at`.
    fn matches(&self, auxiliary: &DiscoveredAuxiliary) -> bool {
        self.parent_session == auxiliary.parent_session
            && self.archive_path == auxiliary.archive_path
            && self.size == auxiliary.size
            && self.mtime_ms == auxiliary.mtime_ms
    }
}

/// Deserializes the first line of a Codex rollout.
#[derive(Debug, Deserialize)]
struct CodexSessionMetaEnvelope {
    #[serde(rename = "type")]
    event_type: Option<String>,
    payload: CodexSessionMetaPayload,
}

/// Deserializes the `payload` field from a Codex `session_meta` line.
#[derive(Debug, Deserialize)]
struct CodexSessionMetaPayload {
    id: String,
    cwd: PathBuf,
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn unique_test_dir(prefix: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "test-{prefix}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ))
    }

    fn write_file(path: &Path, content: &str) -> Result<()> {
        let parent = path.parent().context("missing parent directory")?;
        fs::create_dir_all(parent)?;
        fs::write(path, content)?;
        Ok(())
    }

    fn run_git(cwd: &Path, args: &[&str]) -> Result<()> {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .with_context(|| format!("failed to run git {:?} in {}", args, cwd.display()))?;
        if output.status.success() {
            return Ok(());
        }

        bail!(
            "git {:?} failed in {}: {}",
            args,
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }

    fn init_git_repo(path: &Path, remote: &str) -> Result<()> {
        fs::create_dir_all(path)?;
        run_git(path, &["init"])?;
        run_git(path, &["remote", "add", "origin", remote])
    }

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
                id: "memstack-abc123".into(),
                name: "memstack".into(),
                local_path: project_root.to_path_buf(),
                git_upstream: None,
                sessions_root: root.join("projects/memstack-abc123/sessions"),
                known_paths: Vec::new(),
            }],
            crate::config::SourcesConfig {
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

    #[test]
    fn parse_codex_ulid_reads_logical_id() {
        let id = parse_codex_ulid(
            "rollout-2026-03-27T17-53-08-019d2e7f-4e07-7940-8d37-0a04e9aeb621.jsonl",
        );

        assert_eq!(id.as_deref(), Some("019d2e7f-4e07-7940-8d37-0a04e9aeb621"));
    }

    #[test]
    fn extract_codex_session_meta_normalizes_cwd() -> Result<()> {
        let dir = unique_test_dir("codex-meta");
        let rollout = dir.join("rollout.jsonl");
        write_file(
            &rollout,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\",\"cwd\":\"/tmp/demo/./worktree/../repo/\"}}\n",
        )?;

        let meta = extract_codex_session_meta(&rollout)?.context("missing meta")?;

        assert_eq!(meta.cwd, PathBuf::from("/tmp/demo/repo"));
        assert_eq!(meta.id, "x");

        Ok(())
    }

    #[test]
    fn matching_claude_dirs_uses_exact_names_only() -> Result<()> {
        let root = unique_test_dir("claude-match");
        fs::create_dir_all(root.join("-tmp-main"))?;
        fs::create_dir_all(root.join("-tmp-main-extra"))?;
        let mut warnings = Vec::new();

        let matches = matching_claude_dirs(
            &root,
            &BTreeSet::from(["-tmp-main".to_owned()]),
            &mut warnings,
        )?;

        assert_eq!(matches, vec![root.join("-tmp-main")]);
        assert!(warnings.is_empty());

        Ok(())
    }

    #[test]
    fn discover_claude_sessions_ignores_orphan_directories() -> Result<()> {
        let root = unique_test_dir("claude-orphan");
        let project_root = root.join("repo");
        let canonical_project_root = {
            fs::create_dir_all(&project_root)?;
            fs::canonicalize(&project_root)?
        };
        let projects_root = root.join("claude/projects");
        let session_id = "885a05b8-f731-4fde-bfdb-a24ce28dc9c3";
        let project_dir = projects_root.join(encode_path_for_claude(&canonical_project_root));
        let mut warnings = Vec::new();

        write_file(
            &project_dir.join(format!("{session_id}.jsonl")),
            "{\"type\":\"message\"}\n",
        )?;
        write_file(
            &project_dir.join(format!("{session_id}/subagents/agent-a1.jsonl")),
            "{\"type\":\"subagent\"}\n",
        )?;
        write_file(&project_dir.join("orphan/note.txt"), "orphan\n")?;

        let discovery = discover_claude_sessions(
            &ClaudeSourceConfig {
                enabled: true,
                home: root.join("claude"),
                include_subagents: true,
                projects_root,
            },
            &BTreeSet::from([canonical_project_root]),
            &mut warnings,
        )?;

        assert_eq!(discovery.sessions.len(), 1);
        assert_eq!(discovery.auxiliary.len(), 1);
        assert_eq!(discovery.auxiliary[0].parent_session, session_id);
        assert!(warnings.is_empty());

        Ok(())
    }

    #[test]
    fn codex_duplicate_resolution_prefers_parseable_then_larger_then_newer() {
        let matching = CodexCandidate {
            session_id: "id".into(),
            source_path: PathBuf::from("/tmp/a"),
            archive_path: PathBuf::from("codex/a.jsonl"),
            meta: Some(CodexSessionMeta {
                id: "id".into(),
                cwd: PathBuf::from("/tmp/project"),
            }),
            matches_project: true,
            size: 10,
            mtime_ms: 20,
        };
        let larger = CodexCandidate {
            size: 20,
            mtime_ms: 10,
            ..matching.clone()
        };
        let broken = CodexCandidate {
            meta: None,
            size: 100,
            ..matching.clone()
        };

        let candidates = [matching, larger.clone(), broken];
        let winner = select_codex_candidate(&candidates).expect("winner");

        assert_eq!(winner.size, larger.size);
    }

    #[test]
    fn utc_formatter_handles_unix_epoch() -> Result<()> {
        let formatted = format_system_time_utc(UNIX_EPOCH)?;

        assert_eq!(formatted, "1970-01-01T00:00:00Z");

        Ok(())
    }

    #[test]
    fn replace_existing_file_swaps_in_new_content() -> Result<()> {
        let dir = unique_test_dir("replace-existing");
        let destination = dir.join("session.jsonl");
        let temp_path = dir.join("session.jsonl.tmp");
        fs::create_dir_all(&dir)?;
        write_file(&destination, "old\n")?;
        write_file(&temp_path, "new\n")?;

        replace_existing_file(
            &temp_path,
            &destination,
            std::io::Error::other("simulated rename failure"),
        )?;

        assert_eq!(fs::read_to_string(&destination)?, "new\n");
        assert!(!temp_path.exists());

        Ok(())
    }

    #[test]
    fn replace_existing_file_restores_destination_on_failure() -> Result<()> {
        let dir = unique_test_dir("replace-restore");
        let destination = dir.join("session.jsonl");
        let missing_temp = dir.join("missing.tmp");
        fs::create_dir_all(&dir)?;
        write_file(&destination, "old\n")?;

        let error = replace_existing_file(
            &missing_temp,
            &destination,
            std::io::Error::other("simulated rename failure"),
        )
        .expect_err("replacement should fail when temp path is missing");

        assert!(
            error
                .to_string()
                .contains("after moving the existing file aside")
        );
        assert_eq!(fs::read_to_string(&destination)?, "old\n");

        Ok(())
    }

    #[test]
    fn unique_sibling_path_is_distinct_per_call() {
        let path = Path::new("/tmp/session.jsonl");
        let first = unique_sibling_path(path, "tmp");
        let second = unique_sibling_path(path, "tmp");
        let backup = unique_sibling_path(path, "bak");

        assert_ne!(first, second);
        assert_ne!(first, backup);
        assert_ne!(second, backup);
    }

    #[test]
    fn prepare_sync_rewrites_known_paths_without_primary_root() -> Result<()> {
        let workspace = unique_test_dir("sync-known-path-cleanup");
        let project_root = workspace.join("repo");
        let memstack_root = workspace.join("memstack");
        let claude_home = workspace.join("claude");
        let claude_projects = claude_home.join("projects");
        let codex_home = workspace.join("codex");
        let codex_sessions_root = codex_home.join("sessions");
        fs::create_dir_all(&project_root)?;
        fs::create_dir_all(&claude_projects)?;
        fs::create_dir_all(&codex_sessions_root)?;
        let canonical_project_root = fs::canonicalize(&project_root)?;

        let mut config = sample_config(
            &memstack_root,
            &project_root,
            &claude_home,
            &claude_projects,
            &codex_home,
            &codex_sessions_root,
        );
        config.projects[0].known_paths = vec![canonical_project_root.clone()];
        fs::create_dir_all(&memstack_root)?;
        write_file(
            &memstack_root.join(CONFIG_FILE_NAME),
            &toml::to_string_pretty(&config)?,
        )?;

        let plan = prepare_sync_from(&project_root, memstack_root.clone(), SyncOptions::default())?;

        assert!(plan.new_known_paths.is_empty());
        assert!(plan.config_written);

        let report = execute_sync(plan)?;

        assert!(report.new_known_paths.is_empty());
        assert!(report.config_written);

        let config_after = load_config(&memstack_root.join(CONFIG_FILE_NAME))?;
        assert!(config_after.projects[0].known_paths.is_empty());

        Ok(())
    }

    #[test]
    fn prepare_and_execute_sync_copies_sessions_and_updates_manifest() -> Result<()> {
        let workspace = unique_test_dir("sync-exec");
        let claude_session_id = "885a05b8-f731-4fde-bfdb-a24ce28dc9c3";
        let project_root = workspace.join("repo");
        let memstack_root = workspace.join("memstack");
        let claude_home = workspace.join("claude");
        let claude_projects = claude_home.join("projects");
        let codex_home = workspace.join("codex");
        let codex_sessions_root = codex_home.join("sessions");
        let codex_sessions = codex_sessions_root.join("2026/03/31");
        let codex_archived = codex_home.join("archived_sessions");
        fs::create_dir_all(&project_root)?;
        fs::create_dir_all(&claude_projects)?;
        fs::create_dir_all(&codex_sessions)?;
        fs::create_dir_all(&codex_archived)?;
        let canonical_project_root = fs::canonicalize(&project_root)?;

        let config = sample_config(
            &memstack_root,
            &project_root,
            &claude_home,
            &claude_projects,
            &codex_home,
            &codex_sessions_root,
        );
        fs::create_dir_all(&memstack_root)?;
        write_file(
            &memstack_root.join(CONFIG_FILE_NAME),
            &toml::to_string_pretty(&config)?,
        )?;

        let encoded = encode_path_for_claude(&canonical_project_root);
        let claude_dir = claude_projects.join(encoded);
        let claude_session = claude_dir.join(format!("{claude_session_id}.jsonl"));
        let claude_aux = claude_dir.join(format!("{claude_session_id}/subagents/agent-a1.jsonl"));
        write_file(&claude_session, "{\"type\":\"message\"}\n")?;
        write_file(&claude_aux, "{\"type\":\"subagent\"}\n")?;

        let rollout_name = "rollout-2026-03-31T11-24-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl";
        let active_rollout = codex_sessions.join(rollout_name);
        let archived_rollout = codex_archived.join(rollout_name);
        write_file(
            &active_rollout,
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\"}}}}\n",
                canonical_project_root.display()
            ),
        )?;
        write_file(
            &archived_rollout,
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"message\"}}\n",
                canonical_project_root.display()
            ),
        )?;

        let plan = prepare_sync_from(&project_root, memstack_root.clone(), SyncOptions::default())?;

        assert_eq!(plan.sessions_to_copy, 2);
        assert_eq!(plan.auxiliary_to_copy, 1);
        assert!(plan.new_known_paths.is_empty());

        let report = execute_sync(plan)?;

        assert_eq!(report.sessions_copied, 2);
        assert_eq!(report.auxiliary_copied, 1);
        assert!(report.manifest_written);
        assert!(!report.config_written);

        let manifest_path = memstack_root.join("projects/memstack-abc123/sessions/.manifest.json");
        let manifest: Manifest = serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;
        let claude_entry = manifest
            .sessions
            .get(claude_session_id)
            .context("missing Claude manifest entry")?;
        assert_eq!(
            claude_entry.archive_path,
            PathBuf::from(format!(
                "claude/{claude_session_id}/{claude_session_id}.jsonl"
            ))
        );
        let codex_entry = manifest
            .sessions
            .get("019d3415-0b9c-7dc3-88e0-e9cb7a789e3f")
            .context("missing Codex manifest entry")?;
        assert_eq!(codex_entry.provider, SourceKind::Codex);
        assert_eq!(
            codex_entry.source_path, archived_rollout,
            "duplicate resolution should keep the larger archived rollout",
        );
        assert!(memstack_root
            .join(format!(
                "projects/memstack-abc123/sessions/claude/{claude_session_id}/{claude_session_id}.jsonl"
            ))
            .exists());
        assert!(memstack_root
            .join(format!(
                "projects/memstack-abc123/sessions/claude/{claude_session_id}/subagents/agent-a1.jsonl"
            ))
            .exists());

        let config_after = load_config(&memstack_root.join(CONFIG_FILE_NAME))?;
        assert!(config_after.projects[0].known_paths.is_empty());

        let second_plan = prepare_sync_from(&project_root, memstack_root, SyncOptions::default())?;
        assert_eq!(second_plan.sessions_to_copy, 0);
        assert_eq!(second_plan.auxiliary_to_copy, 0);
        assert_eq!(second_plan.sessions_unchanged, 2);
        assert_eq!(second_plan.auxiliary_unchanged, 1);
        assert!(!second_plan.manifest_written);
        assert!(!second_plan.config_written);

        Ok(())
    }

    #[test]
    fn prepare_sync_learns_codex_checkout_with_same_upstream() -> Result<()> {
        let workspace = unique_test_dir("sync-codex-known-path");
        let remote = "https://example.com/acme/memstack.git";
        let project_root = workspace.join("repo-a");
        let related_root = workspace.join("repo-b");
        let related_subdir = related_root.join("nested");
        let memstack_root = workspace.join("memstack");
        let claude_home = workspace.join("claude");
        let claude_projects = claude_home.join("projects");
        let codex_home = workspace.join("codex");
        let codex_sessions_root = codex_home.join("sessions");
        let codex_sessions = codex_sessions_root.join("2026/04/01");
        fs::create_dir_all(&claude_projects)?;
        fs::create_dir_all(&codex_sessions)?;
        init_git_repo(&project_root, remote)?;
        init_git_repo(&related_root, remote)?;
        fs::create_dir_all(&related_subdir)?;
        let canonical_project_root = fs::canonicalize(&project_root)?;
        let canonical_related_root = fs::canonicalize(&related_root)?;

        let mut config = sample_config(
            &memstack_root,
            &project_root,
            &claude_home,
            &claude_projects,
            &codex_home,
            &codex_sessions_root,
        );
        config.projects[0].git_upstream = Some(remote.into());
        fs::create_dir_all(&memstack_root)?;
        write_file(
            &memstack_root.join(CONFIG_FILE_NAME),
            &toml::to_string_pretty(&config)?,
        )?;

        let rollout_name = "rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl";
        write_file(
            &codex_sessions.join(rollout_name),
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"message\"}}\n",
                related_subdir.display()
            ),
        )?;

        let plan = prepare_sync_from(&project_root, memstack_root.clone(), SyncOptions::default())?;

        assert_eq!(plan.project_root, canonical_project_root);
        assert_eq!(plan.sessions_to_copy, 1);
        assert_eq!(plan.new_known_paths, vec![canonical_related_root.clone()]);

        let report = execute_sync(plan)?;

        assert_eq!(report.new_known_paths, vec![canonical_related_root.clone()]);

        let config_after = load_config(&memstack_root.join(CONFIG_FILE_NAME))?;
        assert_eq!(
            config_after.projects[0].known_paths,
            vec![canonical_related_root]
        );

        Ok(())
    }
}
