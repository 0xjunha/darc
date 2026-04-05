use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::types::SourceKind;

const MANIFEST_VERSION: u32 = 1;

/// Stores source and destination metadata for a pending copy.
#[derive(Debug, Clone)]
pub(crate) struct PendingCopy {
    pub(crate) source_path: PathBuf,
    pub(crate) destination_path: PathBuf,
}

/// Stores the private write operations captured during sync planning.
#[derive(Debug, Clone)]
pub(crate) struct PreparedSyncWrites {
    pub(crate) manifest_written: bool,
    pub(crate) manifest_path: PathBuf,
    pub(crate) manifest: Manifest,
    pub(crate) session_copies: Vec<PendingCopy>,
    pub(crate) auxiliary_copies: Vec<PendingCopy>,
}

/// Loads the on-disk manifest or returns an empty v1 manifest.
pub(crate) fn load_manifest(path: &Path) -> Result<Manifest> {
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

/// Describes one discovered artifact that can update the sync manifest.
pub(crate) trait ManifestArtifact {
    type Entry;
    type Key: Clone + Ord;

    /// Returns the manifest key for one discovered artifact.
    fn key(&self) -> &Self::Key;

    /// Returns whether the discovered artifact still needs copying.
    fn should_copy(&self, entry: &Self::Entry) -> bool;

    /// Returns whether the manifest entry still matches the discovered artifact.
    fn matches_entry(&self, entry: &Self::Entry) -> bool;

    /// Builds the manifest entry for one discovered artifact.
    fn manifest_entry(&self, synced_at: &str) -> Self::Entry;

    /// Builds a pending copy for one discovered artifact.
    fn into_pending_copy(self, sessions_root: &Path) -> PendingCopy;
}

/// Plans manifest updates and file copies for one discovered artifact list.
pub(crate) fn plan_manifest_updates<T>(
    discovered: Vec<T>,
    manifest_entries: &mut BTreeMap<T::Key, T::Entry>,
    synced_at: &str,
    sessions_root: &Path,
    manifest_written: &mut bool,
) -> (Vec<PendingCopy>, usize)
where
    T: ManifestArtifact,
{
    let mut copies = Vec::new();
    let mut unchanged = 0;

    for artifact in discovered {
        let key = artifact.key().clone();
        let existing = manifest_entries.get(&key);
        let should_copy = existing.is_none_or(|entry| artifact.should_copy(entry));
        if !should_copy {
            unchanged += 1;
        }
        if should_copy || existing.is_some_and(|entry| !artifact.matches_entry(entry)) {
            *manifest_written = true;
            manifest_entries.insert(key, artifact.manifest_entry(synced_at));
        }
        if should_copy {
            copies.push(artifact.into_pending_copy(sessions_root));
        }
    }

    (copies, unchanged)
}

/// Mirrors the on-disk sync manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) sessions: BTreeMap<String, ManifestSessionEntry>,
    #[serde(default)]
    pub(crate) auxiliary: BTreeMap<String, ManifestAuxiliaryEntry>,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ManifestSessionEntry {
    pub(crate) provider: SourceKind,
    pub(crate) source_path: PathBuf,
    pub(crate) archive_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) size: u64,
    pub(crate) mtime_ms: u64,
    pub(crate) synced_at: String,
}

impl ManifestSessionEntry {
    /// Returns whether one stored entry matches the discovered session metadata except `synced_at`.
    pub(crate) fn matches(
        &self,
        provider: SourceKind,
        source_path: &Path,
        archive_path: &Path,
        cwd: Option<&Path>,
        size: u64,
        mtime_ms: u64,
    ) -> bool {
        self.provider == provider
            && self.source_path == source_path
            && self.archive_path == archive_path
            && self.cwd.as_deref() == cwd
            && self.size == size
            && self.mtime_ms == mtime_ms
    }
}

/// Stores one auxiliary manifest entry keyed by source path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ManifestAuxiliaryEntry {
    pub(crate) parent_session: String,
    pub(crate) archive_path: PathBuf,
    pub(crate) size: u64,
    pub(crate) mtime_ms: u64,
    pub(crate) synced_at: String,
}

impl ManifestAuxiliaryEntry {
    /// Returns whether one stored entry matches the discovered auxiliary metadata except `synced_at`.
    pub(crate) fn matches(
        &self,
        parent_session: &str,
        archive_path: &Path,
        size: u64,
        mtime_ms: u64,
    ) -> bool {
        self.parent_session == parent_session
            && self.archive_path == archive_path
            && self.size == size
            && self.mtime_ms == mtime_ms
    }
}
