use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};
use darc_paths::{
    current_project_root, encode_path_for_claude, normalize_project_path, try_git_output,
};
use darc_rollout::codex::{
    CodexRolloutSessionMeta, compare_rollout_priority, read_rollout_session_meta,
    reconcile_rollout_session_id,
};
use walkdir::WalkDir;

use crate::{
    manifest::{
        ManifestArtifact, ManifestAuxiliaryEntry, ManifestSessionEntry, PendingCopy,
        PreparedSyncWrites, load_manifest, plan_manifest_updates,
    },
    types::{ClaudeSource, CodexSource, SourceKind, SyncRequest},
    utils::{copy_file_atomically, file_snapshot, format_system_time_utc, write_json_atomically},
};

/// Describes a prepared sync before any writes happen.
#[derive(Debug, Clone)]
pub struct SyncPlan {
    pub project_name: String,
    pub project_root: PathBuf,
    pub sessions_root: PathBuf,
    pub sources: Vec<SourceKind>,
    pub sessions_unchanged: usize,
    pub auxiliary_unchanged: usize,
    pub new_known_paths: Vec<PathBuf>,
    pub warnings: Vec<String>,
    persisted_known_paths: Vec<PathBuf>,
    writes: PreparedSyncWrites,
}

impl SyncPlan {
    /// Returns how many parent session files this sync would copy.
    pub fn sessions_to_copy(&self) -> usize {
        self.writes.session_copies.len()
    }

    /// Returns how many auxiliary files this sync would copy.
    pub fn auxiliary_to_copy(&self) -> usize {
        self.writes.auxiliary_copies.len()
    }

    /// Returns whether executing this plan would rewrite the manifest.
    pub fn manifest_written(&self) -> bool {
        self.writes.manifest_written
    }

    /// Returns the known paths that should be persisted after a successful sync.
    pub fn persisted_known_paths(&self) -> &[PathBuf] {
        &self.persisted_known_paths
    }
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
}

/// Plans a sync from explicit project and source inputs.
pub fn prepare_sync(request: SyncRequest) -> Result<SyncPlan> {
    let manifest_path = request.sessions_root.join(".manifest.json");
    let mut manifest = load_manifest(&manifest_path)?;
    let synced_at = format_system_time_utc(SystemTime::now())?;
    let mut warnings = Vec::new();

    let mut discovered_sessions = Vec::new();
    let mut discovered_auxiliary = Vec::new();
    let mut persisted_known_paths = request.project_paths.clone();
    persisted_known_paths.remove(&request.primary_project_path);

    for source in &request.sources {
        match source {
            SourceKind::Claude => {
                let claude = request
                    .claude
                    .as_ref()
                    .context("Claude source was selected but no Claude config was provided")?;
                let discovery =
                    discover_claude_sessions(claude, &request.project_paths, &mut warnings)?;
                discovered_sessions.extend(discovery.sessions);
                discovered_auxiliary.extend(discovery.auxiliary);
            }
            SourceKind::Codex => {
                let codex = request
                    .codex
                    .as_ref()
                    .context("Codex source was selected but no Codex config was provided")?;
                let sessions = discover_codex_sessions(
                    codex,
                    &request.project_paths,
                    request.project_upstream.as_deref(),
                    &request.other_project_paths,
                    &mut warnings,
                )?;
                for session in &sessions {
                    if let Some(path) = &session.cwd
                        && path != &request.primary_project_path
                    {
                        persisted_known_paths.insert(path.clone());
                    }
                }
                discovered_sessions.extend(sessions);
            }
        }
    }

    let mut manifest_written = false;
    let (session_copies, sessions_unchanged) = plan_manifest_updates(
        discovered_sessions,
        &mut manifest.sessions,
        &synced_at,
        &request.sessions_root,
        &mut manifest_written,
    );
    let (auxiliary_copies, auxiliary_unchanged) = plan_manifest_updates(
        discovered_auxiliary,
        &mut manifest.auxiliary,
        &synced_at,
        &request.sessions_root,
        &mut manifest_written,
    );

    let new_known_paths = persisted_known_paths
        .difference(&request.stored_known_paths)
        .cloned()
        .collect::<Vec<_>>();

    Ok(SyncPlan {
        project_name: request.project_name,
        project_root: request.project_root,
        sessions_root: request.sessions_root,
        sources: request.sources,
        sessions_unchanged,
        auxiliary_unchanged,
        new_known_paths,
        warnings,
        persisted_known_paths: persisted_known_paths.into_iter().collect(),
        writes: PreparedSyncWrites {
            manifest_written,
            manifest_path,
            manifest,
            session_copies,
            auxiliary_copies,
        },
    })
}

/// Executes a prepared sync by copying files and atomically updating metadata.
pub fn execute_sync(plan: SyncPlan) -> Result<SyncReport> {
    let SyncPlan {
        project_name,
        project_root,
        sessions_root,
        sources,
        sessions_unchanged,
        auxiliary_unchanged,
        new_known_paths,
        warnings,
        persisted_known_paths: _persisted_known_paths,
        writes,
    } = plan;
    let PreparedSyncWrites {
        manifest_written,
        manifest_path,
        manifest,
        session_copies,
        auxiliary_copies,
    } = writes;

    for copy in session_copies.iter().chain(&auxiliary_copies) {
        copy_file_atomically(&copy.source_path, &copy.destination_path)?;
    }
    if manifest_written {
        write_json_atomically(&manifest_path, &manifest)?;
    }

    Ok(SyncReport {
        project_name,
        project_root,
        sessions_root,
        sources,
        sessions_copied: session_copies.len(),
        sessions_unchanged,
        auxiliary_copied: auxiliary_copies.len(),
        auxiliary_unchanged,
        new_known_paths,
        warnings,
        manifest_written,
    })
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

/// Captures supported Claude discovery results.
#[derive(Debug, Default)]
pub(crate) struct ClaudeDiscovery {
    pub(crate) sessions: Vec<DiscoveredSession>,
    pub(crate) auxiliary: Vec<DiscoveredAuxiliary>,
}

/// Stores one discovered parent session before manifest diffing.
#[derive(Debug, Clone)]
pub(crate) struct DiscoveredSession {
    pub(crate) id: String,
    pub(crate) provider: SourceKind,
    pub(crate) source_path: PathBuf,
    pub(crate) archive_path: PathBuf,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) size: u64,
    pub(crate) mtime_ms: u64,
}

impl ManifestArtifact for DiscoveredSession {
    type Entry = ManifestSessionEntry;
    type Key = String;

    /// Returns the manifest key for one discovered session.
    fn key(&self) -> &Self::Key {
        &self.id
    }

    /// Returns whether the discovered session still needs copying.
    fn should_copy(&self, entry: &Self::Entry) -> bool {
        entry.size != self.size || entry.mtime_ms != self.mtime_ms
    }

    /// Returns whether the manifest entry still matches the discovered session.
    fn matches_entry(&self, entry: &Self::Entry) -> bool {
        entry.matches(
            self.provider,
            &self.source_path,
            &self.archive_path,
            self.cwd.as_deref(),
            self.size,
            self.mtime_ms,
        )
    }

    /// Builds the manifest entry for one discovered session.
    fn manifest_entry(&self, synced_at: &str) -> Self::Entry {
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

    /// Builds a pending copy for one discovered session.
    fn into_pending_copy(self, sessions_root: &Path) -> PendingCopy {
        PendingCopy {
            source_path: self.source_path,
            destination_path: sessions_root.join(self.archive_path),
        }
    }
}

/// Discovers Claude parent sessions and auxiliary files for the project path set.
pub(crate) fn discover_claude_sessions(
    source: &ClaudeSource,
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
pub(crate) fn matching_claude_dirs(
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

/// Stores one discovered Claude auxiliary artifact before manifest diffing.
#[derive(Debug, Clone)]
pub(crate) struct DiscoveredAuxiliary {
    pub(crate) key: String,
    pub(crate) source_path: PathBuf,
    pub(crate) archive_path: PathBuf,
    pub(crate) parent_session: String,
    pub(crate) size: u64,
    pub(crate) mtime_ms: u64,
}

impl ManifestArtifact for DiscoveredAuxiliary {
    type Entry = ManifestAuxiliaryEntry;
    type Key = String;

    /// Returns the manifest key for one discovered auxiliary file.
    fn key(&self) -> &Self::Key {
        &self.key
    }

    /// Returns whether the discovered auxiliary file still needs copying.
    fn should_copy(&self, entry: &Self::Entry) -> bool {
        entry.size != self.size || entry.mtime_ms != self.mtime_ms
    }

    /// Returns whether the manifest entry still matches the discovered auxiliary file.
    fn matches_entry(&self, entry: &Self::Entry) -> bool {
        entry.matches(
            &self.parent_session,
            &self.archive_path,
            self.size,
            self.mtime_ms,
        )
    }

    /// Builds the manifest entry for one discovered auxiliary file.
    fn manifest_entry(&self, synced_at: &str) -> Self::Entry {
        ManifestAuxiliaryEntry {
            parent_session: self.parent_session.clone(),
            archive_path: self.archive_path.clone(),
            size: self.size,
            mtime_ms: self.mtime_ms,
            synced_at: synced_at.to_owned(),
        }
    }

    /// Builds a pending copy for one discovered auxiliary file.
    fn into_pending_copy(self, sessions_root: &Path) -> PendingCopy {
        PendingCopy {
            source_path: self.source_path,
            destination_path: sessions_root.join(self.archive_path),
        }
    }
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
    let ext = path.extension().and_then(|e| e.to_str());
    ext == Some("jsonl")
        || ext == Some("txt")
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".meta.json"))
}

/// Stores one Codex rollout candidate before duplicate resolution.
#[derive(Debug, Clone)]
pub(crate) struct CodexCandidate {
    pub(crate) session_id: String,
    pub(crate) source_path: PathBuf,
    pub(crate) archive_path: PathBuf,
    pub(crate) repo_root: PathBuf,
    pub(crate) matches_project: bool,
    pub(crate) size: u64,
    pub(crate) mtime_ms: u64,
}

/// Discovers Codex rollout files, matches them by cwd, and resolves duplicates by logical session id.
fn discover_codex_sessions(
    source: &CodexSource,
    project_paths: &BTreeSet<PathBuf>,
    project_upstream: Option<&str>,
    other_project_paths: &BTreeSet<PathBuf>,
    warnings: &mut Vec<String>,
) -> Result<Vec<DiscoveredSession>> {
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
            if !file_name.starts_with("rollout-") || !file_name.ends_with(".jsonl") {
                continue;
            }
            let snapshot = match file_snapshot(path) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    warnings.push(format!("skipped {}: {error:#}", path.display()));
                    continue;
                }
            };

            let (session_id, repo_root, matches_project) = match extract_codex_session_meta(path) {
                Ok(Some(meta)) => {
                    let session_id = match reconcile_rollout_session_id(
                        path,
                        Some(file_name),
                        Some(meta.session_id.as_str()),
                    ) {
                        Ok(Some(session_id)) => session_id,
                        Ok(None) => {
                            warnings.push(format!(
                                "skipped Codex session without filename or payload id in {}",
                                path.display()
                            ));
                            continue;
                        }
                        Err(error) => {
                            warnings.push(error.to_string());
                            continue;
                        }
                    };
                    let repo_root = repo_cache.repo_root(&meta.cwd);
                    if !project_paths.contains(&repo_root)
                        && other_project_paths.contains(&repo_root)
                    {
                        continue;
                    }
                    let matches_project = codex_session_matches_project(
                        &repo_root,
                        project_paths,
                        project_upstream,
                        &mut repo_cache,
                    );
                    (session_id, repo_root, matches_project)
                }
                Ok(None) => {
                    warnings.push(format!(
                        "skipped Codex session meta match for {}: first line was not session_meta",
                        path.display()
                    ));
                    continue;
                }
                Err(error) => {
                    warnings.push(format!(
                        "failed to parse Codex session meta from {}: {error:#}",
                        path.display()
                    ));
                    continue;
                }
            };

            candidates
                .entry(session_id.clone())
                .or_default()
                .push(CodexCandidate {
                    session_id,
                    source_path: path.to_path_buf(),
                    archive_path: PathBuf::from(SourceKind::Codex.directory_name()).join(file_name),
                    repo_root,
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
            cwd: Some(candidate.repo_root.clone()),
            size: candidate.size,
            mtime_ms: candidate.mtime_ms,
        })
        .collect();

    Ok(sessions)
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
pub(crate) fn select_codex_candidate(candidates: &[CodexCandidate]) -> Option<CodexCandidate> {
    candidates
        .iter()
        .filter(|candidate| candidate.matches_project)
        .max_by(|left, right| {
            compare_rollout_priority(
                left.size,
                left.mtime_ms,
                &left.source_path,
                right.size,
                right.mtime_ms,
                &right.source_path,
            )
        })
        .cloned()
}

/// Extracts the Codex `session_meta` id and cwd from the first rollout line.
pub(crate) fn extract_codex_session_meta(path: &Path) -> Result<Option<CodexRolloutSessionMeta>> {
    read_rollout_session_meta(path)
}
