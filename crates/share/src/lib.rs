//! Git-backed encrypted sharing for redacted Darc index projections.

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    str::FromStr,
    thread,
};

use age::{
    secrecy::ExposeSecret,
    x25519::{Identity, Recipient},
};
use anyhow::{Context, Result, bail};
use darc_paths::current_utc_timestamp;
#[cfg(test)]
use darc_store::ShareSessionExport;
use darc_store::{
    SharePolicy, ShareSessionExportState, ShareState, ShareTurnExport, ShareTurnImport,
    ShareUserRecord, clear_project_share_states, import_shared_turns, open_index_database_writer,
    prune_shared_turns, query_share_export_session_states, query_share_export_turns,
    query_share_status, set_project_share_policy, set_session_share_state,
};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod artifact;
mod crypto;
mod export;
mod git;
mod import;
mod remote;
mod util;
mod workflow;

#[cfg(test)]
mod tests;

use artifact::*;
use crypto::*;
use export::*;
use git::*;
use import::*;
use remote::*;
pub use remote::{sanitize_git_url_for_display, validate_share_remote_url};
use util::*;
#[cfg(test)]
use workflow::ensure_share_signing_key;
pub use workflow::{
    add_recipient, ensure_share_key, exclude_all_sessions, fetch_share_branch,
    include_all_sessions, local_share_identity, merge_share_branch, pull_share_branch,
    pull_share_branch_with_progress, push_share_branch, push_share_branch_with_progress,
    remove_recipient, share_git_branch, share_status, update_session_share_state,
    update_share_policy, upsert_remote, validate_share_recipient,
};

const ARTIFACT_ROOT: &str = "darc-share/v1";
const PROJECT_SCHEMA: &str = "darc.share.project.v1";
const MANIFEST_SCHEMA: &str = "darc.share.manifest.v1";
const TURN_PAYLOAD_SCHEMA: &str = "darc.share.turn.v1";
const CHUNK_PAYLOAD_SCHEMA: &str = "darc.share.chunk.v1";
const SYNC_PAYLOAD_SCHEMA: &str = "darc.share.sync.v1";
const CHUNK_PAYLOAD_VERSION: u32 = 1;
const SYNC_PAYLOAD_VERSION: u32 = 2;
const GIT_ATTRIBUTES_FILE: &str = ".gitattributes";
const LEGACY_MANIFEST_FILE: &str = "manifest.json";
const PROJECT_FILE: &str = "project.json";
const EXPORTERS_DIR: &str = "exporters";
const KEY_FILE_NAME: &str = "share.agekey";
const SIGNING_KEY_FILE_NAME: &str = "share.signingkey";
const SHARE_CACHE_DIR: &str = "share-cache";
const TRUSTED_OBJECT_CACHE_DIR: &str = "darc-object-cache";
const DEFAULT_REMOTE_NAME: &str = "origin";
const MAX_SHARE_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
#[cfg(not(test))]
const MAX_CACHED_SHARE_MANIFESTS: usize = 1024;
#[cfg(test)]
const MAX_CACHED_SHARE_MANIFESTS: usize = 8;
#[cfg(not(test))]
const MAX_CACHED_SHARE_EXPORTER_DIRS: usize = 4096;
#[cfg(test)]
const MAX_CACHED_SHARE_EXPORTER_DIRS: usize = 16;
#[cfg(not(test))]
const MAX_CACHED_SHARE_MANIFEST_BYTES: u64 = 128 * 1024 * 1024;
#[cfg(test)]
const MAX_CACHED_SHARE_MANIFEST_BYTES: u64 = 16 * 1024;
const MAX_SHARE_OBJECT_BYTES: u64 = 256 * 1024 * 1024;
#[cfg(not(test))]
const MAX_SHARE_CHUNK_DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;
#[cfg(test)]
const MAX_SHARE_CHUNK_DECOMPRESSED_BYTES: u64 = 64 * 1024;
const MAX_SHARE_EXPORT_OBJECTS: usize = 100_000;
const MAX_SHARE_EXPORT_BYTES: usize = 8 * 1024 * 1024 * 1024;
#[cfg(not(test))]
const SHARE_CHUNK_TARGET_BYTES: usize = 64 * 1024 * 1024;
#[cfg(test)]
const SHARE_CHUNK_TARGET_BYTES: usize = 4 * 1024;
const TURN_SIGNATURE_DOMAIN: &[u8] = b"darc.share.turn.signature.v1";
const SYNC_SIGNATURE_DOMAIN: &[u8] = b"darc.share.sync.signature.v1";
const GIT_LFS_POINTER_PREFIX: &[u8] = b"version https://git-lfs.github.com/spec/v1\n";

/// Stores one active project resolved by Darc core for sharing.
#[derive(Debug, Clone)]
pub struct ShareProjectContext {
    pub root: PathBuf,
    pub index_db_path: PathBuf,
    pub project_id: String,
    pub project_name: String,
    pub local_path: PathBuf,
    pub git_upstream: Option<String>,
}

/// Stores one configured Darc share remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareRemote {
    pub name: String,
    pub url: String,
}

/// Stores one configured Darc encryption recipient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareRecipient {
    pub recipient: String,
}

/// Stores share settings loaded from Darc config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShareSettings {
    pub remotes: Vec<ShareRemote>,
    pub recipients: Vec<ShareRecipient>,
}

/// Stores one local Git identity used for share authorship.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareIdentity {
    pub user_id: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub public_key: String,
    #[serde(default)]
    pub signing_public_key: String,
}

/// Stores one share key pair loaded from the Darc root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareKeyInfo {
    pub key_path: PathBuf,
    pub public_key: String,
}

/// Stores one share branch push report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SharePushReport {
    pub branch: String,
    pub git_branch: String,
    pub remote_name: String,
    pub remote_url: String,
    pub project_key: String,
    pub exported_turn_count: u64,
    pub exported_session_count: u64,
    pub object_count: u64,
    pub commit_id: String,
    pub pushed: bool,
}

/// Stores one share branch fetch report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShareFetchReport {
    pub branch: String,
    pub git_branch: String,
    pub remote_name: String,
    pub remote_url: String,
    pub fetched: bool,
}

/// Stores one share branch merge/import report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShareMergeReport {
    pub branch: String,
    pub git_branch: String,
    pub remote_name: String,
    pub project_key: String,
    pub imported_turn_count: u64,
    pub skipped_turn_count: u64,
    pub warning_count: u64,
    pub warnings: Vec<String>,
}

/// Stores one pull report containing fetch and merge phases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SharePullReport {
    pub fetch: ShareFetchReport,
    pub merge: ShareMergeReport,
}

/// Identifies one Git upload phase during a share push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShareUploadKind {
    Lfs,
    Git,
}

/// Describes one progress event emitted while pushing a share branch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SharePushProgress {
    Started {
        git_branch: String,
        remote_name: String,
        remote_url: String,
    },
    PreparingCache,
    FetchingRemote,
    HydratingLfs,
    ReadingCache,
    ReusingPreviousExport {
        exported_turn_count: u64,
        exported_session_count: u64,
    },
    BuildingExport {
        total_turns: u64,
    },
    ExportingTurns {
        exported_turns: u64,
        total_turns: u64,
    },
    ExportingSessions {
        exported_sessions: u64,
        total_sessions: u64,
    },
    WritingMetadata {
        object_count: u64,
    },
    Committing,
    Uploading {
        kind: ShareUploadKind,
    },
    GitProgress {
        kind: ShareUploadKind,
        message: String,
    },
    Finished {
        commit_id: String,
    },
}

/// Describes one progress event emitted while pulling a share branch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SharePullProgress {
    Started {
        git_branch: String,
        remote_name: String,
        remote_url: String,
    },
    PreparingCache,
    FetchingRemote,
    HydratingLfs,
    ReadingCache,
    ImportingSessions {
        processed_sessions: u64,
        total_sessions: u64,
    },
    Finished {
        imported_turn_count: u64,
        skipped_turn_count: u64,
        warning_count: u64,
    },
}

/// Stores one visible project artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProjectArtifact {
    schema: String,
    version: u32,
    project_key: String,
    project_name: String,
    updated_at: String,
}

/// Stores one visible manifest artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ManifestArtifact {
    schema: String,
    version: u32,
    project_key: String,
    branch: String,
    exported_at: String,
    exporter: ShareIdentity,
    sync: SyncManifestEntry,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    chunks: Vec<ChunkManifestEntry>,
    turns: Vec<TurnManifestEntry>,
}

/// Stores one visible encrypted sync object reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SyncManifestEntry {
    payload_hash: String,
    object_path: String,
}

/// Stores one visible encrypted turn object reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TurnManifestEntry {
    provider: darc_paths::SourceKind,
    session_id: String,
    turn_ordinal: i64,
    started_at: String,
    payload_hash: String,
    object_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chunk_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chunk_record_index: Option<u32>,
}

/// Stores one visible encrypted chunk object reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ChunkManifestEntry {
    chunk_id: String,
    object_path: String,
    compression: String,
    plaintext_hash: String,
    ciphertext_hash: String,
    plaintext_bytes: u64,
    ciphertext_bytes: u64,
    turn_count: u64,
}

/// Stores one encrypted per-turn payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EncryptedTurnPayload {
    schema: String,
    version: u32,
    project_key: String,
    exporter: ShareIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    turn: ShareTurnExport,
}

/// Stores one compressed chunk plaintext before age encryption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ShareChunkPayload {
    schema: String,
    version: u32,
    project_key: String,
    exporter: ShareIdentity,
    chunk_id: String,
    turns: Vec<EncryptedTurnPayload>,
}

/// Stores one encrypted export manifest used to authenticate pruning inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EncryptedSyncPayload {
    schema: String,
    version: u32,
    project_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    export_fingerprint: String,
    exporter: ShareIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sessions: Vec<SyncSessionEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    chunks: Vec<SyncChunkEntry>,
    turns: Vec<SyncTurnEntry>,
}

/// Stores one authenticated exported session identity for fast unchanged-export reuse.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct SyncSessionEntry {
    provider: darc_paths::SourceKind,
    session_id: String,
    source_size: i64,
    source_mtime_ms: i64,
}

/// Stores authenticated chunk metadata for fast unchanged-export reuse.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct SyncChunkEntry {
    chunk_id: String,
    object_path: String,
    compression: String,
    plaintext_hash: String,
    ciphertext_hash: String,
    plaintext_bytes: u64,
    ciphertext_bytes: u64,
    turn_count: u64,
}

/// Stores one authenticated exported turn identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct SyncTurnEntry {
    provider: darc_paths::SourceKind,
    session_id: String,
    turn_ordinal: i64,
    started_at: String,
    payload_hash: String,
    object_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chunk_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chunk_record_index: Option<u32>,
}

/// Builds the authenticated sync entry corresponding to one visible manifest entry.
fn sync_entry_from_manifest(entry: &TurnManifestEntry) -> SyncTurnEntry {
    SyncTurnEntry {
        provider: entry.provider,
        session_id: entry.session_id.clone(),
        turn_ordinal: entry.turn_ordinal,
        started_at: entry.started_at.clone(),
        payload_hash: entry.payload_hash.clone(),
        object_path: entry.object_path.clone(),
        chunk_id: entry.chunk_id.clone(),
        chunk_record_index: entry.chunk_record_index,
    }
}

/// Builds the authenticated sync entry corresponding to one visible chunk entry.
fn sync_chunk_from_manifest(entry: &ChunkManifestEntry) -> SyncChunkEntry {
    SyncChunkEntry {
        chunk_id: entry.chunk_id.clone(),
        object_path: entry.object_path.clone(),
        compression: entry.compression.clone(),
        plaintext_hash: entry.plaintext_hash.clone(),
        ciphertext_hash: entry.ciphertext_hash.clone(),
        plaintext_bytes: entry.plaintext_bytes,
        ciphertext_bytes: entry.ciphertext_bytes,
        turn_count: entry.turn_count,
    }
}

/// Builds an authenticated session entry from one fully materialized export turn.
fn sync_session_entry_from_turn(turn: &ShareTurnExport) -> Option<SyncSessionEntry> {
    Some(SyncSessionEntry {
        provider: turn.session.provider,
        session_id: turn.session.session_id.clone(),
        source_size: turn.session.source_size?,
        source_mtime_ms: turn.session.source_mtime_ms?,
    })
}

/// Builds an authenticated session entry from one lightweight export state row.
fn sync_session_entry_from_state(state: &ShareSessionExportState) -> Option<SyncSessionEntry> {
    Some(SyncSessionEntry {
        provider: state.provider,
        session_id: state.session_id.clone(),
        source_size: state.source_size?,
        source_mtime_ms: state.source_mtime_ms?,
    })
}

/// Hashes the current redacted export rows used by fast unchanged-export reuse.
fn share_export_fingerprint(turns: &[ShareTurnExport]) -> Result<String> {
    let encoded = serde_json::to_vec(turns).context("failed to fingerprint share export rows")?;
    Ok(sha256_hex(&encoded))
}

/// Returns the progress key for one visible manifest session entry.
fn manifest_session_progress_key(
    manifest: &ManifestArtifact,
    entry: &TurnManifestEntry,
) -> ManifestSessionProgressKey {
    (
        exporter_manifest_id(&manifest.exporter),
        entry.provider,
        entry.session_id.clone(),
    )
}

/// Stores one built export artifact and its encrypted object paths.
struct BuiltExportArtifact {
    project: ProjectArtifact,
    manifest: ManifestArtifact,
    #[cfg(test)]
    objects: BTreeMap<String, Vec<u8>>,
    object_paths: BTreeSet<String>,
    exported_turn_count: u64,
    exported_session_count: u64,
    object_count: u64,
}

/// Receives encrypted export objects as they are generated.
enum ExportObjectTarget<'a> {
    #[cfg(test)]
    Memory {
        objects: &'a mut BTreeMap<String, Vec<u8>>,
    },
    Disk {
        cache_path: &'a Path,
    },
}

/// Stores one resolved remote target.
#[derive(Debug, Clone)]
struct ResolvedRemote {
    name: String,
    #[cfg(test)]
    url: String,
    resolved_url: String,
    cache_url: String,
    display_url: String,
}

/// Stores one manifest found in the local share cache.
#[derive(Clone)]
struct CachedManifest {
    relative_path: String,
    manifest: ManifestArtifact,
}

/// Stores cached manifests plus warnings collected while reading them.
struct CachedManifestRead {
    manifests: Vec<CachedManifest>,
    warnings: Vec<String>,
}

/// Stores the inputs needed to build one share export artifact.
struct ExportBuildRequest<'a> {
    context: &'a ShareProjectContext,
    settings: &'a ShareSettings,
    project_key: &'a str,
    identity: &'a ShareIdentity,
    signing_key: &'a SigningKey,
    branch: &'a str,
    turns: Vec<ShareTurnExport>,
}

/// Stores trusted local reuse inputs for stable encrypted export objects.
#[derive(Clone, Copy, Default)]
struct ExportReuseContext<'a> {
    trusted_object_cache_path: Option<&'a Path>,
    decryption_identity: Option<&'a Identity>,
    previous_project: Option<&'a ProjectArtifact>,
    previous_manifest: Option<&'a ManifestArtifact>,
}

/// Stores immutable context used while importing one manifest entry.
struct ImportEntryContext<'a> {
    expected_project_key: &'a str,
    expected_exporter: &'a ShareIdentity,
    identity: &'a Identity,
    cache_path: &'a Path,
    chunks: &'a DecodedChunks,
}

/// Stores immutable inputs for importing one share cache checkout.
struct ImportCacheRequest<'a> {
    context: &'a ShareProjectContext,
    branch: &'a str,
    git_branch: &'a str,
    remote_name: &'a str,
    remote_url: &'a str,
    expected_project_key: &'a str,
    cache_path: &'a Path,
}

/// Stores decoded share chunks plus per-chunk failures.
#[derive(Clone, Default)]
struct DecodedChunks {
    turns: BTreeMap<(String, u32), DecodedChunkTurn>,
    errors: BTreeMap<String, String>,
}

/// Stores one decoded turn payload found inside an encrypted chunk.
#[derive(Clone)]
struct DecodedChunkTurn {
    object_path: String,
    payload_hash: String,
    turn: ShareTurnExport,
}

/// Keys one manifest session for pull import progress accounting.
type ManifestSessionProgressKey = (String, darc_paths::SourceKind, String);

/// Tracks completed manifest sessions while importing a share branch.
struct ImportSessionProgress {
    remaining_turns_by_session: BTreeMap<ManifestSessionProgressKey, usize>,
    processed_sessions: u64,
    total_sessions: u64,
}

impl ImportSessionProgress {
    /// Builds import progress accounting for all visible manifest sessions.
    fn new(manifests: &[CachedManifest]) -> Result<Self> {
        let mut remaining_turns_by_session = BTreeMap::new();
        for cached in manifests {
            for entry in &cached.manifest.turns {
                *remaining_turns_by_session
                    .entry(manifest_session_progress_key(&cached.manifest, entry))
                    .or_insert(0) += 1;
            }
        }
        let total_sessions = u64::try_from(remaining_turns_by_session.len())
            .context("session count exceeds u64 range")?;
        Ok(Self {
            remaining_turns_by_session,
            processed_sessions: 0,
            total_sessions,
        })
    }

    /// Emits the current import progress count.
    fn emit(&self, progress: &mut impl FnMut(SharePullProgress)) {
        progress(SharePullProgress::ImportingSessions {
            processed_sessions: self.processed_sessions,
            total_sessions: self.total_sessions,
        });
    }

    /// Marks every session in a skipped manifest as processed.
    fn finish_manifest(
        &mut self,
        manifest: &ManifestArtifact,
        progress: &mut impl FnMut(SharePullProgress),
    ) {
        for entry in &manifest.turns {
            self.finish_entry(manifest, entry, progress);
        }
    }

    /// Marks every session represented by a processed entry set as processed.
    fn finish_entries(
        &mut self,
        manifest: &ManifestArtifact,
        entries: &[&TurnManifestEntry],
        progress: &mut impl FnMut(SharePullProgress),
    ) {
        for entry in entries {
            self.finish_entry(manifest, entry, progress);
        }
    }

    /// Marks one manifest turn entry as processed for session progress.
    fn finish_entry(
        &mut self,
        manifest: &ManifestArtifact,
        entry: &TurnManifestEntry,
        progress: &mut impl FnMut(SharePullProgress),
    ) {
        let key = manifest_session_progress_key(manifest, entry);
        let Some(remaining_turns) = self.remaining_turns_by_session.get_mut(&key) else {
            return;
        };
        *remaining_turns = remaining_turns.saturating_sub(1);
        if *remaining_turns == 0 {
            self.remaining_turns_by_session.remove(&key);
            self.processed_sessions += 1;
            self.emit(progress);
        }
    }
}

/// Stores one completed system Git command result.
#[derive(Debug)]
struct GitCommandOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

/// Stores one system Git upload command for a share branch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PushBranchCommand {
    kind: ShareUploadKind,
    quiet_args: Vec<OsString>,
    progress_args: Vec<OsString>,
    context: String,
}

/// Tracks completed export sessions independent of turn ordering.
struct ExportSessionProgress {
    remaining_turns_by_session: BTreeMap<(darc_paths::SourceKind, String), usize>,
    exported_sessions: usize,
    total_sessions: usize,
}

impl ExportSessionProgress {
    /// Builds export progress accounting for all selected turns.
    fn new(turns: &[ShareTurnExport]) -> Self {
        let mut remaining_turns_by_session = BTreeMap::new();
        for turn in turns {
            *remaining_turns_by_session
                .entry(share_turn_session_key(turn))
                .or_insert(0) += 1;
        }
        let total_sessions = remaining_turns_by_session.len();
        Self {
            remaining_turns_by_session,
            exported_sessions: 0,
            total_sessions,
        }
    }

    /// Emits the current export progress count.
    fn emit(&self, progress: &mut impl FnMut(SharePushProgress)) -> Result<()> {
        emit_export_session_progress(progress, self.exported_sessions, self.total_sessions)
    }

    /// Marks one exported turn and emits when its session is complete.
    fn finish_turn(
        &mut self,
        session_key: &(darc_paths::SourceKind, String),
        progress: &mut impl FnMut(SharePushProgress),
    ) -> Result<()> {
        let Some(remaining_turns) = self.remaining_turns_by_session.get_mut(session_key) else {
            bail!("export session progress saw an unknown session");
        };
        *remaining_turns = remaining_turns
            .checked_sub(1)
            .context("export session progress underflow")?;
        if *remaining_turns == 0 {
            self.remaining_turns_by_session.remove(session_key);
            self.exported_sessions += 1;
            self.emit(progress)?;
        }
        Ok(())
    }
}
