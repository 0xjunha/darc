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

/// Returns the canonical Git branch for one Darc share branch shorthand.
pub fn share_git_branch(branch: &str) -> Result<String> {
    validate_share_branch_name(branch)?;
    Ok(format!("darc/{branch}"))
}

/// Ensures the Darc share key exists and returns its public key.
pub fn ensure_share_key(root: &Path) -> Result<ShareKeyInfo> {
    let key_path = root.join("keys").join(KEY_FILE_NAME);
    if !key_path.exists() {
        ensure_private_key_directory(root)?;
        let identity = Identity::generate();
        let secret_key = identity.to_string();
        write_share_identity_key(&key_path, secret_key.expose_secret())?;
    }
    harden_share_key_permissions(&key_path)?;
    let identity = read_share_identity_key(&key_path)?;
    Ok(ShareKeyInfo {
        key_path,
        public_key: identity.to_public().to_string(),
    })
}

/// Ensures the Darc share signing key exists and returns the private key.
fn ensure_share_signing_key(root: &Path) -> Result<SigningKey> {
    let key_path = root.join("keys").join(SIGNING_KEY_FILE_NAME);
    if !key_path.exists() {
        ensure_private_key_directory(root)?;
        let entropy = Identity::generate().to_string();
        let seed = Sha256::digest(entropy.expose_secret().as_bytes());
        write_share_private_key(&key_path, &hex_encode(&seed))?;
    }
    harden_private_key_permissions(&key_path)?;
    read_share_signing_key(&key_path)
}

/// Ensures the share key directory exists under the Darc root without following symlinks.
fn ensure_private_key_directory(root: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(root) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!("share root path is a symlink: {}", root.display());
            }
            if !file_type.is_dir() {
                bail!("share root path is not a directory: {}", root.display());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(root)
                .with_context(|| format!("failed to create {}", root.display()))?;
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", root.display()));
        }
    }
    ensure_safe_private_key_directory(root, &root.join("keys"))
}

/// Builds the local share identity from Git config and the Darc public key.
pub fn local_share_identity(context: &ShareProjectContext) -> Result<ShareIdentity> {
    let key = ensure_share_key(&context.root)?;
    let signing_key = ensure_share_signing_key(&context.root)?;
    let signing_public_key = signing_public_key_hex(&signing_key);
    ensure_git_repository(&context.local_path)?;
    let display_name = git_config_value(&context.local_path, "user.name")?;
    let email = git_config_value(&context.local_path, "user.email")?;
    let user_id = derive_user_id(&signing_public_key);
    Ok(ShareIdentity {
        user_id,
        display_name,
        email,
        public_key: key.public_key,
        signing_public_key,
    })
}

/// Reads the project sharing status from the index.
pub fn share_status(context: &ShareProjectContext) -> Result<darc_store::ShareStatus> {
    let connection = open_index_database_writer(&context.index_db_path)?;
    query_share_status(&connection, &context.project_id)
}

/// Sets the default project share policy.
pub fn update_share_policy(context: &ShareProjectContext, policy: SharePolicy) -> Result<()> {
    let connection = open_index_database_writer(&context.index_db_path)?;
    set_project_share_policy(
        &connection,
        &context.project_id,
        policy,
        &current_utc_timestamp(),
    )
}

/// Includes all local sessions by switching the project policy to all.
pub fn include_all_sessions(context: &ShareProjectContext) -> Result<()> {
    let mut connection = open_index_database_writer(&context.index_db_path)?;
    let transaction = connection
        .transaction()
        .context("failed to begin share include-all transaction")?;
    set_project_share_policy(
        &transaction,
        &context.project_id,
        SharePolicy::All,
        &current_utc_timestamp(),
    )?;
    clear_project_share_states(&transaction, &context.project_id)?;
    transaction
        .commit()
        .context("failed to commit share include-all transaction")
}

/// Excludes all local sessions by switching to manual policy and clearing overrides.
pub fn exclude_all_sessions(context: &ShareProjectContext) -> Result<()> {
    let mut connection = open_index_database_writer(&context.index_db_path)?;
    let transaction = connection
        .transaction()
        .context("failed to begin share exclude-all transaction")?;
    set_project_share_policy(
        &transaction,
        &context.project_id,
        SharePolicy::Manual,
        &current_utc_timestamp(),
    )?;
    clear_project_share_states(&transaction, &context.project_id)?;
    transaction
        .commit()
        .context("failed to commit share exclude-all transaction")
}

/// Sets one local session's sharing state.
pub fn update_session_share_state(
    context: &ShareProjectContext,
    provider: darc_paths::SourceKind,
    session_id: &str,
    state: ShareState,
) -> Result<usize> {
    let connection = open_index_database_writer(&context.index_db_path)?;
    set_session_share_state(
        &connection,
        &context.project_id,
        provider,
        session_id,
        state,
    )
}

/// Exports the active project and pushes it to one share branch.
pub fn push_share_branch(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    branch: &str,
    remote_name: Option<&str>,
) -> Result<SharePushReport> {
    push_share_branch_impl(context, settings, branch, remote_name, false, |_| {})
}

/// Exports the active project and pushes it while emitting progress events.
pub fn push_share_branch_with_progress<F>(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    branch: &str,
    remote_name: Option<&str>,
    progress: F,
) -> Result<SharePushReport>
where
    F: FnMut(SharePushProgress),
{
    push_share_branch_impl(context, settings, branch, remote_name, true, progress)
}

/// Exports the active project with optional progress emission.
fn push_share_branch_impl<F>(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    branch: &str,
    remote_name: Option<&str>,
    progress_enabled: bool,
    mut progress: F,
) -> Result<SharePushReport>
where
    F: FnMut(SharePushProgress),
{
    let git_branch = share_git_branch(branch)?;
    let remote = resolve_remote(context, settings, remote_name)?;
    progress(SharePushProgress::Started {
        git_branch: git_branch.clone(),
        remote_name: remote.name.clone(),
        remote_url: remote.display_url.clone(),
    });
    let project_key = project_key(context)?;
    let identity = local_share_identity(context)?;
    let identity_key = ensure_share_key(&context.root)?;
    let decryption_identity = read_share_identity_key(&identity_key.key_path)?;
    let signing_key = ensure_share_signing_key(&context.root)?;
    let connection = open_index_database_writer(&context.index_db_path)?;
    let selected_sessions = query_share_export_session_states(&connection, &context.project_id)?;
    let cache_path = cache_repo_path(&context.root, &remote.resolved_url, &git_branch);
    progress(SharePushProgress::PreparingCache);
    prepare_cache_repository(
        &cache_path,
        &remote.cache_url,
        &context.local_path,
        &identity,
    )?;
    progress(SharePushProgress::FetchingRemote);
    let branch_exists = fetch_branch_if_exists(&cache_path, &git_branch)?;
    if !branch_exists {
        clear_cache_worktree(&cache_path)?;
    }
    checkout_share_branch(&cache_path, &git_branch)?;
    clean_untracked_cache_worktree(&cache_path)?;
    if branch_exists {
        progress(SharePushProgress::HydratingLfs);
        clean_non_artifact_share_cache_files(&cache_path)?;
        let visible_manifest_read = read_cached_manifests(&cache_path)?;
        hydrate_lfs_objects(
            &cache_path,
            &manifest_lfs_object_paths(&visible_manifest_read.manifests)?,
        )?;
    }
    progress(SharePushProgress::ReadingCache);
    let cached_manifest_read = read_cached_manifests(&cache_path)?;
    let previous_project = read_cached_project_artifact(&cache_path).ok().flatten();
    let previous_manifest = cached_manifest_read
        .manifests
        .iter()
        .find(|cached| {
            exporter_manifest_id(&cached.manifest.exporter) == exporter_manifest_id(&identity)
        })
        .map(|cached| cached.manifest.clone());
    let retained_manifests = authenticated_retained_manifests(
        &cache_path,
        &cached_manifest_read.manifests,
        &project_key,
        &identity,
        &decryption_identity,
    )?;
    let trusted_object_cache_path = trusted_object_cache_path(&cache_path);
    let recipient_strings = encryption_recipient_strings(&identity, settings);
    let recipient_fingerprint = encryption_recipient_fingerprint(&recipient_strings);
    let turns = query_share_export_turns(&connection, &context.project_id)?;
    let export_fingerprint = share_export_fingerprint(&turns)?;
    let reuse_context = ExportReuseContext {
        trusted_object_cache_path: Some(&trusted_object_cache_path),
        decryption_identity: Some(&decryption_identity),
        previous_project: previous_project.as_ref(),
        previous_manifest: previous_manifest.as_ref(),
    };
    let artifact = match unchanged_previous_export_artifact(
        &cache_path,
        previous_project.as_ref(),
        previous_manifest.as_ref(),
        &project_key,
        &context.project_name,
        branch,
        &recipient_fingerprint,
        &export_fingerprint,
        &identity,
        &decryption_identity,
        &selected_sessions,
    )? {
        Some(artifact) => {
            progress(SharePushProgress::ReusingPreviousExport {
                exported_turn_count: artifact.exported_turn_count,
                exported_session_count: artifact.exported_session_count,
            });
            artifact
        }
        None => {
            progress(SharePushProgress::BuildingExport {
                total_turns: u64::try_from(turns.len()).context("turn count exceeds u64 range")?,
            });
            build_export_artifact_to_cache_with_reuse(
                ExportBuildRequest {
                    context,
                    settings,
                    project_key: &project_key,
                    identity: &identity,
                    signing_key: &signing_key,
                    branch,
                    turns,
                },
                reuse_context,
                &cache_path,
                &mut progress,
            )?
        }
    };
    remove_replaced_exporter_artifacts(
        &cache_path,
        &identity,
        &cached_manifest_read.manifests,
        &retained_manifests,
        &artifact,
    )?;
    progress(SharePushProgress::WritingMetadata {
        object_count: artifact.object_count,
    });
    write_export_metadata(&cache_path, &artifact)?;
    let allowed_paths = allowed_share_cache_paths(&artifact, &retained_manifests);
    clean_unexpected_share_cache_files(&cache_path, &allowed_paths)?;
    progress(SharePushProgress::Committing);
    let commit_id = commit_cache_repository(&cache_path, &git_branch)?;
    if progress_enabled {
        push_branch_with_progress(&cache_path, &git_branch, &mut progress)?;
    } else {
        push_branch(&cache_path, &git_branch)?;
    }
    progress(SharePushProgress::Finished {
        commit_id: commit_id.clone(),
    });
    Ok(SharePushReport {
        branch: branch.to_owned(),
        git_branch,
        remote_name: remote.name,
        remote_url: remote.display_url,
        project_key,
        exported_turn_count: artifact.exported_turn_count,
        exported_session_count: artifact.exported_session_count,
        object_count: artifact.object_count,
        commit_id,
        pushed: true,
    })
}

/// Builds share export artifacts directly into one cache worktree.
fn build_export_artifact_to_cache_with_reuse(
    request: ExportBuildRequest<'_>,
    reuse: ExportReuseContext<'_>,
    cache_path: &Path,
    progress: &mut impl FnMut(SharePushProgress),
) -> Result<BuiltExportArtifact> {
    let mut target = ExportObjectTarget::Disk { cache_path };
    build_export_artifact_with_target(request, reuse, &mut target, progress)
}

/// Builds share export artifacts in memory for tests and artifact helpers.
#[cfg(test)]
fn build_export_artifact_with_reuse(
    request: ExportBuildRequest<'_>,
    reuse: ExportReuseContext<'_>,
) -> Result<BuiltExportArtifact> {
    let mut objects = BTreeMap::new();
    let mut target = ExportObjectTarget::Memory {
        objects: &mut objects,
    };
    let mut artifact = build_export_artifact_with_target(request, reuse, &mut target, &mut |_| {})?;
    artifact.objects = objects;
    Ok(artifact)
}

/// Fetches one share branch into the local Darc share cache.
pub fn fetch_share_branch(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    branch: &str,
    remote_name: Option<&str>,
) -> Result<ShareFetchReport> {
    fetch_share_branch_impl(context, settings, branch, remote_name, &mut |_| {})
}

/// Fetches one share branch while reporting pull progress.
fn fetch_share_branch_impl(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    branch: &str,
    remote_name: Option<&str>,
    progress: &mut impl FnMut(SharePullProgress),
) -> Result<ShareFetchReport> {
    let git_branch = share_git_branch(branch)?;
    let remote = resolve_remote(context, settings, remote_name)?;
    progress(SharePullProgress::Started {
        git_branch: git_branch.clone(),
        remote_name: remote.name.clone(),
        remote_url: remote.display_url.clone(),
    });
    let identity = local_share_identity(context)?;
    let cache_path = cache_repo_path(&context.root, &remote.resolved_url, &git_branch);
    progress(SharePullProgress::PreparingCache);
    prepare_cache_repository(
        &cache_path,
        &remote.cache_url,
        &context.local_path,
        &identity,
    )?;
    progress(SharePullProgress::FetchingRemote);
    fetch_branch(&cache_path, &git_branch)?;
    checkout_share_branch(&cache_path, &git_branch)?;
    clean_untracked_cache_worktree(&cache_path)?;
    clean_non_artifact_share_cache_files(&cache_path)?;
    let visible_manifest_read = read_cached_manifests(&cache_path)?;
    progress(SharePullProgress::HydratingLfs);
    hydrate_lfs_objects(
        &cache_path,
        &manifest_lfs_object_paths(&visible_manifest_read.manifests)?,
    )?;
    Ok(ShareFetchReport {
        branch: branch.to_owned(),
        git_branch,
        remote_name: remote.name,
        remote_url: remote.display_url,
        fetched: true,
    })
}

/// Imports one previously fetched share branch from the local cache.
pub fn merge_share_branch(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    branch: &str,
    remote_name: Option<&str>,
) -> Result<ShareMergeReport> {
    merge_share_branch_impl(context, settings, branch, remote_name, &mut |_| {})
}

/// Imports one previously fetched share branch while reporting pull progress.
fn merge_share_branch_impl(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    branch: &str,
    remote_name: Option<&str>,
    progress: &mut impl FnMut(SharePullProgress),
) -> Result<ShareMergeReport> {
    let git_branch = share_git_branch(branch)?;
    let remote = resolve_remote(context, settings, remote_name)?;
    let project_key = project_key(context)?;
    let cache_path = cache_repo_path(&context.root, &remote.resolved_url, &git_branch);
    progress(SharePullProgress::ReadingCache);
    clean_cached_checkout(&cache_path)?;
    clean_non_artifact_share_cache_files(&cache_path)?;
    let visible_manifest_read = read_cached_manifests(&cache_path)?;
    hydrate_lfs_objects(
        &cache_path,
        &manifest_lfs_object_paths(&visible_manifest_read.manifests)?,
    )?;
    import_from_cache_with_progress(
        ImportCacheRequest {
            context,
            branch,
            git_branch: &git_branch,
            remote_name: &remote.name,
            remote_url: &remote.resolved_url,
            expected_project_key: &project_key,
            cache_path: &cache_path,
        },
        progress,
    )
}

/// Fetches and imports one share branch.
pub fn pull_share_branch(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    branch: &str,
    remote_name: Option<&str>,
) -> Result<SharePullReport> {
    pull_share_branch_impl(context, settings, branch, remote_name, &mut |_| {})
}

/// Fetches and imports one share branch while emitting progress events.
pub fn pull_share_branch_with_progress<F>(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    branch: &str,
    remote_name: Option<&str>,
    progress: F,
) -> Result<SharePullReport>
where
    F: FnMut(SharePullProgress),
{
    pull_share_branch_impl(context, settings, branch, remote_name, progress)
}

/// Fetches and imports one share branch with optional progress emission.
fn pull_share_branch_impl<F>(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    branch: &str,
    remote_name: Option<&str>,
    mut progress: F,
) -> Result<SharePullReport>
where
    F: FnMut(SharePullProgress),
{
    let fetch = fetch_share_branch_impl(context, settings, branch, remote_name, &mut progress)?;
    let merge = merge_share_branch_impl(context, settings, branch, remote_name, &mut progress)?;
    progress(SharePullProgress::Finished {
        imported_turn_count: merge.imported_turn_count,
        skipped_turn_count: merge.skipped_turn_count,
        warning_count: merge.warning_count,
    });
    Ok(SharePullReport { fetch, merge })
}

/// Adds one remote to a settings vector, replacing an existing same-name remote.
pub fn upsert_remote(settings: &mut ShareSettings, remote: ShareRemote) {
    settings
        .remotes
        .retain(|existing| existing.name != remote.name);
    settings.remotes.push(remote);
    settings
        .remotes
        .sort_by(|left, right| left.name.cmp(&right.name));
}

/// Adds one recipient to a settings vector when it is not already present.
pub fn add_recipient(settings: &mut ShareSettings, recipient: ShareRecipient) {
    if !settings
        .recipients
        .iter()
        .any(|existing| existing.recipient == recipient.recipient)
    {
        settings.recipients.push(recipient);
    }
    settings
        .recipients
        .sort_by(|left, right| left.recipient.cmp(&right.recipient));
}

/// Removes one recipient from a settings vector and returns whether it was present.
pub fn remove_recipient(settings: &mut ShareSettings, recipient: &str) -> bool {
    let old_len = settings.recipients.len();
    settings
        .recipients
        .retain(|existing| existing.recipient != recipient);
    settings.recipients.len() != old_len
}

/// Validates one age recipient string before it is stored in config.
pub fn validate_share_recipient(recipient: &str) -> Result<()> {
    Recipient::from_str(recipient.trim())
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("{error}"))
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

/// Builds all share artifacts for the current export.
#[cfg(test)]
fn build_export_artifact(request: ExportBuildRequest<'_>) -> Result<BuiltExportArtifact> {
    build_export_artifact_with_reuse(request, ExportReuseContext::default())
}

/// Builds all share artifacts while exposing export progress events to tests.
#[cfg(test)]
fn build_export_artifact_with_progress(
    request: ExportBuildRequest<'_>,
    progress: &mut impl FnMut(SharePushProgress),
) -> Result<BuiltExportArtifact> {
    let mut objects = BTreeMap::new();
    let mut target = ExportObjectTarget::Memory {
        objects: &mut objects,
    };
    let mut artifact = build_export_artifact_with_target(
        request,
        ExportReuseContext::default(),
        &mut target,
        progress,
    )?;
    artifact.objects = objects;
    Ok(artifact)
}

/// Builds share artifacts into the provided encrypted object target.
fn build_export_artifact_with_target(
    request: ExportBuildRequest<'_>,
    reuse: ExportReuseContext<'_>,
    target: &mut ExportObjectTarget<'_>,
    progress: &mut impl FnMut(SharePushProgress),
) -> Result<BuiltExportArtifact> {
    let timestamp = current_utc_timestamp();
    let recipient_strings = encryption_recipient_strings(request.identity, request.settings);
    let recipient_fingerprint = encryption_recipient_fingerprint(&recipient_strings);
    let recipients = parse_encryption_recipients(&recipient_strings)?;
    let mut object_paths = BTreeSet::new();
    let mut total_object_bytes = 0_usize;
    let mut manifest_turns = Vec::with_capacity(request.turns.len());
    let mut manifest_chunks = Vec::new();
    let mut session_ids = BTreeSet::new();
    let mut sync_sessions = BTreeSet::new();
    let export_fingerprint = share_export_fingerprint(&request.turns)?;
    let mut chunk_turns = Vec::new();
    let mut chunk_plaintext_bytes = 0_usize;
    let mut chunk_index = 0_u64;
    let total_turns = request.turns.len();
    let mut session_progress = ExportSessionProgress::new(&request.turns);
    if session_progress.total_sessions > 0 {
        session_progress.emit(progress)?;
    }
    for (turn_index, turn) in request.turns.into_iter().enumerate() {
        let session_key = share_turn_session_key(&turn);
        session_ids.insert(session_key.clone());
        if let Some(sync_session) = sync_session_entry_from_turn(&turn) {
            sync_sessions.insert(sync_session);
        }
        let mut payload = EncryptedTurnPayload {
            schema: TURN_PAYLOAD_SCHEMA.to_owned(),
            version: 1,
            project_key: request.project_key.to_owned(),
            exporter: request.identity.clone(),
            signature: None,
            turn,
        };
        sign_turn_payload(&mut payload, request.signing_key)?;
        let plaintext =
            serde_json::to_vec(&payload).context("failed to serialize share payload")?;
        let payload_hash = sha256_hex(&plaintext);
        if !chunk_turns.is_empty()
            && chunk_plaintext_bytes
                .checked_add(plaintext.len())
                .context("share chunk size overflow")?
                > SHARE_CHUNK_TARGET_BYTES
        {
            write_export_chunk(
                &reuse,
                target,
                &mut object_paths,
                &mut total_object_bytes,
                &mut manifest_turns,
                &mut manifest_chunks,
                request.project_key,
                request.identity,
                &recipients,
                &recipient_fingerprint,
                chunk_index,
                std::mem::take(&mut chunk_turns),
            )?;
            chunk_index += 1;
            chunk_plaintext_bytes = 0;
        }
        chunk_plaintext_bytes = chunk_plaintext_bytes
            .checked_add(plaintext.len())
            .context("share chunk size overflow")?;
        chunk_turns.push((payload_hash, payload));
        let exported_turns = turn_index + 1;
        if should_emit_export_turn_progress(exported_turns, total_turns) {
            progress(SharePushProgress::ExportingTurns {
                exported_turns: u64::try_from(exported_turns)
                    .context("turn count exceeds u64 range")?,
                total_turns: u64::try_from(total_turns).context("turn count exceeds u64 range")?,
            });
        }
        session_progress.finish_turn(&session_key, progress)?;
    }
    if !chunk_turns.is_empty() {
        write_export_chunk(
            &reuse,
            target,
            &mut object_paths,
            &mut total_object_bytes,
            &mut manifest_turns,
            &mut manifest_chunks,
            request.project_key,
            request.identity,
            &recipients,
            &recipient_fingerprint,
            chunk_index,
            chunk_turns,
        )?;
    }
    let mut sync_payload = EncryptedSyncPayload {
        schema: SYNC_PAYLOAD_SCHEMA.to_owned(),
        version: SYNC_PAYLOAD_VERSION,
        project_key: request.project_key.to_owned(),
        export_fingerprint,
        exporter: request.identity.clone(),
        signature: None,
        sessions: sync_sessions.into_iter().collect(),
        chunks: manifest_chunks
            .iter()
            .map(sync_chunk_from_manifest)
            .collect(),
        turns: manifest_turns
            .iter()
            .map(sync_entry_from_manifest)
            .collect(),
    };
    sign_sync_payload(&mut sync_payload, request.signing_key)?;
    let sync_plaintext =
        serde_json::to_vec(&sync_payload).context("failed to serialize share sync payload")?;
    let sync_payload_hash = sha256_hex(&sync_plaintext);
    let sync_object_path =
        format!("{ARTIFACT_ROOT}/objects/sync-{recipient_fingerprint}-{sync_payload_hash}.age");
    let sync_encrypted =
        encrypted_export_object(&reuse, &sync_object_path, &sync_plaintext, &recipients)?;
    insert_export_object(
        target,
        &mut object_paths,
        &mut total_object_bytes,
        sync_object_path.clone(),
        sync_encrypted,
    )?;
    let exported_turn_count =
        u64::try_from(manifest_turns.len()).context("turn count exceeds u64 range")?;
    let mut project = ProjectArtifact {
        schema: PROJECT_SCHEMA.to_owned(),
        version: 1,
        project_key: request.project_key.to_owned(),
        project_name: request.context.project_name.clone(),
        updated_at: timestamp.clone(),
    };
    let mut manifest = ManifestArtifact {
        schema: MANIFEST_SCHEMA.to_owned(),
        version: 1,
        project_key: request.project_key.to_owned(),
        branch: request.branch.to_owned(),
        exported_at: timestamp,
        exporter: request.identity.clone(),
        sync: SyncManifestEntry {
            payload_hash: sync_payload_hash,
            object_path: sync_object_path,
        },
        chunks: manifest_chunks,
        turns: manifest_turns,
    };
    if let Some(previous_project) = reuse.previous_project
        && previous_project.schema == project.schema
        && previous_project.version == project.version
        && previous_project.project_key == project.project_key
        && previous_project.project_name == project.project_name
    {
        project.updated_at.clone_from(&previous_project.updated_at);
    }
    if let Some(previous_manifest) = reuse.previous_manifest
        && manifest_matches_without_timestamp(previous_manifest, &manifest)
    {
        manifest
            .exported_at
            .clone_from(&previous_manifest.exported_at);
    }
    Ok(BuiltExportArtifact {
        project,
        manifest,
        #[cfg(test)]
        objects: BTreeMap::new(),
        object_count: u64::try_from(object_paths.len())
            .context("object count exceeds u64 range")?,
        object_paths,
        exported_turn_count,
        exported_session_count: u64::try_from(session_ids.len())
            .context("session count exceeds u64 range")?,
    })
}

/// Returns whether a per-turn export progress update should be emitted.
#[allow(clippy::manual_is_multiple_of)]
fn should_emit_export_turn_progress(exported_turns: usize, total_turns: usize) -> bool {
    exported_turns == 1 || exported_turns == total_turns || exported_turns % 100 == 0
}

/// Returns the stable session identity for one exported turn.
fn share_turn_session_key(turn: &ShareTurnExport) -> (darc_paths::SourceKind, String) {
    (turn.session.provider, turn.session.session_id.clone())
}

/// Emits one session export progress update after checked count conversion.
fn emit_export_session_progress(
    progress: &mut impl FnMut(SharePushProgress),
    exported_sessions: usize,
    total_sessions: usize,
) -> Result<()> {
    progress(SharePushProgress::ExportingSessions {
        exported_sessions: u64::try_from(exported_sessions)
            .context("exported session count exceeds u64 range")?,
        total_sessions: u64::try_from(total_sessions).context("session count exceeds u64 range")?,
    });
    Ok(())
}

/// Reuses the previous signed export when selected source sessions are unchanged.
#[allow(clippy::too_many_arguments)]
fn unchanged_previous_export_artifact(
    cache_path: &Path,
    previous_project: Option<&ProjectArtifact>,
    previous_manifest: Option<&ManifestArtifact>,
    expected_project_key: &str,
    expected_project_name: &str,
    expected_branch: &str,
    expected_recipient_fingerprint: &str,
    expected_export_fingerprint: &str,
    identity: &ShareIdentity,
    decryption_identity: &Identity,
    selected_sessions: &[ShareSessionExportState],
) -> Result<Option<BuiltExportArtifact>> {
    let Some(project) = previous_project else {
        return Ok(None);
    };
    let Some(manifest) = previous_manifest else {
        return Ok(None);
    };
    if project.schema != PROJECT_SCHEMA
        || project.version != 1
        || project.project_key != expected_project_key
        || project.project_name != expected_project_name
        || manifest.schema != MANIFEST_SCHEMA
        || manifest.version != 1
        || manifest.project_key != expected_project_key
        || manifest.branch != expected_branch
        || manifest.exporter != *identity
        || (!manifest.turns.is_empty() && !manifest_turns_are_chunked(&manifest.turns))
        || !manifest_uses_recipient_fingerprint(manifest, expected_recipient_fingerprint)
    {
        return Ok(None);
    }
    let Some(selected_session_set) = sync_session_entries_from_states(selected_sessions) else {
        return Ok(None);
    };
    let sync_payload = match read_sync_payload(
        cache_path,
        manifest,
        expected_project_key,
        decryption_identity,
    ) {
        Ok(payload) => payload,
        Err(_) => return Ok(None),
    };
    if authenticated_manifest_turns(manifest, &sync_payload).is_err() {
        return Ok(None);
    }
    if authenticated_manifest_chunks(manifest, &sync_payload).is_err() {
        return Ok(None);
    }
    if sync_payload.export_fingerprint != expected_export_fingerprint {
        return Ok(None);
    }
    let previous_session_set = sync_payload
        .sessions
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if previous_session_set != selected_session_set {
        return Ok(None);
    }
    if reusable_chunk_objects_are_available(cache_path, manifest).is_err() {
        return Ok(None);
    }
    let object_paths = manifest_object_paths(manifest);
    Ok(Some(BuiltExportArtifact {
        project: project.clone(),
        manifest: manifest.clone(),
        #[cfg(test)]
        objects: BTreeMap::new(),
        object_count: u64::try_from(object_paths.len())
            .context("object count exceeds u64 range")?,
        object_paths,
        exported_turn_count: u64::try_from(manifest.turns.len())
            .context("turn count exceeds u64 range")?,
        exported_session_count: u64::try_from(selected_session_set.len())
            .context("session count exceeds u64 range")?,
    }))
}

/// Returns the exact authenticated chunk set shared by one manifest and sync payload.
fn authenticated_manifest_chunks(
    manifest: &ManifestArtifact,
    sync_payload: &EncryptedSyncPayload,
) -> Result<BTreeSet<SyncChunkEntry>> {
    let authenticated_chunks = sync_payload.chunks.iter().cloned().collect::<BTreeSet<_>>();
    let manifest_chunks = manifest
        .chunks
        .iter()
        .map(sync_chunk_from_manifest)
        .collect::<BTreeSet<_>>();
    if authenticated_chunks != manifest_chunks {
        bail!("signed sync chunks do not match visible manifest chunks");
    }
    Ok(authenticated_chunks)
}

/// Returns whether every encrypted object path targets the current recipient set.
fn manifest_uses_recipient_fingerprint(
    manifest: &ManifestArtifact,
    expected_recipient_fingerprint: &str,
) -> bool {
    let sync_prefix = format!("{ARTIFACT_ROOT}/objects/sync-{expected_recipient_fingerprint}-");
    let chunk_prefix = format!("{ARTIFACT_ROOT}/objects/{expected_recipient_fingerprint}-");
    manifest.sync.object_path.starts_with(&sync_prefix)
        && manifest
            .chunks
            .iter()
            .all(|chunk| chunk.object_path.starts_with(&chunk_prefix))
}

/// Builds the current selected-session set when every session has source metadata.
fn sync_session_entries_from_states(
    selected_sessions: &[ShareSessionExportState],
) -> Option<BTreeSet<SyncSessionEntry>> {
    selected_sessions
        .iter()
        .map(sync_session_entry_from_state)
        .collect()
}

/// Checks that encrypted chunk files referenced by a reusable manifest still exist.
fn reusable_chunk_objects_are_available(
    cache_path: &Path,
    manifest: &ManifestArtifact,
) -> Result<()> {
    let mut chunk_paths = BTreeSet::new();
    for chunk in &manifest.chunks {
        validate_manifest_object_relative_path(&chunk.object_path)?;
        let object_path = manifest_artifact_path(cache_path, &chunk.object_path)?;
        let ciphertext = read_regular_file(&object_path, MAX_SHARE_OBJECT_BYTES)?;
        ensure_not_lfs_pointer(&ciphertext, &object_path)?;
        if sha256_hex(&ciphertext) != chunk.ciphertext_hash {
            bail!("cached share chunk ciphertext hash mismatch");
        }
        chunk_paths.insert(chunk.object_path.clone());
    }
    let turn_paths = manifest
        .turns
        .iter()
        .map(|turn| turn.object_path.clone())
        .collect::<BTreeSet<_>>();
    if turn_paths != chunk_paths {
        bail!("cached share chunk manifest does not cover every turn object");
    }
    Ok(())
}

/// Builds and inserts one compressed encrypted share chunk.
#[allow(clippy::too_many_arguments)]
fn write_export_chunk(
    reuse: &ExportReuseContext<'_>,
    target: &mut ExportObjectTarget<'_>,
    object_paths: &mut BTreeSet<String>,
    total_object_bytes: &mut usize,
    manifest_turns: &mut Vec<TurnManifestEntry>,
    manifest_chunks: &mut Vec<ChunkManifestEntry>,
    project_key: &str,
    identity: &ShareIdentity,
    recipients: &[Recipient],
    recipient_fingerprint: &str,
    chunk_index: u64,
    chunk_turns: Vec<(String, EncryptedTurnPayload)>,
) -> Result<()> {
    let chunk_id = format!("chunk-{chunk_index:08}");
    let record_count = chunk_turns.len();
    let chunk_payload = ShareChunkPayload {
        schema: CHUNK_PAYLOAD_SCHEMA.to_owned(),
        version: CHUNK_PAYLOAD_VERSION,
        project_key: project_key.to_owned(),
        exporter: identity.clone(),
        chunk_id: chunk_id.clone(),
        turns: chunk_turns
            .iter()
            .map(|(_, payload)| payload.clone())
            .collect(),
    };
    let chunk_json =
        serde_json::to_vec(&chunk_payload).context("failed to serialize share chunk payload")?;
    if u64::try_from(chunk_json.len()).unwrap_or(u64::MAX) > MAX_SHARE_CHUNK_DECOMPRESSED_BYTES {
        bail!(
            "share chunk exceeds maximum supported decompressed size of {MAX_SHARE_CHUNK_DECOMPRESSED_BYTES} bytes"
        );
    }
    let compressed_plaintext = gzip_compress(&chunk_json)?;
    let plaintext_hash = sha256_hex(&compressed_plaintext);
    let object_path =
        format!("{ARTIFACT_ROOT}/objects/{recipient_fingerprint}-{chunk_id}-{plaintext_hash}.age");
    let encrypted =
        encrypted_export_object(reuse, &object_path, &compressed_plaintext, recipients)?;
    let ciphertext_bytes =
        u64::try_from(encrypted.len()).context("chunk ciphertext size exceeds u64 range")?;
    let ciphertext_hash = sha256_hex(&encrypted);
    insert_export_object(
        target,
        object_paths,
        total_object_bytes,
        object_path.clone(),
        encrypted,
    )?;
    manifest_chunks.push(ChunkManifestEntry {
        chunk_id: chunk_id.clone(),
        object_path: object_path.clone(),
        compression: "gzip".to_owned(),
        plaintext_hash,
        ciphertext_hash,
        plaintext_bytes: u64::try_from(compressed_plaintext.len())
            .context("chunk plaintext size exceeds u64 range")?,
        ciphertext_bytes,
        turn_count: u64::try_from(record_count).context("chunk turn count exceeds u64 range")?,
    });
    for (record_index, (payload_hash, payload)) in chunk_turns.into_iter().enumerate() {
        manifest_turns.push(TurnManifestEntry {
            provider: payload.turn.session.provider,
            session_id: payload.turn.session.session_id,
            turn_ordinal: payload.turn.turn_ordinal,
            started_at: payload.turn.started_at,
            payload_hash,
            object_path: object_path.clone(),
            chunk_id: Some(chunk_id.clone()),
            chunk_record_index: Some(
                u32::try_from(record_index).context("chunk record index exceeds u32 range")?,
            ),
        });
    }
    Ok(())
}

/// Returns whether two manifests differ only by export timestamp.
fn manifest_matches_without_timestamp(left: &ManifestArtifact, right: &ManifestArtifact) -> bool {
    left.schema == right.schema
        && left.version == right.version
        && left.project_key == right.project_key
        && left.branch == right.branch
        && left.exporter == right.exporter
        && left.sync == right.sync
        && left.chunks == right.chunks
        && left.turns == right.turns
}

/// Returns encrypted bytes for one export object, reusing trusted local bytes when valid.
fn encrypted_export_object(
    reuse: &ExportReuseContext<'_>,
    object_path: &str,
    plaintext: &[u8],
    recipients: &[Recipient],
) -> Result<Vec<u8>> {
    if let (Some(cache_path), Some(identity)) =
        (reuse.trusted_object_cache_path, reuse.decryption_identity)
        && let Some(ciphertext) =
            read_trusted_export_object(cache_path, object_path, plaintext, identity)?
    {
        return Ok(ciphertext);
    }

    let encrypted = encrypt_payload(plaintext, recipients)?;
    if let Some(cache_path) = reuse.trusted_object_cache_path {
        write_trusted_export_object(cache_path, object_path, &encrypted)?;
    }
    Ok(encrypted)
}

/// Reads one trusted local object cache entry if it decrypts to the expected plaintext.
fn read_trusted_export_object(
    cache_path: &Path,
    object_path: &str,
    expected_plaintext: &[u8],
    identity: &Identity,
) -> Result<Option<Vec<u8>>> {
    let path = trusted_export_object_path(cache_path, object_path);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!(
                    "trusted share object cache path is a symlink: {}",
                    path.display()
                );
            }
            if !metadata.file_type().is_file() {
                return Ok(None);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
    let ciphertext = read_regular_file(&path, MAX_SHARE_OBJECT_BYTES)?;
    let Ok(plaintext) = decrypt_payload(&ciphertext, identity) else {
        return Ok(None);
    };
    if plaintext == expected_plaintext {
        Ok(Some(ciphertext))
    } else {
        Ok(None)
    }
}

/// Writes one encrypted object to the trusted local object cache.
fn write_trusted_export_object(cache_path: &Path, object_path: &str, content: &[u8]) -> Result<()> {
    ensure_safe_trusted_object_cache_dir(cache_path)?;
    let target = trusted_export_object_path(cache_path, object_path);
    if let Ok(metadata) = fs::symlink_metadata(&target)
        && metadata.file_type().is_symlink()
    {
        bail!(
            "trusted share object cache path is a symlink: {}",
            target.display()
        );
    }
    let temporary = target.with_extension(format!("tmp-{}", &sha256_hex(content)[..16]));
    remove_file_if_exists(&temporary)?;
    fs::write(&temporary, content)
        .with_context(|| format!("failed to write {}", target.display()))?;
    fs::rename(&temporary, &target)
        .with_context(|| format!("failed to replace {}", target.display()))?;
    Ok(())
}

/// Ensures the trusted local object cache directory exists without following symlinks.
fn ensure_safe_trusted_object_cache_dir(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("trusted share object cache path is missing a parent")?;
    let parent_metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("failed to inspect {}", parent.display()))?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.file_type().is_dir() {
        bail!(
            "trusted share object cache parent is not a real directory: {}",
            parent.display()
        );
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!(
                    "trusted share object cache path is a symlink: {}",
                    path.display()
                );
            }
            if !metadata.file_type().is_dir() {
                bail!(
                    "trusted share object cache path is not a directory: {}",
                    path.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).with_context(|| format!("failed to create {}", path.display()))?;
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
    Ok(())
}

/// Returns the trusted local cache path for one export object path.
fn trusted_export_object_path(cache_path: &Path, object_path: &str) -> PathBuf {
    cache_path.join(format!("{}.age", sha256_hex(object_path.as_bytes())))
}

/// Inserts one encrypted export object while enforcing in-memory export caps.
fn insert_export_object(
    target: &mut ExportObjectTarget<'_>,
    object_paths: &mut BTreeSet<String>,
    total_object_bytes: &mut usize,
    object_path: String,
    content: Vec<u8>,
) -> Result<()> {
    if object_paths.len() >= MAX_SHARE_EXPORT_OBJECTS {
        bail!("share export exceeds {MAX_SHARE_EXPORT_OBJECTS} encrypted objects");
    }
    *total_object_bytes = total_object_bytes
        .checked_add(content.len())
        .context("share export object size overflow")?;
    if *total_object_bytes > MAX_SHARE_EXPORT_BYTES {
        bail!("share export exceeds {MAX_SHARE_EXPORT_BYTES} encrypted bytes");
    }
    object_paths.insert(object_path.clone());
    match target {
        #[cfg(test)]
        ExportObjectTarget::Memory { objects } => {
            objects.insert(object_path, content);
        }
        ExportObjectTarget::Disk { cache_path } => {
            write_artifact_file(cache_path, &object_path, &content)?
        }
    }
    Ok(())
}

/// Writes all share artifacts into a cache repository workdir.
#[cfg(test)]
fn write_export_artifact(path: &Path, artifact: &BuiltExportArtifact) -> Result<()> {
    write_export_metadata(path, artifact)?;
    for (relative, content) in &artifact.objects {
        write_artifact_file(path, relative, content)?;
    }
    Ok(())
}

/// Writes visible share metadata into a cache repository workdir.
fn write_export_metadata(path: &Path, artifact: &BuiltExportArtifact) -> Result<()> {
    write_artifact_file(
        path,
        GIT_ATTRIBUTES_FILE,
        b"darc-share/v1/objects/*.age filter=lfs diff=lfs merge=lfs -text\n",
    )?;
    write_json_artifact_file(
        path,
        &format!("{ARTIFACT_ROOT}/{PROJECT_FILE}"),
        &artifact.project,
    )?;
    write_json_artifact_file(
        path,
        &exporter_manifest_relative_path(&artifact.manifest.exporter),
        &artifact.manifest,
    )?;
    Ok(())
}

/// Reads the visible project artifact from one cache workdir when it exists.
fn read_cached_project_artifact(cache_path: &Path) -> Result<Option<ProjectArtifact>> {
    let relative_path = format!("{ARTIFACT_ROOT}/{PROJECT_FILE}");
    let relative = Path::new(&relative_path);
    ensure_safe_artifact_ancestors(cache_path, relative)?;
    let path = cache_path.join(relative);
    if !path.exists() {
        return Ok(None);
    }
    read_json_file(&path).map(Some)
}

/// Reads all visible manifests from one cache workdir.
fn read_cached_manifests(cache_path: &Path) -> Result<CachedManifestRead> {
    if !ensure_safe_existing_cache_dir(cache_path)? {
        return Ok(CachedManifestRead {
            manifests: Vec::new(),
            warnings: Vec::new(),
        });
    }
    let mut manifests = Vec::new();
    let mut warnings = Vec::new();
    let mut manifest_count = 0_usize;
    let mut manifest_bytes = 0_u64;
    let exporter_root_relative = format!("{ARTIFACT_ROOT}/{EXPORTERS_DIR}");
    let exporter_root = cache_path.join(&exporter_root_relative);
    if let Err(error) =
        ensure_safe_artifact_ancestors(cache_path, Path::new(&exporter_root_relative))
    {
        warnings.push(format!(
            "skipped share exporter root {}: {error:#}",
            exporter_root.display()
        ));
        return Ok(CachedManifestRead {
            manifests,
            warnings,
        });
    }
    match is_regular_directory(&exporter_root) {
        Ok(true) => {
            let mut exporter_dir_count = 0_usize;
            for entry in fs::read_dir(&exporter_root)
                .with_context(|| format!("failed to read {}", exporter_root.display()))?
            {
                let entry =
                    entry.with_context(|| format!("failed to read {}", exporter_root.display()))?;
                let file_type = entry
                    .file_type()
                    .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
                if !file_type.is_dir() {
                    continue;
                }
                if exporter_dir_count >= MAX_CACHED_SHARE_EXPORTER_DIRS {
                    warnings.push(format!(
                        "skipped share exporter directories under {}: cached exporter directory count exceeds {MAX_CACHED_SHARE_EXPORTER_DIRS}",
                        exporter_root.display()
                    ));
                    break;
                }
                exporter_dir_count += 1;
                let Some(exporter_dir) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let relative_path = format!(
                    "{ARTIFACT_ROOT}/{EXPORTERS_DIR}/{exporter_dir}/{LEGACY_MANIFEST_FILE}"
                );
                let manifest_path = cache_path.join(&relative_path);
                if !read_cached_manifest(
                    &mut manifests,
                    &mut warnings,
                    &mut manifest_count,
                    &mut manifest_bytes,
                    cache_path,
                    relative_path,
                    &manifest_path,
                ) {
                    break;
                }
            }
        }
        Ok(false) => {}
        Err(error) => warnings.push(format!(
            "skipped share exporter root {}: {error:#}",
            exporter_root.display()
        )),
    }
    manifests.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let legacy_relative_path = format!("{ARTIFACT_ROOT}/{LEGACY_MANIFEST_FILE}");
    let legacy_path = cache_path.join(&legacy_relative_path);
    read_cached_manifest(
        &mut manifests,
        &mut warnings,
        &mut manifest_count,
        &mut manifest_bytes,
        cache_path,
        legacy_relative_path,
        &legacy_path,
    );
    Ok(CachedManifestRead {
        manifests,
        warnings,
    })
}

/// Reads one cached manifest, collecting parse failures as warnings.
fn read_cached_manifest(
    manifests: &mut Vec<CachedManifest>,
    warnings: &mut Vec<String>,
    manifest_count: &mut usize,
    manifest_bytes: &mut u64,
    cache_path: &Path,
    relative_path: String,
    manifest_path: &Path,
) -> bool {
    if let Err(error) = ensure_safe_artifact_ancestors(cache_path, Path::new(&relative_path)) {
        warnings.push(format!("skipped share manifest {relative_path}: {error:#}"));
        return true;
    }
    if !manifest_path.exists() {
        return true;
    }
    if *manifest_count >= MAX_CACHED_SHARE_MANIFESTS {
        warnings.push(format!(
            "skipped share manifest {relative_path}: cached manifest count exceeds {MAX_CACHED_SHARE_MANIFESTS}"
        ));
        return false;
    }
    let manifest_size = match checked_cached_manifest_size(manifest_path) {
        Ok(size) => size,
        Err(error) => {
            warnings.push(format!("skipped share manifest {relative_path}: {error:#}"));
            return true;
        }
    };
    match manifest_bytes.checked_add(manifest_size) {
        Some(total) if total <= MAX_CACHED_SHARE_MANIFEST_BYTES => {
            *manifest_count += 1;
            *manifest_bytes = total;
        }
        _ => {
            warnings.push(format!(
                "skipped share manifest {relative_path}: cached manifest bytes exceed {MAX_CACHED_SHARE_MANIFEST_BYTES}"
            ));
            return false;
        }
    }
    match read_json_file(manifest_path) {
        Ok(manifest) => manifests.push(CachedManifest {
            relative_path,
            manifest,
        }),
        Err(error) => warnings.push(format!("skipped share manifest {relative_path}: {error:#}")),
    }
    true
}

/// Returns one candidate manifest size after regular-file validation.
fn checked_cached_manifest_size(path: &Path) -> Result<u64> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!("share artifact path is a symlink: {}", path.display());
    }
    if !file_type.is_file() {
        bail!(
            "share artifact path is not a regular file: {}",
            path.display()
        );
    }
    if metadata.len() > MAX_SHARE_MANIFEST_BYTES {
        bail!(
            "share artifact {} exceeds maximum supported size of {} bytes",
            path.display(),
            MAX_SHARE_MANIFEST_BYTES
        );
    }
    Ok(metadata.len())
}

/// Removes stale files owned by the exporter being replaced.
fn remove_replaced_exporter_artifacts(
    cache_path: &Path,
    identity: &ShareIdentity,
    cached_manifests: &[CachedManifest],
    retained_manifests: &[CachedManifest],
    artifact: &BuiltExportArtifact,
) -> Result<()> {
    let current_exporter_id = exporter_manifest_id(identity);
    let retained_object_paths = retained_manifests
        .iter()
        .flat_map(|cached| manifest_object_paths(&cached.manifest))
        .collect::<BTreeSet<_>>();
    let stale_object_paths = cached_manifests
        .iter()
        .filter(|cached| exporter_manifest_id(&cached.manifest.exporter) == current_exporter_id)
        .flat_map(|cached| manifest_object_paths(&cached.manifest))
        .filter(|path| {
            !artifact.object_paths.contains(path) && !retained_object_paths.contains(path)
        })
        .collect::<BTreeSet<_>>();

    for cached in cached_manifests
        .iter()
        .filter(|cached| exporter_manifest_id(&cached.manifest.exporter) == current_exporter_id)
    {
        remove_relative_file(cache_path, &cached.relative_path)?;
    }
    for object_path in stale_object_paths {
        remove_artifact_object(cache_path, &object_path)?;
    }
    Ok(())
}

/// Returns all encrypted object paths referenced by one manifest.
fn manifest_object_paths(manifest: &ManifestArtifact) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    paths.insert(manifest.sync.object_path.clone());
    paths.extend(manifest.turns.iter().map(|turn| turn.object_path.clone()));
    paths
}

/// Returns validated encrypted object paths referenced by visible manifests.
fn manifest_lfs_object_paths(cached_manifests: &[CachedManifest]) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    for cached in cached_manifests {
        for object_path in manifest_object_paths(&cached.manifest) {
            validate_manifest_object_relative_path(&object_path).with_context(|| {
                format!(
                    "share manifest {} references invalid object path {object_path}",
                    cached.relative_path
                )
            })?;
            paths.insert(object_path);
        }
    }
    Ok(paths)
}

/// Returns cached manifests whose encrypted payloads authenticate for retention.
fn authenticated_retained_manifests(
    cache_path: &Path,
    cached_manifests: &[CachedManifest],
    expected_project_key: &str,
    identity: &ShareIdentity,
    decryption_identity: &Identity,
) -> Result<Vec<CachedManifest>> {
    let current_exporter_id = exporter_manifest_id(identity);
    let mut retained = Vec::new();
    for cached in cached_manifests
        .iter()
        .filter(|cached| exporter_manifest_id(&cached.manifest.exporter) != current_exporter_id)
        .filter(|cached| cached.manifest.schema == MANIFEST_SCHEMA)
        .filter(|cached| cached.manifest.version == 1)
        .filter(|cached| cached.manifest.project_key == expected_project_key)
    {
        let sync_payload = read_sync_payload(
            cache_path,
            &cached.manifest,
            expected_project_key,
            decryption_identity,
        )
        .with_context(|| {
            format!(
                "failed to authenticate retained share manifest {} for exporter {}; refusing to rewrite share branch without preserving it",
                cached.relative_path, cached.manifest.exporter.user_id
            )
        })?;
        let exporter_id = exporter_manifest_id(&sync_payload.exporter);
        if !manifest_path_matches_exporter(&cached.relative_path, &exporter_id) {
            bail!(
                "retained share manifest {} path does not match exporter {}; refusing to rewrite share branch without preserving it",
                cached.relative_path,
                sync_payload.exporter.user_id
            );
        }
        authenticated_manifest_turns(&cached.manifest, &sync_payload).with_context(|| {
            format!(
                "failed to authenticate retained share manifest {} turn metadata for exporter {}; refusing to rewrite share branch without preserving it",
                cached.relative_path, sync_payload.exporter.user_id
            )
        })?;
        authenticated_manifest_chunks(&cached.manifest, &sync_payload).with_context(|| {
            format!(
                "failed to authenticate retained share manifest {} chunk metadata for exporter {}; refusing to rewrite share branch without preserving it",
                cached.relative_path, sync_payload.exporter.user_id
            )
        })?;
        verify_cached_manifest_payloads(
            cache_path,
            &cached.manifest,
            expected_project_key,
            decryption_identity,
        )
        .with_context(|| {
            format!(
                "failed to verify retained share manifest {} payloads for exporter {}; refusing to rewrite share branch without preserving it",
                cached.relative_path, sync_payload.exporter.user_id
            )
        })?;
        retained.push(cached.clone());
    }
    Ok(retained)
}

/// Verifies all encrypted turn payloads referenced by one cached manifest.
fn verify_cached_manifest_payloads(
    cache_path: &Path,
    manifest: &ManifestArtifact,
    expected_project_key: &str,
    identity: &Identity,
) -> Result<()> {
    if manifest_turns_are_chunked(&manifest.turns) {
        let chunks = read_manifest_chunks(cache_path, manifest, expected_project_key, identity);
        if !chunks.errors.is_empty() {
            bail!("share manifest contains unauthenticated chunks");
        }
        for entry in &manifest.turns {
            verify_chunked_manifest_entry(&chunks, entry)?;
        }
        return Ok(());
    }
    for entry in &manifest.turns {
        verify_cached_turn_payload(cache_path, manifest, expected_project_key, identity, entry)?;
    }
    Ok(())
}

/// Returns the exact authenticated turn set shared by one manifest and sync payload.
fn authenticated_manifest_turns(
    manifest: &ManifestArtifact,
    sync_payload: &EncryptedSyncPayload,
) -> Result<BTreeSet<SyncTurnEntry>> {
    validate_manifest_chunk_mode(&manifest.turns)?;
    let authenticated_turns = sync_payload.turns.iter().cloned().collect::<BTreeSet<_>>();
    let manifest_turns = manifest
        .turns
        .iter()
        .map(sync_entry_from_manifest)
        .collect::<BTreeSet<_>>();
    if authenticated_turns != manifest_turns {
        bail!("signed sync entries do not match visible manifest entries");
    }
    Ok(authenticated_turns)
}

/// Returns whether one manifest entry belongs to a chunked payload.
fn manifest_entry_is_chunked(entry: &TurnManifestEntry) -> bool {
    entry.chunk_id.is_some() || entry.chunk_record_index.is_some()
}

/// Returns whether a signed manifest turn set uses chunked payloads.
fn manifest_turns_are_chunked(entries: &[TurnManifestEntry]) -> bool {
    entries.iter().any(manifest_entry_is_chunked)
}

/// Validates that all manifest entries use one payload mode.
fn validate_manifest_chunk_mode(entries: &[TurnManifestEntry]) -> Result<()> {
    let chunked_count = entries
        .iter()
        .filter(|entry| manifest_entry_is_chunked(entry))
        .count();
    if chunked_count != 0 && chunked_count != entries.len() {
        bail!("share manifest mixes legacy and chunked turn entries");
    }
    Ok(())
}

/// Verifies one cached turn object without importing it into SQLite.
fn verify_cached_turn_payload(
    cache_path: &Path,
    manifest: &ManifestArtifact,
    expected_project_key: &str,
    identity: &Identity,
    entry: &TurnManifestEntry,
) -> Result<()> {
    if manifest_entry_is_chunked(entry) {
        let chunks = read_manifest_chunks(cache_path, manifest, expected_project_key, identity);
        if !chunks.errors.is_empty() {
            bail!("share manifest contains unauthenticated chunks");
        }
        verify_chunked_manifest_entry(&chunks, entry)?;
        return Ok(());
    }
    if entry.chunk_id.is_some() || entry.chunk_record_index.is_some() {
        bail!("legacy share manifest entry unexpectedly references a chunk");
    }
    let object_path = manifest_object_path(cache_path, entry)?;
    let ciphertext = read_regular_file(&object_path, MAX_SHARE_OBJECT_BYTES)?;
    ensure_not_lfs_pointer(&ciphertext, &object_path)?;
    let plaintext =
        decrypt_payload(&ciphertext, identity).context("failed to decrypt share object")?;
    if sha256_hex(&plaintext) != entry.payload_hash {
        bail!("share payload hash mismatch");
    }
    let payload: EncryptedTurnPayload =
        serde_json::from_slice(&plaintext).context("failed to parse share payload JSON")?;
    if payload.schema != TURN_PAYLOAD_SCHEMA {
        bail!("unsupported share payload schema `{}`", payload.schema);
    }
    if payload.version != 1 {
        bail!("unsupported share payload version `{}`", payload.version);
    }
    if payload.project_key != expected_project_key {
        bail!("share payload project key does not match active project");
    }
    if payload.exporter != manifest.exporter {
        bail!("share payload exporter does not match manifest exporter");
    }
    verify_turn_payload_signature(&payload)?;
    if payload.turn.session.provider != entry.provider
        || payload.turn.session.session_id != entry.session_id
        || payload.turn.turn_ordinal != entry.turn_ordinal
        || payload.turn.started_at != entry.started_at
    {
        bail!("share payload identity does not match manifest entry");
    }
    Ok(())
}

/// Imports all valid encrypted payloads from one cache workdir.
#[cfg(test)]
fn import_from_cache(
    context: &ShareProjectContext,
    branch: &str,
    git_branch: &str,
    remote_name: &str,
    remote_url: &str,
    expected_project_key: &str,
    cache_path: &Path,
) -> Result<ShareMergeReport> {
    import_from_cache_with_progress(
        ImportCacheRequest {
            context,
            branch,
            git_branch,
            remote_name,
            remote_url,
            expected_project_key,
            cache_path,
        },
        &mut |_| {},
    )
}

/// Imports all valid encrypted payloads while reporting session progress.
fn import_from_cache_with_progress(
    request: ImportCacheRequest<'_>,
    progress: &mut impl FnMut(SharePullProgress),
) -> Result<ShareMergeReport> {
    let ImportCacheRequest {
        context,
        branch,
        git_branch,
        remote_name,
        remote_url,
        expected_project_key,
        cache_path,
    } = request;
    let CachedManifestRead {
        manifests,
        mut warnings,
    } = read_cached_manifests(cache_path)?;
    if manifests.is_empty() && warnings.is_empty() {
        bail!(
            "share branch does not contain a Darc manifest at {}/{} or {}/{}/<exporter>/{}",
            ARTIFACT_ROOT,
            LEGACY_MANIFEST_FILE,
            ARTIFACT_ROOT,
            EXPORTERS_DIR,
            LEGACY_MANIFEST_FILE
        );
    }
    if manifests.is_empty() {
        return Ok(ShareMergeReport {
            branch: branch.to_owned(),
            git_branch: git_branch.to_owned(),
            remote_name: remote_name.to_owned(),
            project_key: expected_project_key.to_owned(),
            imported_turn_count: 0,
            skipped_turn_count: 0,
            warning_count: u64::try_from(warnings.len())
                .context("warning count exceeds u64 range")?,
            warnings,
        });
    }
    let mut session_progress = ImportSessionProgress::new(&manifests)?;
    if session_progress.total_sessions > 0 {
        session_progress.emit(progress);
    }
    let identity_key = ensure_share_key(&context.root)?;
    let identity = read_share_identity_key(&identity_key.key_path)?;
    let mut connection = open_index_database_writer(&context.index_db_path)?;
    let origin_remote = share_origin_remote(remote_url, git_branch);
    let mut imported_exporters = BTreeSet::new();
    let mut imported_turn_count = 0_u64;
    let mut skipped_turn_count = 0_u64;
    for cached in manifests {
        let manifest = cached.manifest;
        if manifest.schema != MANIFEST_SCHEMA {
            skipped_turn_count += u64::try_from(manifest.turns.len())
                .context("skipped turn count exceeds u64 range")?;
            warnings.push(format!(
                "skipped share manifest {} for exporter {}: unsupported Darc share manifest schema `{}`",
                cached.relative_path,
                manifest.exporter.user_id,
                manifest.schema
            ));
            session_progress.finish_manifest(&manifest, progress);
            continue;
        }
        if manifest.version != 1 {
            skipped_turn_count += u64::try_from(manifest.turns.len())
                .context("skipped turn count exceeds u64 range")?;
            warnings.push(format!(
                "skipped share manifest {} for exporter {}: unsupported Darc share manifest version `{}`",
                cached.relative_path,
                manifest.exporter.user_id,
                manifest.version
            ));
            session_progress.finish_manifest(&manifest, progress);
            continue;
        }
        if manifest.project_key != expected_project_key {
            skipped_turn_count += u64::try_from(manifest.turns.len())
                .context("skipped turn count exceeds u64 range")?;
            warnings.push(format!(
                "skipped share manifest {} for exporter {}: share branch project key `{}` does not match active project key `{}`",
                cached.relative_path,
                manifest.exporter.user_id,
                manifest.project_key,
                expected_project_key
            ));
            session_progress.finish_manifest(&manifest, progress);
            continue;
        }
        let sync_payload =
            match read_sync_payload(cache_path, &manifest, expected_project_key, &identity) {
                Ok(payload) => payload,
                Err(error) => {
                    skipped_turn_count += u64::try_from(manifest.turns.len())
                        .context("skipped turn count exceeds u64 range")?;
                    warnings.push(format!(
                        "skipped share manifest {} for exporter {}: {error:#}",
                        cached.relative_path, manifest.exporter.user_id
                    ));
                    session_progress.finish_manifest(&manifest, progress);
                    continue;
                }
            };
        let exporter_id = exporter_manifest_id(&sync_payload.exporter);
        if !manifest_path_matches_exporter(&cached.relative_path, &exporter_id) {
            skipped_turn_count += u64::try_from(manifest.turns.len())
                .context("skipped turn count exceeds u64 range")?;
            warnings.push(format!(
                "skipped share manifest {} for exporter {}: manifest path does not match exporter identity",
                cached.relative_path, sync_payload.exporter.user_id
            ));
            session_progress.finish_manifest(&manifest, progress);
            continue;
        }
        if imported_exporters.contains(&exporter_id) {
            warnings.push(format!(
                "skipped duplicate share manifest {} for exporter {}",
                cached.relative_path, sync_payload.exporter.user_id
            ));
            session_progress.finish_manifest(&manifest, progress);
            continue;
        }
        let authenticated_turns = match authenticated_manifest_turns(&manifest, &sync_payload) {
            Ok(turns) => turns,
            Err(error) => {
                skipped_turn_count += u64::try_from(manifest.turns.len())
                    .context("skipped turn count exceeds u64 range")?;
                warnings.push(format!(
                    "skipped share manifest {} for exporter {}: {error:#}",
                    cached.relative_path, sync_payload.exporter.user_id
                ));
                session_progress.finish_manifest(&manifest, progress);
                continue;
            }
        };
        if let Err(error) = authenticated_manifest_chunks(&manifest, &sync_payload) {
            skipped_turn_count += u64::try_from(manifest.turns.len())
                .context("skipped turn count exceeds u64 range")?;
            warnings.push(format!(
                "skipped share manifest {} for exporter {}: {error:#}",
                cached.relative_path, sync_payload.exporter.user_id
            ));
            session_progress.finish_manifest(&manifest, progress);
            continue;
        }
        let mut keep_turns = authenticated_turns
            .iter()
            .map(sync_turn_prune_key)
            .collect::<BTreeSet<_>>();
        let mut manifest_decode_complete = true;
        let imported_at = current_utc_timestamp();
        let user = ShareUserRecord {
            user_id: sync_payload.exporter.user_id.clone(),
            display_name: sync_payload.exporter.display_name.clone(),
            email: sync_payload.exporter.email.clone(),
            public_key: Some(sync_payload.exporter.public_key.clone()),
            source: "share-manifest".to_owned(),
            updated_at: imported_at.clone(),
        };
        if !manifest_turns_are_chunked(&manifest.turns) {
            let chunks = DecodedChunks::default();
            let import_context = ImportEntryContext {
                expected_project_key,
                expected_exporter: &sync_payload.exporter,
                identity: &identity,
                cache_path,
                chunks: &chunks,
            };
            let mut decoded_turns = Vec::new();
            for entry in &manifest.turns {
                match read_manifest_entry_turn(&import_context, entry) {
                    Ok(turn) => decoded_turns.push(turn),
                    Err(error) => {
                        manifest_decode_complete = false;
                        skipped_turn_count += 1;
                        warnings.push(format!(
                            "skipped {} session {} turn {}: {error:#}",
                            entry.provider.directory_name(),
                            entry.session_id,
                            entry.turn_ordinal
                        ));
                    }
                }
                session_progress.finish_entry(&manifest, entry, progress);
            }
            import_decoded_turns(
                &mut connection,
                context,
                &origin_remote,
                &user,
                &imported_at,
                &decoded_turns,
                &mut keep_turns,
                &mut imported_turn_count,
                &mut skipped_turn_count,
                &mut warnings,
            )?;
        } else {
            manifest_decode_complete = import_chunked_manifest_turns(
                &mut connection,
                context,
                cache_path,
                expected_project_key,
                &identity,
                &manifest,
                &sync_payload.exporter,
                &cached.relative_path,
                &origin_remote,
                &user,
                &imported_at,
                &mut keep_turns,
                &mut imported_turn_count,
                &mut skipped_turn_count,
                &mut warnings,
                &mut session_progress,
                progress,
            )?;
        }
        prune_shared_turns(
            &connection,
            &context.project_id,
            &origin_remote,
            &sync_payload.exporter.user_id,
            &keep_turns,
        )?;
        if manifest_decode_complete {
            imported_exporters.insert(exporter_id);
        }
    }
    Ok(ShareMergeReport {
        branch: branch.to_owned(),
        git_branch: git_branch.to_owned(),
        remote_name: remote_name.to_owned(),
        project_key: expected_project_key.to_owned(),
        imported_turn_count,
        skipped_turn_count,
        warning_count: u64::try_from(warnings.len()).context("warning count exceeds u64 range")?,
        warnings,
    })
}

/// Returns the imported-turn identity used when pruning shared rows.
fn sync_turn_prune_key(entry: &SyncTurnEntry) -> (darc_paths::SourceKind, String, i64) {
    (entry.provider, entry.session_id.clone(), entry.turn_ordinal)
}

/// Imports decoded turns and updates per-exporter keep state.
#[allow(clippy::too_many_arguments)]
fn import_decoded_turns(
    connection: &mut rusqlite::Connection,
    context: &ShareProjectContext,
    origin_remote: &str,
    user: &ShareUserRecord,
    imported_at: &str,
    decoded_turns: &[ShareTurnExport],
    keep_turns: &mut BTreeSet<(darc_paths::SourceKind, String, i64)>,
    imported_turn_count: &mut u64,
    skipped_turn_count: &mut u64,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let imports = decoded_turns
        .iter()
        .map(|turn| ShareTurnImport {
            project_id: &context.project_id,
            user,
            remote_name: origin_remote,
            imported_at,
            turn,
        })
        .collect::<Vec<_>>();
    let outcomes = import_shared_turns(connection, &imports)?;
    for (turn, outcome) in decoded_turns.iter().zip(outcomes) {
        match outcome {
            Ok(true) => {
                *imported_turn_count += 1;
                keep_turns.insert((
                    turn.session.provider,
                    turn.session.session_id.clone(),
                    turn.turn_ordinal,
                ));
            }
            Ok(false) => *skipped_turn_count += 1,
            Err(error) => {
                *skipped_turn_count += 1;
                warnings.push(format!(
                    "skipped {} session {} turn {}: {error:#}",
                    turn.session.provider.directory_name(),
                    turn.session.session_id,
                    turn.turn_ordinal
                ));
            }
        }
    }
    Ok(())
}

/// Imports a chunked manifest one chunk at a time to bound memory.
#[allow(clippy::too_many_arguments)]
fn import_chunked_manifest_turns(
    connection: &mut rusqlite::Connection,
    context: &ShareProjectContext,
    cache_path: &Path,
    expected_project_key: &str,
    identity: &Identity,
    manifest: &ManifestArtifact,
    expected_exporter: &ShareIdentity,
    relative_path: &str,
    origin_remote: &str,
    user: &ShareUserRecord,
    imported_at: &str,
    keep_turns: &mut BTreeSet<(darc_paths::SourceKind, String, i64)>,
    imported_turn_count: &mut u64,
    skipped_turn_count: &mut u64,
    warnings: &mut Vec<String>,
    session_progress: &mut ImportSessionProgress,
    progress: &mut impl FnMut(SharePullProgress),
) -> Result<bool> {
    let mut manifest_decode_complete = true;
    let mut entries_by_chunk = BTreeMap::<String, Vec<&TurnManifestEntry>>::new();
    for entry in &manifest.turns {
        if let Some(chunk_id) = &entry.chunk_id {
            entries_by_chunk
                .entry(chunk_id.clone())
                .or_default()
                .push(entry);
        } else {
            manifest_decode_complete = false;
            *skipped_turn_count += 1;
            warnings.push(format!(
                "skipped {} session {} turn {}: chunked share manifest entry is missing chunk_id",
                entry.provider.directory_name(),
                entry.session_id,
                entry.turn_ordinal
            ));
            session_progress.finish_entry(manifest, entry, progress);
        }
    }

    for (chunk_id, entries) in entries_by_chunk {
        let decoded_turns = match read_manifest_chunk_for_entries(
            cache_path,
            manifest,
            expected_project_key,
            identity,
            &chunk_id,
            &entries,
        ) {
            Ok(turns) => {
                let decoded = DecodedChunks {
                    turns,
                    errors: BTreeMap::new(),
                };
                let import_context = ImportEntryContext {
                    expected_project_key,
                    expected_exporter,
                    identity,
                    cache_path,
                    chunks: &decoded,
                };
                let (turns, complete) = decode_manifest_entries(
                    &import_context,
                    &entries,
                    skipped_turn_count,
                    warnings,
                );
                manifest_decode_complete &= complete;
                turns
            }
            Err(error) => {
                manifest_decode_complete = false;
                *skipped_turn_count +=
                    u64::try_from(entries.len()).context("skipped turn count exceeds u64 range")?;
                warnings.push(format!(
                    "skipped share chunk {chunk_id} in manifest {relative_path} for exporter {}: {error:#}",
                    expected_exporter.user_id
                ));
                Vec::new()
            }
        };
        session_progress.finish_entries(manifest, &entries, progress);
        import_decoded_turns(
            connection,
            context,
            origin_remote,
            user,
            imported_at,
            &decoded_turns,
            keep_turns,
            imported_turn_count,
            skipped_turn_count,
            warnings,
        )?;
    }
    Ok(manifest_decode_complete)
}

/// Decodes manifest entries against already-decoded chunk content.
fn decode_manifest_entries(
    import_context: &ImportEntryContext<'_>,
    entries: &[&TurnManifestEntry],
    skipped_turn_count: &mut u64,
    warnings: &mut Vec<String>,
) -> (Vec<ShareTurnExport>, bool) {
    let mut decoded_turns = Vec::new();
    let mut complete = true;
    for entry in entries {
        match read_manifest_entry_turn(import_context, entry) {
            Ok(turn) => decoded_turns.push(turn),
            Err(error) => {
                complete = false;
                *skipped_turn_count += 1;
                warnings.push(format!(
                    "skipped {} session {} turn {}: {error:#}",
                    entry.provider.directory_name(),
                    entry.session_id,
                    entry.turn_ordinal
                ));
            }
        }
    }
    (decoded_turns, complete)
}

/// Reads and validates the encrypted sync payload used for pruning.
fn read_sync_payload(
    cache_path: &Path,
    manifest: &ManifestArtifact,
    expected_project_key: &str,
    identity: &Identity,
) -> Result<EncryptedSyncPayload> {
    let sync_path = manifest_artifact_path(cache_path, &manifest.sync.object_path)?;
    let ciphertext = read_regular_file(&sync_path, MAX_SHARE_OBJECT_BYTES)?;
    ensure_not_lfs_pointer(&ciphertext, &sync_path)?;
    let plaintext =
        decrypt_payload(&ciphertext, identity).context("failed to decrypt share sync object")?;
    if sha256_hex(&plaintext) != manifest.sync.payload_hash {
        bail!("share sync payload hash mismatch");
    }
    let payload: EncryptedSyncPayload =
        serde_json::from_slice(&plaintext).context("failed to parse share sync payload JSON")?;
    if payload.schema != SYNC_PAYLOAD_SCHEMA {
        bail!("unsupported share sync payload schema `{}`", payload.schema);
    }
    if payload.version != SYNC_PAYLOAD_VERSION {
        bail!(
            "unsupported share sync payload version `{}`",
            payload.version
        );
    }
    if payload.project_key != expected_project_key {
        bail!("share sync payload project key does not match active project");
    }
    if payload.exporter != manifest.exporter {
        bail!("share sync payload exporter does not match manifest exporter");
    }
    verify_sync_payload_signature(&payload)?;
    Ok(payload)
}

/// Reads and validates one manifest entry from an encrypted object file.
fn read_manifest_entry_turn(
    context: &ImportEntryContext<'_>,
    entry: &TurnManifestEntry,
) -> Result<ShareTurnExport> {
    if !context.chunks.turns.is_empty() || !context.chunks.errors.is_empty() {
        return verify_chunked_manifest_entry(context.chunks, entry);
    }
    if entry.chunk_id.is_some() || entry.chunk_record_index.is_some() {
        bail!("legacy share manifest entry unexpectedly references a chunk");
    }
    let object_path = manifest_object_path(context.cache_path, entry)?;
    let ciphertext = read_regular_file(&object_path, MAX_SHARE_OBJECT_BYTES)?;
    ensure_not_lfs_pointer(&ciphertext, &object_path)?;
    let plaintext =
        decrypt_payload(&ciphertext, context.identity).context("failed to decrypt share object")?;
    if sha256_hex(&plaintext) != entry.payload_hash {
        bail!("share payload hash mismatch");
    }
    let payload: EncryptedTurnPayload =
        serde_json::from_slice(&plaintext).context("failed to parse share payload JSON")?;
    if payload.schema != TURN_PAYLOAD_SCHEMA {
        bail!("unsupported share payload schema `{}`", payload.schema);
    }
    if payload.version != 1 {
        bail!("unsupported share payload version `{}`", payload.version);
    }
    if payload.project_key != context.expected_project_key {
        bail!("share payload project key does not match active project");
    }
    if payload.exporter != *context.expected_exporter {
        bail!("share payload exporter does not match sync payload exporter");
    }
    verify_turn_payload_signature(&payload)?;
    if payload.turn.session.provider != entry.provider
        || payload.turn.session.session_id != entry.session_id
        || payload.turn.turn_ordinal != entry.turn_ordinal
        || payload.turn.started_at != entry.started_at
    {
        bail!("share payload identity does not match manifest entry");
    }
    Ok(payload.turn)
}

/// Reads every encrypted chunk for a manifest and indexes its decoded turn payloads.
fn read_manifest_chunks(
    cache_path: &Path,
    manifest: &ManifestArtifact,
    expected_project_key: &str,
    identity: &Identity,
) -> DecodedChunks {
    let mut decoded = DecodedChunks::default();
    let mut entries_by_chunk = BTreeMap::<String, Vec<&TurnManifestEntry>>::new();
    for entry in &manifest.turns {
        if let Some(chunk_id) = &entry.chunk_id {
            entries_by_chunk
                .entry(chunk_id.clone())
                .or_default()
                .push(entry);
        }
    }
    for (chunk_id, entries) in entries_by_chunk {
        match read_manifest_chunk_for_entries(
            cache_path,
            manifest,
            expected_project_key,
            identity,
            &chunk_id,
            &entries,
        ) {
            Ok(turns) => decoded.turns.extend(turns),
            Err(error) => {
                decoded
                    .errors
                    .insert(chunk_id.clone(), format!("{error:#}"));
            }
        }
    }
    decoded
}

/// Reads one encrypted chunk selected by authenticated turn entries.
fn read_manifest_chunk_for_entries(
    cache_path: &Path,
    manifest: &ManifestArtifact,
    expected_project_key: &str,
    identity: &Identity,
    chunk_id: &str,
    entries: &[&TurnManifestEntry],
) -> Result<BTreeMap<(String, u32), DecodedChunkTurn>> {
    let object_path = chunk_object_path_for_entries(chunk_id, entries)?;
    read_manifest_chunk(
        cache_path,
        manifest,
        expected_project_key,
        identity,
        chunk_id,
        &object_path,
    )
}

/// Returns the single signed object path used by entries in one chunk.
fn chunk_object_path_for_entries(chunk_id: &str, entries: &[&TurnManifestEntry]) -> Result<String> {
    let first = entries
        .first()
        .with_context(|| format!("share chunk `{chunk_id}` has no manifest entries"))?;
    let object_path = first.object_path.clone();
    for entry in entries {
        if entry.object_path != object_path {
            bail!("share chunk entries disagree on object path");
        }
    }
    Ok(object_path)
}

/// Reads one encrypted chunk and indexes its decoded turn payloads.
fn read_manifest_chunk(
    cache_path: &Path,
    manifest: &ManifestArtifact,
    expected_project_key: &str,
    identity: &Identity,
    chunk_id: &str,
    object_path: &str,
) -> Result<BTreeMap<(String, u32), DecodedChunkTurn>> {
    let path = manifest_artifact_path(cache_path, object_path)?;
    let ciphertext = read_regular_file(&path, MAX_SHARE_OBJECT_BYTES)?;
    ensure_not_lfs_pointer(&ciphertext, &path)?;
    let compressed =
        decrypt_payload(&ciphertext, identity).context("failed to decrypt share chunk")?;
    let plaintext = gzip_decompress(&compressed)?;
    let payload: ShareChunkPayload =
        serde_json::from_slice(&plaintext).context("failed to parse share chunk JSON")?;
    if payload.schema != CHUNK_PAYLOAD_SCHEMA {
        bail!(
            "unsupported share chunk payload schema `{}`",
            payload.schema
        );
    }
    if payload.version != CHUNK_PAYLOAD_VERSION {
        bail!(
            "unsupported share chunk payload version `{}`",
            payload.version
        );
    }
    if payload.project_key != expected_project_key {
        bail!("share chunk project key does not match active project");
    }
    if payload.exporter != manifest.exporter {
        bail!("share chunk exporter does not match manifest exporter");
    }
    if payload.chunk_id != chunk_id {
        bail!("share chunk id does not match manifest chunk entry");
    }
    let mut decoded = BTreeMap::new();
    for (record_index, turn_payload) in payload.turns.into_iter().enumerate() {
        if turn_payload.schema != TURN_PAYLOAD_SCHEMA {
            bail!(
                "unsupported share payload schema `{}` in chunk",
                turn_payload.schema
            );
        }
        if turn_payload.version != 1 {
            bail!(
                "unsupported share payload version `{}` in chunk",
                turn_payload.version
            );
        }
        if turn_payload.project_key != expected_project_key {
            bail!("share payload project key in chunk does not match active project");
        }
        if turn_payload.exporter != manifest.exporter {
            bail!("share payload exporter in chunk does not match manifest exporter");
        }
        verify_turn_payload_signature(&turn_payload)?;
        let payload_bytes = serde_json::to_vec(&turn_payload)
            .context("failed to serialize decoded share chunk turn")?;
        let payload_hash = sha256_hex(&payload_bytes);
        decoded.insert(
            (
                chunk_id.to_owned(),
                u32::try_from(record_index)
                    .context("share chunk record index exceeds u32 range")?,
            ),
            DecodedChunkTurn {
                object_path: object_path.to_owned(),
                payload_hash,
                turn: turn_payload.turn,
            },
        );
    }
    Ok(decoded)
}

/// Verifies and returns one decoded chunked manifest entry.
fn verify_chunked_manifest_entry(
    chunks: &DecodedChunks,
    entry: &TurnManifestEntry,
) -> Result<ShareTurnExport> {
    let chunk_id = entry
        .chunk_id
        .as_ref()
        .context("chunked share manifest entry is missing chunk_id")?;
    if let Some(error) = chunks.errors.get(chunk_id) {
        bail!("share chunk `{chunk_id}` could not be imported: {error}");
    }
    let record_index = entry
        .chunk_record_index
        .context("chunked share manifest entry is missing chunk_record_index")?;
    let decoded = chunks
        .turns
        .get(&(chunk_id.clone(), record_index))
        .context("share chunk does not contain manifest entry")?;
    if decoded.object_path != entry.object_path {
        validate_manifest_object_relative_path(&entry.object_path)?;
        bail!("share chunked manifest object path does not match chunk entry");
    }
    if decoded.payload_hash != entry.payload_hash {
        bail!("share payload hash mismatch");
    }
    if decoded.turn.session.provider != entry.provider
        || decoded.turn.session.session_id != entry.session_id
        || decoded.turn.turn_ordinal != entry.turn_ordinal
        || decoded.turn.started_at != entry.started_at
    {
        bail!("share payload identity does not match manifest entry");
    }
    Ok(decoded.turn.clone())
}

/// Resolves the remote URL for one share operation.
fn resolve_remote(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    remote_name: Option<&str>,
) -> Result<ResolvedRemote> {
    if let Some(remote_name) = remote_name {
        let remote = settings
            .remotes
            .iter()
            .find(|remote| remote.name == remote_name)
            .with_context(|| format!("Darc share remote `{remote_name}` is not configured"))?;
        return resolved_remote(&context.local_path, &remote.name, &remote.url);
    }
    if let Some(url) = context.git_upstream.clone() {
        return resolved_remote(&context.local_path, DEFAULT_REMOTE_NAME, &url);
    }
    let url = origin_configured_remote_url(&context.local_path)
        .context("active project has no git_upstream and no origin remote")?;
    resolved_remote(&context.local_path, DEFAULT_REMOTE_NAME, &url)
}

/// Builds one remote target from a configured URL and one rewritten URL lookup.
fn resolved_remote(project_path: &Path, name: &str, url: &str) -> Result<ResolvedRemote> {
    validate_share_remote_url(url)?;
    let resolved_url = resolved_remote_url(project_path, url)?;
    let cache_url = cache_remote_url_from_resolved(&resolved_url)?;
    Ok(ResolvedRemote {
        name: name.to_owned(),
        display_url: sanitize_git_url_for_display(url),
        resolved_url,
        cache_url,
        #[cfg(test)]
        url: url.to_owned(),
    })
}

/// Returns the canonical shared-project key for one active project.
fn project_key(context: &ShareProjectContext) -> Result<String> {
    let url = if let Some(url) = context.git_upstream.as_deref() {
        resolved_remote_url(&context.local_path, url)?
    } else {
        origin_effective_remote_url(&context.local_path)
            .context("active project has no git_upstream and no origin remote")?
    };
    Ok(format!("git:{}", normalize_git_url(&url)?))
}

/// Normalizes one Git URL enough for Darc project matching.
fn normalize_git_url(url: &str) -> Result<String> {
    let trimmed = strip_url_query_fragment(url.trim())
        .trim_end_matches('/')
        .trim_end_matches(".git");
    if let Some(normalized) = normalize_scp_like_git_url(trimmed) {
        return Ok(normalized);
    }
    if let Some(normalized) = normalize_scheme_git_url(trimmed, "ssh://", "https") {
        return Ok(normalized);
    }
    if let Some(normalized) = normalize_scheme_git_url(trimmed, "https://", "https") {
        return Ok(normalized);
    }
    if let Some(normalized) = normalize_scheme_git_url(trimmed, "http://", "http") {
        return Ok(normalized);
    }
    bail!(
        "Darc share project keys require an ssh, https, or http Git remote; refusing to publish unsupported or local remote `{}` in visible share metadata",
        sanitize_git_url_for_display(trimmed)
    )
}

/// Removes URL query and fragment suffixes before URLs become visible metadata.
fn strip_url_query_fragment(url: &str) -> &str {
    url.find(['?', '#']).map_or(url, |index| &url[..index])
}

/// Normalizes one SSH scp-like Git URL.
fn normalize_scp_like_git_url(url: &str) -> Option<String> {
    if url.contains("://") {
        return None;
    }
    let (user_host, path) = url.split_once(':')?;
    let host = user_host
        .rsplit_once('@')
        .map_or(user_host, |(_, host)| host);
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!(
        "https://{}/{}",
        host.to_ascii_lowercase(),
        path.trim_start_matches('/')
    ))
}

/// Normalizes one scheme Git URL while removing credential userinfo.
fn normalize_scheme_git_url(url: &str, input_scheme: &str, output_scheme: &str) -> Option<String> {
    if !url
        .get(..input_scheme.len())?
        .eq_ignore_ascii_case(input_scheme)
    {
        return None;
    }
    let rest = &url[input_scheme.len()..];
    let (authority, path) = rest.split_once('/')?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .to_ascii_lowercase();
    Some(format!(
        "{output_scheme}://{host}/{}",
        path.trim_start_matches('/')
    ))
}

/// Returns a remote URL suitable for terminal output.
pub fn sanitize_git_url_for_display(url: &str) -> String {
    let trimmed = strip_url_query_fragment(url.trim());
    if let Some((scheme, rest)) = trimmed.split_once("://") {
        let (authority, path) = rest
            .split_once('/')
            .map_or((rest, None), |(authority, path)| (authority, Some(path)));
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        return path.map_or_else(
            || format!("{scheme}://{host}"),
            |path| format!("{scheme}://{host}/{path}"),
        );
    }
    if let Some((user_host, path)) = trimmed.split_once(':')
        && !trimmed.contains("://")
        && let Some((user, host)) = user_host.rsplit_once('@')
    {
        return format!("{user}@{host}:{path}");
    }
    trimmed.to_owned()
}

/// Rejects share remote URLs that would persist credential-bearing URL parts.
pub fn validate_share_remote_url(url: &str) -> Result<()> {
    let trimmed = url.trim();
    let display_url = sanitize_git_url_for_display(trimmed);
    if trimmed.contains(['?', '#']) {
        bail!(
            "share remote URL `{display_url}` must not include query strings or fragments; configure Git credentials outside the URL"
        );
    }
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return Ok(());
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let scheme = scheme.to_ascii_lowercase();
    let userinfo = authority.rsplit_once('@').map(|(userinfo, _)| userinfo);
    if userinfo.is_some_and(|userinfo| {
        matches!(scheme.as_str(), "http" | "https") || scheme != "ssh" || userinfo.contains(':')
    }) {
        bail!(
            "share remote URL `{display_url}` must not include URL credentials; configure Git credentials outside the URL"
        );
    }
    Ok(())
}

/// Reads and parses one age identity file.
fn read_share_identity_key(path: &Path) -> Result<Identity> {
    ensure_regular_private_key_file(path)?;
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Identity::from_str(content.trim()).map_err(|error| anyhow::anyhow!("{error}"))
}

/// Reads and parses one Ed25519 share signing key file.
fn read_share_signing_key(path: &Path) -> Result<SigningKey> {
    ensure_regular_private_key_file(path)?;
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let seed = hex_decode_fixed::<32>(content.trim())
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Writes one age identity file with private-key permissions on Unix.
fn write_share_identity_key(path: &Path, content: &str) -> Result<()> {
    write_share_private_key(path, content)
}

/// Writes one share private-key file with private permissions on Unix.
fn write_share_private_key(path: &Path, content: &str) -> Result<()> {
    #[cfg(unix)]
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("failed to write {}", path.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))
    }
}

/// Restricts one private key file to the current user on Unix.
fn harden_private_key_permissions(path: &Path) -> Result<()> {
    ensure_regular_private_key_file(path)?;
    #[cfg(unix)]
    {
        let mut permissions = fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect {}", path.display()))?
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

/// Rejects missing, symlinked, or non-regular private key files.
fn ensure_regular_private_key_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!("share private key path is a symlink: {}", path.display());
    }
    if !file_type.is_file() {
        bail!(
            "share private key path is not a regular file: {}",
            path.display()
        );
    }
    Ok(())
}

/// Creates and validates a private-key directory without following symlinked ancestors.
fn ensure_safe_private_key_directory(root: &Path, directory: &Path) -> Result<PathBuf> {
    let relative = directory.strip_prefix(root).with_context(|| {
        format!(
            "share private key directory {} is outside root {}",
            directory.display(),
            root.display()
        )
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            bail!("share private key directory contains unsafe path components");
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_symlink() {
                    bail!(
                        "share private key directory is a symlink: {}",
                        current.display()
                    );
                }
                if !file_type.is_dir() {
                    bail!(
                        "share private key directory path is not a directory: {}",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)
                    .with_context(|| format!("failed to create {}", current.display()))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", current.display()));
            }
        }
    }
    Ok(directory.to_path_buf())
}

/// Restricts one age identity file to the current user on Unix.
fn harden_share_key_permissions(path: &Path) -> Result<()> {
    harden_private_key_permissions(path)
}

/// Builds sorted encryption recipient strings from local identity and configured teammates.
fn encryption_recipient_strings(identity: &ShareIdentity, settings: &ShareSettings) -> Vec<String> {
    let mut recipient_strings = BTreeSet::new();
    recipient_strings.insert(identity.public_key.clone());
    for recipient in &settings.recipients {
        recipient_strings.insert(recipient.recipient.clone());
    }
    recipient_strings.into_iter().collect()
}

/// Parses age recipients from sorted recipient strings.
fn parse_encryption_recipients(recipient_strings: &[String]) -> Result<Vec<Recipient>> {
    recipient_strings
        .iter()
        .map(|recipient| Recipient::from_str(recipient).map_err(|error| anyhow::anyhow!("{error}")))
        .collect()
}

/// Returns a short stable fingerprint for the recipient set used by an object.
fn encryption_recipient_fingerprint(recipient_strings: &[String]) -> String {
    sha256_hex(recipient_strings.join("\n").as_bytes())[..16].to_owned()
}

/// Compresses one share chunk before encryption.
fn gzip_compress(plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder
        .write_all(plaintext)
        .context("failed to write gzip share chunk")?;
    encoder
        .finish()
        .context("failed to finish gzip share chunk")
}

/// Decompresses one decrypted share chunk.
fn gzip_decompress(compressed: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(compressed);
    let mut plaintext = Vec::new();
    decoder
        .by_ref()
        .take(MAX_SHARE_CHUNK_DECOMPRESSED_BYTES + 1)
        .read_to_end(&mut plaintext)
        .context("failed to decompress share chunk")?;
    if u64::try_from(plaintext.len()).unwrap_or(u64::MAX) > MAX_SHARE_CHUNK_DECOMPRESSED_BYTES {
        bail!(
            "decompressed share chunk exceeds maximum supported size of {MAX_SHARE_CHUNK_DECOMPRESSED_BYTES} bytes"
        );
    }
    Ok(plaintext)
}

/// Encrypts one plaintext payload to every configured recipient.
fn encrypt_payload(plaintext: &[u8], recipients: &[Recipient]) -> Result<Vec<u8>> {
    if recipients.is_empty() {
        bail!("at least one share recipient is required");
    }
    let encryptor = age::Encryptor::with_recipients(
        recipients
            .iter()
            .map(|recipient| recipient as &dyn age::Recipient),
    )
    .context("failed to create age encryptor")?;
    let mut encrypted = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut encrypted)
        .context("failed to start age encryption")?;
    writer
        .write_all(plaintext)
        .context("failed to write age plaintext")?;
    writer.finish().context("failed to finish age encryption")?;
    Ok(encrypted)
}

/// Decrypts one encrypted payload with the local identity.
fn decrypt_payload(ciphertext: &[u8], identity: &Identity) -> Result<Vec<u8>> {
    let decryptor = age::Decryptor::new(ciphertext).context("failed to read age payload")?;
    let mut reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .context("failed to create age decryptor")?;
    let mut plaintext = Vec::new();
    reader
        .read_to_end(&mut plaintext)
        .context("failed to read decrypted share payload")?;
    Ok(plaintext)
}

/// Signs one turn payload with the local share signing key.
fn sign_turn_payload(payload: &mut EncryptedTurnPayload, signing_key: &SigningKey) -> Result<()> {
    payload.signature = None;
    let unsigned =
        serde_json::to_vec(payload).context("failed to serialize unsigned turn payload")?;
    payload.signature = Some(sign_bytes(signing_key, TURN_SIGNATURE_DOMAIN, &unsigned));
    Ok(())
}

/// Signs one sync payload with the local share signing key.
fn sign_sync_payload(payload: &mut EncryptedSyncPayload, signing_key: &SigningKey) -> Result<()> {
    payload.signature = None;
    let unsigned =
        serde_json::to_vec(payload).context("failed to serialize unsigned sync payload")?;
    payload.signature = Some(sign_bytes(signing_key, SYNC_SIGNATURE_DOMAIN, &unsigned));
    Ok(())
}

/// Verifies one decrypted turn payload signature.
fn verify_turn_payload_signature(payload: &EncryptedTurnPayload) -> Result<()> {
    let mut unsigned_payload = payload.clone();
    let signature = unsigned_payload
        .signature
        .take()
        .context("share turn payload is missing an exporter signature")?;
    let unsigned = serde_json::to_vec(&unsigned_payload)
        .context("failed to serialize unsigned turn payload")?;
    verify_payload_signature(
        &payload.exporter,
        TURN_SIGNATURE_DOMAIN,
        &unsigned,
        &signature,
    )
}

/// Verifies one decrypted sync payload signature.
fn verify_sync_payload_signature(payload: &EncryptedSyncPayload) -> Result<()> {
    let mut unsigned_payload = payload.clone();
    let signature = unsigned_payload
        .signature
        .take()
        .context("share sync payload is missing an exporter signature")?;
    let unsigned = serde_json::to_vec(&unsigned_payload)
        .context("failed to serialize unsigned sync payload")?;
    verify_payload_signature(
        &payload.exporter,
        SYNC_SIGNATURE_DOMAIN,
        &unsigned,
        &signature,
    )
}

/// Signs one domain-separated payload byte sequence.
fn sign_bytes(signing_key: &SigningKey, domain: &[u8], unsigned: &[u8]) -> String {
    let signature: Signature = signing_key.sign(&signature_message(domain, unsigned));
    hex_encode(&signature.to_bytes())
}

/// Verifies one domain-separated payload signature against the exporter identity.
fn verify_payload_signature(
    exporter: &ShareIdentity,
    domain: &[u8],
    unsigned: &[u8],
    signature: &str,
) -> Result<()> {
    if derive_user_id(&exporter.signing_public_key) != exporter.user_id {
        bail!("share payload exporter user_id does not match signing key");
    }
    let public_key = hex_decode_fixed::<32>(&exporter.signing_public_key)
        .context("share payload exporter signing key is not valid hex")?;
    let verifying_key = VerifyingKey::from_bytes(&public_key)
        .context("share payload exporter signing key is invalid")?;
    let signature_bytes =
        hex_decode_fixed::<64>(signature).context("share payload signature is not valid hex")?;
    let signature = Signature::from_bytes(&signature_bytes);
    verifying_key
        .verify(&signature_message(domain, unsigned), &signature)
        .context("share payload exporter signature is invalid")
}

/// Builds the exact byte string signed by share payloads.
fn signature_message(domain: &[u8], unsigned: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(domain.len() + 1 + unsigned.len());
    message.extend_from_slice(domain);
    message.push(b'\n');
    message.extend_from_slice(unsigned);
    message
}

/// Returns the hex-encoded Ed25519 verifying key.
fn signing_public_key_hex(signing_key: &SigningKey) -> String {
    hex_encode(&signing_key.verifying_key().to_bytes())
}

/// Verifies that a path resolves inside a Git repository.
fn ensure_git_repository(path: &Path) -> Result<()> {
    run_git(
        path,
        ["rev-parse", "--git-dir"],
        &format!("failed to discover Git repository from {}", path.display()),
    )?;
    Ok(())
}

/// Reads one optional Git config value through the user's Git client.
fn git_config_value(path: &Path, key: &str) -> Result<Option<String>> {
    let output = run_git_raw(path, ["config", "--get", key])
        .with_context(|| format!("failed to read Git config `{key}`"))?;
    if output.status.success() {
        let value = output.stdout.trim().to_owned();
        return Ok((!value.is_empty()).then_some(value));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    bail!(
        "{}",
        git_failure_message(&format!("failed to read Git config `{key}`"), &output)
    )
}

/// Reads the active repository's configured origin URL without expanding rewrites.
fn origin_configured_remote_url(path: &Path) -> Result<String> {
    ensure_git_repository(path)?;
    let output = run_git_raw(path, ["config", "--get", "remote.origin.url"])
        .context("failed to read configured origin remote URL")?;
    if !output.status.success() {
        if output.status.code() == Some(1) {
            bail!("origin remote URL is not configured");
        }
        bail!(
            "{}",
            git_failure_message("failed to read configured origin remote URL", &output)
        );
    }
    let value = output.stdout.trim().to_owned();
    (!value.is_empty())
        .then_some(value)
        .context("origin remote URL is empty")
}

/// Reads the active repository's effective origin URL through Git URL rewrites.
fn origin_effective_remote_url(path: &Path) -> Result<String> {
    ensure_git_repository(path)?;
    let output = run_git(
        path,
        ["remote", "get-url", DEFAULT_REMOTE_NAME],
        "failed to read origin remote URL",
    )
    .context("active project has no origin remote URL configured")?;
    let value = output.stdout.trim().to_owned();
    (!value.is_empty())
        .then_some(value)
        .context("origin remote URL is empty")
}

/// Resolves one remote URL through Git URL rewrite configuration without contacting the remote.
fn resolved_remote_url(path: &Path, url: &str) -> Result<String> {
    let output = run_git(
        path,
        ["ls-remote", "--get-url", url],
        "failed to resolve Git remote URL",
    )?;
    let value = output.stdout.trim().to_owned();
    let resolved = if value.is_empty() {
        url.to_owned()
    } else {
        value
    };
    Ok(resolve_local_git_path_url(path, &resolved))
}

/// Returns the credential-free URL written into one share cache Git remote.
fn cache_remote_url_from_resolved(resolved: &str) -> Result<String> {
    let cache_url = sanitize_git_url_for_cache_remote(resolved);
    validate_share_remote_url(&cache_url)?;
    Ok(cache_url)
}

/// Resolves one local Git path URL against the active project path.
fn resolve_local_git_path_url(project_path: &Path, url: &str) -> String {
    let candidate = Path::new(url);
    if url.contains("://") || normalize_scp_like_git_url(url).is_some() || candidate.is_absolute() {
        return url.to_owned();
    }
    project_path.join(candidate).to_string_lossy().into_owned()
}

/// Removes credential-bearing URL parts before persisting a cache Git remote.
fn sanitize_git_url_for_cache_remote(url: &str) -> String {
    let trimmed = strip_url_query_fragment(url.trim());
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return trimmed.to_owned();
    };
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, None), |(authority, path)| (authority, Some(path)));
    let scheme_lower = scheme.to_ascii_lowercase();
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(userinfo, host)| {
            if scheme_lower == "ssh" && !userinfo.contains(':') {
                authority
            } else {
                host
            }
        });
    path.map_or_else(
        || format!("{scheme}://{authority}"),
        |path| format!("{scheme}://{authority}/{path}"),
    )
}

/// Prepares one local Git cache repository.
fn prepare_cache_repository(
    path: &Path,
    remote_url: &str,
    source_repo_path: &Path,
    identity: &ShareIdentity,
) -> Result<()> {
    create_safe_cache_repository_dir(path)?;
    if path.join(".git").exists() {
        ensure_safe_cache_git_dir(path)?;
        run_cache_git(
            path,
            ["rev-parse", "--git-dir"],
            "failed to open share cache repository",
        )?;
    } else {
        run_git(path, ["init"], "failed to init share cache repository")?;
        ensure_safe_cache_git_dir(path)?;
    }
    configure_cache_repository(path, remote_url, source_repo_path, identity)
}

/// Creates one cache repository root without following symlinked ancestors.
fn create_safe_cache_repository_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_safe_ancestor_dir_all(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!("share cache path is a symlink: {}", path.display());
            }
            if !file_type.is_dir() {
                bail!("share cache path is not a directory: {}", path.display());
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).with_context(|| format!("failed to create {}", path.display()))
        }
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

/// Creates parent directories while rejecting symlinked existing ancestors.
fn create_safe_ancestor_dir_all(path: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        match fs::symlink_metadata(current) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_symlink() {
                    bail!("share cache ancestor is a symlink: {}", current.display());
                }
                if !file_type.is_dir() {
                    bail!(
                        "share cache ancestor is not a directory: {}",
                        current.display()
                    );
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(current.to_path_buf());
                current = current
                    .parent()
                    .context("share cache path is missing an existing ancestor")?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", current.display()));
            }
        }
    }
    for directory in missing.iter().rev() {
        fs::create_dir(directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
    }
    Ok(())
}

/// Verifies one existing cache root is a real directory, not a symlink.
fn ensure_safe_existing_cache_dir(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!("share cache path is a symlink: {}", path.display());
            }
            if !file_type.is_dir() {
                bail!("share cache path is not a directory: {}", path.display());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

/// Verifies the share cache `.git` directory is a normal directory.
fn ensure_safe_cache_git_dir(path: &Path) -> Result<()> {
    let git_dir = path.join(".git");
    let metadata = fs::symlink_metadata(&git_dir)
        .with_context(|| format!("failed to inspect {}", git_dir.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!(
            "share cache Git directory is a symlink: {}",
            git_dir.display()
        );
    }
    if !file_type.is_dir() {
        bail!(
            "share cache Git path is not a directory: {}",
            git_dir.display()
        );
    }
    Ok(())
}

/// Configures remote and author identity for one cache repository.
fn configure_cache_repository(
    path: &Path,
    remote_url: &str,
    source_repo_path: &Path,
    identity: &ShareIdentity,
) -> Result<()> {
    if run_cache_git_raw(path, ["remote", "get-url", DEFAULT_REMOTE_NAME])
        .context("failed to inspect share cache remote")?
        .status
        .success()
    {
        run_cache_git(
            path,
            ["remote", "set-url", DEFAULT_REMOTE_NAME, remote_url],
            "failed to update share cache remote URL",
        )?;
    } else {
        run_cache_git(
            path,
            ["remote", "add", DEFAULT_REMOTE_NAME, remote_url],
            "failed to add share cache remote",
        )?;
    }
    run_cache_git(
        path,
        [
            "config",
            "user.name",
            identity.display_name.as_deref().unwrap_or("Darc Share"),
        ],
        "failed to set share cache user.name",
    )?;
    run_cache_git(
        path,
        [
            "config",
            "user.email",
            identity
                .email
                .as_deref()
                .unwrap_or("darc-share@example.invalid"),
        ],
        "failed to set share cache user.email",
    )?;
    run_cache_git(
        path,
        ["config", "commit.gpgsign", "false"],
        "failed to disable share cache commit signing",
    )?;
    configure_cache_ssh_command(path, source_repo_path)?;
    configure_git_lfs(path)?;
    Ok(())
}

/// Mirrors active-repository SSH transport config into one cache repository.
fn configure_cache_ssh_command(cache_path: &Path, source_repo_path: &Path) -> Result<()> {
    let Some(command) = git_core_ssh_command(source_repo_path) else {
        let output = run_cache_git_raw(cache_path, ["config", "--unset", "core.sshCommand"])
            .context("failed to clear stale share cache core.sshCommand")?;
        if output.status.success() || output.status.code() == Some(5) {
            return Ok(());
        }
        bail!(
            "{}",
            git_failure_message("failed to clear stale share cache core.sshCommand", &output)
        );
    };
    run_cache_git(
        cache_path,
        [
            OsString::from("config"),
            OsString::from("core.sshCommand"),
            command,
        ],
        "failed to copy active repository core.sshCommand into share cache",
    )?;
    Ok(())
}

/// Enables Git LFS filters for the share cache when LFS publishing is opted in.
fn configure_git_lfs(path: &Path) -> Result<bool> {
    if !git_lfs_publish_enabled(path)? {
        return Ok(false);
    }
    run_cache_git_with_hook_override(
        path,
        ["lfs", "install", "--local"],
        "failed to initialize Git LFS in share cache",
        false,
    )?;
    Ok(true)
}

/// Returns whether the system Git client can run git-lfs.
fn git_lfs_available(path: &Path) -> Result<bool> {
    let output = run_git_raw(path, ["lfs", "version"]).context("failed to inspect Git LFS")?;
    Ok(output.status.success())
}

/// Returns whether Darc should publish share objects through Git LFS.
fn git_lfs_publish_enabled(path: &Path) -> Result<bool> {
    if !git_lfs_publish_enabled_from_env(
        std::env::var_os("DARC_SHARE_ENABLE_LFS"),
        std::env::var_os("DARC_SHARE_DISABLE_LFS"),
        git_lfs_available(path)?,
    ) {
        return Ok(false);
    }
    Ok(true)
}

/// Resolves Git LFS publish opt-in flags against local git-lfs availability.
fn git_lfs_publish_enabled_from_env(
    enable: Option<OsString>,
    disable: Option<OsString>,
    available: bool,
) -> bool {
    disable.is_none() && enable.is_some() && available
}

/// Returns whether Darc can hydrate existing Git LFS share objects.
fn git_lfs_hydration_enabled(path: &Path) -> Result<bool> {
    if std::env::var_os("DARC_SHARE_DISABLE_LFS").is_some() {
        return Ok(false);
    }
    git_lfs_available(path)
}

/// Downloads referenced Git LFS share objects for one fetched cache checkout when supported.
fn hydrate_lfs_objects(path: &Path, object_paths: &BTreeSet<String>) -> Result<()> {
    if !ensure_safe_existing_cache_dir(path)? {
        return Ok(());
    }
    #[cfg(test)]
    assert_no_checked_out_lfs_config(path)?;
    if object_paths.is_empty() {
        return Ok(());
    }
    if !git_lfs_hydration_enabled(path)? {
        reject_lfs_pointer_objects(path, object_paths)?;
        return Ok(());
    }
    let include_paths = object_paths.iter().cloned().collect::<Vec<_>>().join(",");
    run_cache_git(
        path,
        [
            OsString::from("lfs"),
            OsString::from("pull"),
            OsString::from(DEFAULT_REMOTE_NAME),
            OsString::from(format!("--include={include_paths}")),
        ],
        "failed to hydrate Git LFS share objects",
    )?;
    reject_lfs_pointer_objects(path, object_paths)?;
    Ok(())
}

/// Rejects checked-out LFS pointer files for referenced encrypted objects.
fn reject_lfs_pointer_objects(cache_path: &Path, object_paths: &BTreeSet<String>) -> Result<()> {
    for object_path in object_paths {
        let relative = validate_manifest_object_relative_path(object_path)?;
        ensure_safe_artifact_ancestors(cache_path, relative)?;
        let path = cache_path.join(relative);
        if !path.exists() {
            continue;
        }
        let prefix = read_file_prefix(&path, u64::try_from(GIT_LFS_POINTER_PREFIX.len())?)?;
        if prefix == GIT_LFS_POINTER_PREFIX {
            bail!(
                "share object {} is a Git LFS pointer; install git-lfs and retry without DARC_SHARE_DISABLE_LFS so Darc can hydrate existing encrypted share objects before continuing",
                path.display()
            );
        }
    }
    Ok(())
}

/// Asserts tests have pruned local LFS config before hydration.
#[cfg(test)]
fn assert_no_checked_out_lfs_config(cache_path: &Path) -> Result<()> {
    let lfs_config = cache_path.join(".lfsconfig");
    if lfs_config.exists() {
        bail!(
            "share cache still contains .lfsconfig before Git LFS hydration: {}",
            lfs_config.display()
        );
    }
    Ok(())
}

/// Fetches a branch and treats a missing remote branch as a non-fatal first push case.
fn fetch_branch_if_exists(path: &Path, git_branch: &str) -> Result<bool> {
    match fetch_branch(path, git_branch) {
        Ok(()) => {
            let remote_ref = format!("refs/remotes/{DEFAULT_REMOTE_NAME}/{git_branch}");
            if !git_ref_exists(path, &remote_ref)? {
                clear_share_branch_refs(path, git_branch)?;
                return Ok(false);
            }
            Ok(true)
        }
        Err(error) if is_missing_remote_ref_error(&error) => {
            clear_share_branch_refs(path, git_branch)?;
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// Deletes stale local cache refs for one missing remote share branch.
fn clear_share_branch_refs(path: &Path, git_branch: &str) -> Result<()> {
    for reference_name in [
        format!("refs/heads/{git_branch}"),
        format!("refs/remotes/{DEFAULT_REMOTE_NAME}/{git_branch}"),
    ] {
        if git_ref_exists(path, &reference_name)? {
            run_cache_git(
                path,
                ["update-ref", "-d", &reference_name],
                &format!("failed to delete stale share cache ref `{reference_name}`"),
            )?;
        }
    }
    Ok(())
}

/// Removes every non-Git file from one share cache worktree.
fn clear_cache_worktree(path: &Path) -> Result<()> {
    if !ensure_safe_existing_cache_dir(path)? {
        return Ok(());
    }
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry.with_context(|| format!("failed to read {}", path.display()))?;
        if entry.file_name() == ".git" {
            continue;
        }
        let entry_path = entry.path();
        let metadata = fs::symlink_metadata(&entry_path)
            .with_context(|| format!("failed to inspect {}", entry_path.display()))?;
        if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(&entry_path)
                .with_context(|| format!("failed to remove {}", entry_path.display()))?;
        } else {
            fs::remove_file(&entry_path)
                .with_context(|| format!("failed to remove {}", entry_path.display()))?;
        }
    }
    Ok(())
}

/// Removes untracked and ignored files from one fetched share cache checkout.
fn clean_cached_checkout(path: &Path) -> Result<()> {
    if !ensure_safe_existing_cache_dir(path)? {
        return Ok(());
    }
    ensure_safe_cache_git_dir(path)?;
    reset_cached_checkout(path)?;
    clean_untracked_cache_worktree(path)
}

/// Resets one cache checkout to its current HEAD before importing artifacts.
fn reset_cached_checkout(path: &Path) -> Result<()> {
    let head = run_cache_git_raw(path, ["rev-parse", "--verify", "HEAD"])
        .context("failed to inspect share cache HEAD")?;
    if !head.status.success() {
        return Ok(());
    }
    run_cache_git_with_lfs_filter_override(
        path,
        ["reset", "--hard", "HEAD"],
        "failed to reset share cache checkout",
        true,
    )?;
    Ok(())
}

/// Removes worktree files that are not present in the checked-out Git tree.
fn clean_untracked_cache_worktree(cache_path: &Path) -> Result<()> {
    if !ensure_safe_existing_cache_dir(cache_path)? {
        return Ok(());
    }
    run_cache_git(
        cache_path,
        ["clean", "-ffdx"],
        "failed to clean untracked share cache files",
    )?;
    Ok(())
}

/// Removes checked-out files that are outside the share artifact layout.
fn clean_non_artifact_share_cache_files(path: &Path) -> Result<()> {
    clean_share_cache_files(path, allowed_share_cache_file)
}

/// Builds the exact cache-relative file set that may be published.
fn allowed_share_cache_paths(
    artifact: &BuiltExportArtifact,
    retained_manifests: &[CachedManifest],
) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    insert_allowed_share_cache_path(&mut paths, GIT_ATTRIBUTES_FILE);
    insert_allowed_share_cache_path(&mut paths, &format!("{ARTIFACT_ROOT}/{PROJECT_FILE}"));
    insert_allowed_share_cache_path(
        &mut paths,
        &exporter_manifest_relative_path(&artifact.manifest.exporter),
    );
    for object_path in &artifact.object_paths {
        insert_allowed_share_cache_path(&mut paths, object_path);
    }
    for cached in retained_manifests {
        insert_allowed_share_cache_path(&mut paths, &cached.relative_path);
        for object_path in manifest_object_paths(&cached.manifest) {
            insert_allowed_share_cache_path(&mut paths, &object_path);
        }
    }
    paths
}

/// Adds one validated cache-relative artifact path to a publish allowlist.
fn insert_allowed_share_cache_path(paths: &mut BTreeSet<String>, relative: &str) {
    if allowed_share_cache_file(Path::new(relative)) {
        paths.insert(relative.to_owned());
    }
}

/// Removes files outside the authenticated share artifact publish set.
fn clean_unexpected_share_cache_files(path: &Path, allowed_paths: &BTreeSet<String>) -> Result<()> {
    clean_share_cache_files(path, |relative| {
        cache_relative_path_key(relative)
            .as_ref()
            .is_some_and(|relative| allowed_paths.contains(relative))
    })
}

/// Removes cache files rejected by one cache-relative allow predicate.
fn clean_share_cache_files(path: &Path, keep_file: impl Fn(&Path) -> bool + Copy) -> Result<()> {
    if !ensure_safe_existing_cache_dir(path)? {
        return Ok(());
    }
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry.with_context(|| format!("failed to read {}", path.display()))?;
        if entry.file_name() == ".git" {
            continue;
        }
        clean_share_cache_entry(path, &entry.path(), keep_file)?;
    }
    Ok(())
}

/// Removes one rejected cache entry and prunes empty directories.
fn clean_share_cache_entry(
    cache_path: &Path,
    path: &Path,
    keep_file: impl Fn(&Path) -> bool + Copy,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return remove_file_if_exists(path);
    }
    if file_type.is_dir() {
        for entry in
            fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?
        {
            let entry = entry.with_context(|| format!("failed to read {}", path.display()))?;
            clean_share_cache_entry(cache_path, &entry.path(), keep_file)?;
        }
        if fs::read_dir(path)
            .with_context(|| format!("failed to read {}", path.display()))?
            .next()
            .is_none()
        {
            fs::remove_dir(path).with_context(|| format!("failed to remove {}", path.display()))?;
        }
        return Ok(());
    }
    let relative = path.strip_prefix(cache_path).with_context(|| {
        format!(
            "share cache path {} is outside cache {}",
            path.display(),
            cache_path.display()
        )
    })?;
    if keep_file(relative) {
        Ok(())
    } else {
        remove_file_if_exists(path)
    }
}

/// Returns whether one cache-relative file belongs to the share artifact layout.
fn allowed_share_cache_file(relative: &Path) -> bool {
    let Some(components) = cache_relative_path_components(relative) else {
        return false;
    };
    match components.as_slice() {
        [GIT_ATTRIBUTES_FILE] => true,
        ["darc-share", "v1", PROJECT_FILE] => true,
        ["darc-share", "v1", "objects", file_name] => file_name.ends_with(".age"),
        [
            "darc-share",
            "v1",
            EXPORTERS_DIR,
            exporter,
            LEGACY_MANIFEST_FILE,
        ] => !exporter.is_empty(),
        _ => false,
    }
}

/// Returns normalized string components for one safe cache-relative path.
fn cache_relative_path_components(relative: &Path) -> Option<Vec<&str>> {
    relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect()
}

/// Returns one slash-separated key for a cache-relative path.
fn cache_relative_path_key(relative: &Path) -> Option<String> {
    cache_relative_path_components(relative).map(|components| components.join("/"))
}

/// Fetches one remote share branch into the cache repository.
fn fetch_branch(path: &Path, git_branch: &str) -> Result<()> {
    let refspec =
        format!("+refs/heads/{git_branch}:refs/remotes/{DEFAULT_REMOTE_NAME}/{git_branch}");
    run_cache_git(
        path,
        ["fetch", "--prune", DEFAULT_REMOTE_NAME, &refspec],
        &format!("failed to fetch share branch `{git_branch}` with system git"),
    )?;
    Ok(())
}

/// Checks out one local share branch from remote state when possible.
fn checkout_share_branch(path: &Path, git_branch: &str) -> Result<()> {
    let local_ref = format!("refs/heads/{git_branch}");
    let remote_ref = format!("refs/remotes/{DEFAULT_REMOTE_NAME}/{git_branch}");
    if git_ref_exists(path, &remote_ref)? {
        run_cache_git(
            path,
            ["update-ref", &local_ref, &remote_ref],
            &format!("failed to update share branch `{git_branch}`"),
        )?;
        run_cache_git(
            path,
            ["symbolic-ref", "HEAD", &local_ref],
            &format!("failed to set HEAD to `{git_branch}`"),
        )?;
        reset_cached_checkout(path)?;
    } else if git_ref_exists(path, &local_ref)? {
        run_cache_git(
            path,
            ["symbolic-ref", "HEAD", &local_ref],
            &format!("failed to check out share branch `{git_branch}`"),
        )?;
        reset_cached_checkout(path)?;
    } else {
        run_cache_git(
            path,
            ["symbolic-ref", "HEAD", &local_ref],
            &format!("failed to set unborn HEAD to `{git_branch}`"),
        )?;
    }
    Ok(())
}

/// Commits the current cache repository workdir.
fn commit_cache_repository(path: &Path, git_branch: &str) -> Result<String> {
    run_cache_git(
        path,
        ["rm", "-r", "-f", "--cached", "--ignore-unmatch", "."],
        "failed to stage removed share artifacts",
    )?;
    let use_lfs_filters = git_lfs_publish_enabled(path)?;
    run_cache_git_with_lfs_filter_override(
        path,
        ["add", "-f", "--", GIT_ATTRIBUTES_FILE, ARTIFACT_ROOT],
        "failed to add share artifacts to index",
        !use_lfs_filters,
    )?;
    let diff = run_cache_git_raw(path, ["diff", "--cached", "--quiet"])
        .context("failed to inspect staged share artifacts")?;
    if diff.status.success() {
        return rev_parse_head(path);
    }
    if diff.status.code() != Some(1) {
        bail!(
            "{}",
            git_failure_message("failed to inspect staged share artifacts", &diff)
        );
    }
    let message = format!("chore(share): update {git_branch}");
    run_cache_git(
        path,
        ["commit", "--no-gpg-sign", "-m", &message],
        "failed to commit share artifacts",
    )?;
    rev_parse_head(path)
}

/// Pushes one local share branch without streaming Git progress.
fn push_branch(path: &Path, git_branch: &str) -> Result<()> {
    push_branch_impl::<fn(SharePushProgress)>(path, git_branch, None)
}

/// Pushes one local share branch while streaming Git upload progress.
fn push_branch_with_progress<F>(path: &Path, git_branch: &str, progress: &mut F) -> Result<()>
where
    F: FnMut(SharePushProgress),
{
    push_branch_impl(path, git_branch, Some(progress))
}

/// Pushes one local share branch, optionally streaming Git upload progress.
fn push_branch_impl<F>(path: &Path, git_branch: &str, mut progress: Option<&mut F>) -> Result<()>
where
    F: FnMut(SharePushProgress),
{
    for command in push_branch_commands(git_branch, git_lfs_publish_enabled(path)?) {
        match progress.as_mut() {
            Some(progress) => {
                progress(SharePushProgress::Uploading { kind: command.kind });
                run_cache_git_streaming_progress(
                    path,
                    command.progress_args.iter(),
                    &command.context,
                    command.kind,
                    &mut **progress,
                )?;
            }
            None => {
                run_cache_git(path, command.quiet_args.iter(), &command.context)?;
            }
        }
    }
    Ok(())
}

/// Builds the ordered Git commands needed to upload one share branch.
fn push_branch_commands(git_branch: &str, lfs_available: bool) -> Vec<PushBranchCommand> {
    let mut commands = Vec::new();
    if lfs_available {
        let local_ref = format!("refs/heads/{git_branch}");
        let args = vec![
            OsString::from("lfs"),
            OsString::from("push"),
            OsString::from(DEFAULT_REMOTE_NAME),
            OsString::from(local_ref),
        ];
        commands.push(PushBranchCommand {
            kind: ShareUploadKind::Lfs,
            quiet_args: args.clone(),
            progress_args: args,
            context: format!("failed to push Git LFS objects for share branch `{git_branch}`"),
        });
    }
    let refspec = format!("refs/heads/{git_branch}:refs/heads/{git_branch}");
    commands.push(PushBranchCommand {
        kind: ShareUploadKind::Git,
        quiet_args: vec![
            OsString::from("push"),
            OsString::from(DEFAULT_REMOTE_NAME),
            OsString::from(&refspec),
        ],
        progress_args: vec![
            OsString::from("push"),
            OsString::from("--progress"),
            OsString::from(DEFAULT_REMOTE_NAME),
            OsString::from(refspec),
        ],
        context: format!("failed to push share branch `{git_branch}` with system git"),
    });
    commands
}

/// Returns whether one Git ref exists in the cache repository.
fn git_ref_exists(path: &Path, reference: &str) -> Result<bool> {
    let output = run_cache_git_raw(path, ["show-ref", "--verify", "--quiet", reference])
        .with_context(|| format!("failed to inspect Git ref `{reference}`"))?;
    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    bail!(
        "{}",
        git_failure_message(&format!("failed to inspect Git ref `{reference}`"), &output)
    )
}

/// Returns the current HEAD commit id from a cache repository.
fn rev_parse_head(path: &Path) -> Result<String> {
    let output = run_cache_git(
        path,
        ["rev-parse", "HEAD"],
        "failed to read share commit id",
    )?;
    Ok(output.stdout.trim().to_owned())
}

/// Returns whether one Git failure means the remote share branch is absent.
fn is_missing_remote_ref_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("couldn't find remote ref")
        || message.contains("could not find remote ref")
        || message.contains("couldn't find remote branch")
}

/// Runs one system Git command and requires a successful exit status.
fn run_git<I, S>(path: &Path, args: I, context: &str) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = run_git_raw(path, args).with_context(|| context.to_owned())?;
    if output.status.success() {
        Ok(output)
    } else {
        bail!("{}", git_failure_message(context, &output))
    }
}

/// Runs one system Git command pinned to a Darc share cache worktree.
fn run_cache_git<I, S>(cache_path: &Path, args: I, context: &str) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = run_cache_git_raw(cache_path, args).with_context(|| context.to_owned())?;
    if output.status.success() {
        Ok(output)
    } else {
        bail!("{}", git_failure_message(context, &output))
    }
}

/// Runs one cache Git command with explicit git-dir and work-tree scope.
fn run_cache_git_raw<I, S>(cache_path: &Path, args: I) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_cache_git_raw_with_hook_override(cache_path, args, true)
}

/// Runs one cache Git command while controlling Git hook override behavior.
fn run_cache_git_with_hook_override<I, S>(
    cache_path: &Path,
    args: I,
    context: &str,
    disable_hooks: bool,
) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = run_cache_git_raw_with_hook_override(cache_path, args, disable_hooks)
        .with_context(|| context.to_owned())?;
    if output.status.success() {
        Ok(output)
    } else {
        bail!("{}", git_failure_message(context, &output))
    }
}

/// Runs one cache Git command while optionally disabling LFS filters.
fn run_cache_git_with_lfs_filter_override<I, S>(
    cache_path: &Path,
    args: I,
    context: &str,
    disable_lfs_filters: bool,
) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = run_cache_git_raw_with_options(cache_path, args, true, disable_lfs_filters)
        .with_context(|| context.to_owned())?;
    if output.status.success() {
        Ok(output)
    } else {
        bail!("{}", git_failure_message(context, &output))
    }
}

/// Runs one cache Git command with optional hook override.
fn run_cache_git_raw_with_hook_override<I, S>(
    cache_path: &Path,
    args: I,
    disable_hooks: bool,
) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_cache_git_raw_with_options(cache_path, args, disable_hooks, false)
}

/// Runs one cache Git command with optional hook and LFS filter overrides.
fn run_cache_git_raw_with_options<I, S>(
    cache_path: &Path,
    args: I,
    disable_hooks: bool,
    disable_lfs_filters: bool,
) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    ensure_safe_cache_git_dir(cache_path)?;
    let scoped_args = scoped_cache_git_args(cache_path, args);
    run_git_raw_with_options(cache_path, scoped_args, disable_hooks, disable_lfs_filters)
}

/// Runs one cache Git command while streaming upload progress.
fn run_cache_git_streaming_progress<I, S>(
    cache_path: &Path,
    args: I,
    context: &str,
    kind: ShareUploadKind,
    progress: &mut impl FnMut(SharePushProgress),
) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = run_cache_git_raw_streaming_progress(cache_path, args, kind, progress)
        .with_context(|| context.to_owned())?;
    if output.status.success() {
        Ok(output)
    } else {
        bail!("{}", git_failure_message(context, &output))
    }
}

/// Runs one cache Git command with streamed stderr and no exit-status interpretation.
fn run_cache_git_raw_streaming_progress<I, S>(
    cache_path: &Path,
    args: I,
    kind: ShareUploadKind,
    progress: &mut impl FnMut(SharePushProgress),
) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    ensure_safe_cache_git_dir(cache_path)?;
    let scoped_args = scoped_cache_git_args(cache_path, args);
    run_git_raw_streaming_progress_with_options(
        cache_path,
        scoped_args,
        true,
        false,
        kind,
        progress,
    )
}

/// Builds scoped Git arguments for one share cache worktree.
fn scoped_cache_git_args<I, S>(cache_path: &Path, args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let git_dir = cache_path.join(".git");
    let mut scoped_args = vec![
        OsString::from("--git-dir"),
        git_dir.into_os_string(),
        OsString::from("--work-tree"),
        cache_path.as_os_str().to_owned(),
    ];
    scoped_args.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
    scoped_args
}

/// Runs one system Git command without interpreting its exit status.
fn run_git_raw<I, S>(path: &Path, args: I) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_git_raw_with_hook_override(path, args, true)
}

/// Runs one system Git command with optional hook override.
fn run_git_raw_with_hook_override<I, S>(
    path: &Path,
    args: I,
    disable_hooks: bool,
) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_git_raw_with_options(path, args, disable_hooks, false)
}

/// Runs one system Git command with optional hook and LFS filter overrides.
fn run_git_raw_with_options<I, S>(
    path: &Path,
    args: I,
    disable_hooks: bool,
    disable_lfs_filters: bool,
) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect::<Vec<OsString>>();
    let output = configured_git_command(path, &args, disable_hooks, disable_lfs_filters)
        .stdin(Stdio::null())
        .output()
        .with_context(|| {
            format!(
                "failed to run system git in {}: git {}",
                path.display(),
                git_args_display(&args)
            )
        })?;
    Ok(GitCommandOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Runs one system Git command and streams sanitized stderr progress.
fn run_git_raw_streaming_progress_with_options<I, S>(
    path: &Path,
    args: I,
    disable_hooks: bool,
    disable_lfs_filters: bool,
    kind: ShareUploadKind,
    progress: &mut impl FnMut(SharePushProgress),
) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect::<Vec<OsString>>();
    let child = configured_git_command(path, &args, disable_hooks, disable_lfs_filters)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "failed to run system git in {}: git {}",
                path.display(),
                git_args_display(&args)
            )
        })?;
    collect_streaming_command_output(child, kind, progress)
}

/// Collects one spawned command while streaming sanitized stderr progress.
fn collect_streaming_command_output(
    mut child: std::process::Child,
    kind: ShareUploadKind,
    progress: &mut impl FnMut(SharePushProgress),
) -> Result<GitCommandOutput> {
    let mut stdout = child
        .stdout
        .take()
        .context("failed to capture Git stdout")?;
    let stdout_reader = thread::spawn(move || {
        let mut data = Vec::new();
        stdout.read_to_end(&mut data).map(|_| data)
    });
    let mut stderr = child
        .stderr
        .take()
        .context("failed to capture Git stderr")?;
    let stderr = read_git_progress_stderr(&mut stderr, kind, progress)
        .context("failed to read Git stderr")?;
    let status = child.wait().context("failed to wait for system git")?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("failed to join Git stdout reader"))?
        .context("failed to read Git stdout")?;
    Ok(GitCommandOutput {
        status,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Builds one sanitized Git command with Darc's non-interactive environment.
fn configured_git_command(
    path: &Path,
    args: &[OsString],
    disable_hooks: bool,
    disable_lfs_filters: bool,
) -> Command {
    let mut command = Command::new("git");
    if disable_hooks {
        command.args(["-c", "core.hooksPath=/dev/null"]);
    }
    if disable_lfs_filters {
        command.args([
            "-c",
            "filter.lfs.clean=",
            "-c",
            "filter.lfs.smudge=cat",
            "-c",
            "filter.lfs.process=",
            "-c",
            "filter.lfs.required=false",
        ]);
    }
    command
        .args(["-c", "core.askPass=false"])
        .args(args)
        .current_dir(path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "false")
        .env("SSH_ASKPASS", "false")
        .env("GIT_SSH_COMMAND", git_ssh_command(path))
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_TEMPLATE_DIR")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_AUTHOR_DATE")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .env_remove("GIT_COMMITTER_DATE");
    command
}

/// Reads Git stderr, returning full text while emitting progress fragments.
fn read_git_progress_stderr<R: Read>(
    reader: &mut R,
    kind: ShareUploadKind,
    progress: &mut impl FnMut(SharePushProgress),
) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut pending = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..read]);
        for byte in &buffer[..read] {
            if *byte == b'\n' || *byte == b'\r' {
                emit_git_progress_fragment(kind, &pending, progress);
                pending.clear();
            } else {
                pending.push(*byte);
            }
        }
    }
    emit_git_progress_fragment(kind, &pending, progress);
    Ok(data)
}

/// Emits one sanitized Git progress fragment when it contains text.
fn emit_git_progress_fragment(
    kind: ShareUploadKind,
    fragment: &[u8],
    progress: &mut impl FnMut(SharePushProgress),
) {
    let message = String::from_utf8_lossy(fragment);
    let message = sanitize_git_diagnostic(message.trim());
    if !message.is_empty() {
        progress(SharePushProgress::GitProgress { kind, message });
    }
}

/// Returns an SSH command that preserves user config while disabling password prompts.
fn git_ssh_command(path: &Path) -> OsString {
    git_ssh_command_with_env(path, std::env::var_os("GIT_SSH_COMMAND"))
}

/// Returns an SSH command using environment first, then Git config.
fn git_ssh_command_with_env(path: &Path, command: Option<OsString>) -> OsString {
    if command.is_some() {
        return noninteractive_ssh_command(command);
    }
    noninteractive_ssh_command(git_core_ssh_command(path))
}

/// Reads the effective Git core.sshCommand for one repository path.
fn git_core_ssh_command(path: &Path) -> Option<OsString> {
    let output = Command::new("git")
        .args(["config", "--get", "core.sshCommand"])
        .current_dir(path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!command.is_empty()).then(|| OsString::from(command))
}

/// Adds SSH batch mode to an optional user-provided SSH command.
fn noninteractive_ssh_command(command: Option<OsString>) -> OsString {
    let command = command.unwrap_or_else(|| OsString::from("ssh"));
    let command_string = command.to_string_lossy();
    if command_string.contains("BatchMode=yes") {
        command
    } else if command_string.contains("BatchMode=no") {
        OsString::from(command_string.replace("BatchMode=no", "BatchMode=yes"))
    } else if let Some(args) = command_string.strip_prefix("ssh ") {
        OsString::from(format!("ssh -o BatchMode=yes {args}"))
    } else if command_string == "ssh" {
        OsString::from("ssh -o BatchMode=yes")
    } else {
        OsString::from(format!("{command_string} -o BatchMode=yes"))
    }
}

/// Formats one failed Git command for user-facing errors.
fn git_failure_message(context: &str, output: &GitCommandOutput) -> String {
    let mut message = format!("{context}: git exited with {}", output.status);
    let stdout_redacted = sanitize_git_diagnostic(&output.stdout);
    let stdout = stdout_redacted.trim();
    if !stdout.is_empty() {
        message.push_str("\nstdout:\n");
        message.push_str(stdout);
    }
    let stderr_redacted = sanitize_git_diagnostic(&output.stderr);
    let stderr = stderr_redacted.trim();
    if !stderr.is_empty() {
        message.push_str("\nstderr:\n");
        message.push_str(stderr);
    }
    message
}

/// Sanitizes Git diagnostic text before it reaches CLI errors or logs.
fn sanitize_git_diagnostic(text: &str) -> String {
    text.split_whitespace()
        .map(sanitize_git_diagnostic_token)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sanitizes one possible URL-bearing diagnostic token.
fn sanitize_git_diagnostic_token(token: &str) -> String {
    let trimmed = token.trim_matches(|character: char| {
        matches!(
            character,
            '\'' | '"' | '`' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
        )
    });
    if trimmed.contains("://") || trimmed.contains('@') || trimmed.contains('?') {
        let sanitized = sanitize_git_url_for_display(trimmed);
        return token.replacen(trimmed, &sanitized, 1);
    }
    token.to_owned()
}

/// Formats Git arguments for diagnostics without invoking a shell.
fn git_args_display(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| sanitize_git_diagnostic_token(arg.to_string_lossy().as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns whether a path is an existing non-symlink directory.
fn is_regular_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!("share artifact directory is a symlink: {}", path.display());
            }
            if !file_type.is_dir() {
                bail!(
                    "share artifact directory path is not a directory: {}",
                    path.display()
                );
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

/// Returns one cache repository path for a remote URL and branch.
fn cache_repo_path(root: &Path, remote_url: &str, git_branch: &str) -> PathBuf {
    root.join(SHARE_CACHE_DIR).join(sha256_hex(
        format!("{}\n{git_branch}", canonical_share_remote_url(remote_url)).as_bytes(),
    ))
}

/// Returns the trusted local object cache path for one cache repository.
fn trusted_object_cache_path(cache_path: &Path) -> PathBuf {
    cache_path.join(".git").join(TRUSTED_OBJECT_CACHE_DIR)
}

/// Builds the stored provenance key for one imported remote branch.
fn share_origin_remote(remote_url: &str, git_branch: &str) -> String {
    let canonical_url = canonical_share_remote_url(remote_url);
    let identity = sha256_hex(format!("{canonical_url}\n{git_branch}").as_bytes());
    format!("remote:{}:{git_branch}", &identity[..16])
}

/// Returns a non-secret canonical URL for share cache and provenance keys.
fn canonical_share_remote_url(remote_url: &str) -> String {
    normalize_git_url(remote_url).unwrap_or_else(|_| sanitize_git_url_for_display(remote_url))
}

/// Returns the per-exporter visible manifest path.
fn exporter_manifest_relative_path(identity: &ShareIdentity) -> String {
    format!(
        "{ARTIFACT_ROOT}/{EXPORTERS_DIR}/{}/{}",
        exporter_manifest_id(identity),
        LEGACY_MANIFEST_FILE
    )
}

/// Returns one stable non-secret exporter path component.
fn exporter_manifest_id(identity: &ShareIdentity) -> String {
    sha256_hex(identity.user_id.as_bytes())[..16].to_owned()
}

/// Returns whether one manifest path is canonical for its authenticated exporter.
fn manifest_path_matches_exporter(relative_path: &str, exporter_id: &str) -> bool {
    relative_path == format!("{ARTIFACT_ROOT}/{LEGACY_MANIFEST_FILE}")
        || relative_path
            == format!("{ARTIFACT_ROOT}/{EXPORTERS_DIR}/{exporter_id}/{LEGACY_MANIFEST_FILE}")
}

/// Resolves and validates one manifest object path below the cache workdir.
fn manifest_object_path(cache_path: &Path, entry: &TurnManifestEntry) -> Result<PathBuf> {
    manifest_artifact_path(cache_path, &entry.object_path)
}

/// Removes one encrypted object path from the cache workdir if it exists.
fn remove_artifact_object(cache_path: &Path, object_path: &str) -> Result<()> {
    let path = manifest_artifact_path(cache_path, object_path)?;
    remove_file_if_exists(&path)
}

/// Removes one relative manifest file from the cache workdir if it exists.
fn remove_relative_file(cache_path: &Path, relative_path: &str) -> Result<()> {
    let relative = validate_relative_artifact_path(relative_path)?;
    ensure_safe_artifact_ancestors(cache_path, relative)?;
    remove_file_if_exists(&cache_path.join(relative))
}

/// Removes one file, ignoring already-missing paths.
fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

/// Resolves and validates one encrypted object path below the cache workdir.
fn manifest_artifact_path(cache_path: &Path, object_path: &str) -> Result<PathBuf> {
    let relative = validate_manifest_object_relative_path(object_path)?;
    ensure_safe_artifact_ancestors(cache_path, relative)?;
    Ok(cache_path.join(relative))
}

/// Validates one visible encrypted object path without touching the worktree.
fn validate_manifest_object_relative_path(object_path: &str) -> Result<&Path> {
    let expected_prefix = format!("{ARTIFACT_ROOT}/objects/");
    if !object_path.starts_with(&expected_prefix) || !object_path.ends_with(".age") {
        bail!("share object path is outside the supported object namespace");
    }
    let object_file = object_path
        .strip_prefix(&expected_prefix)
        .context("share object path is outside the supported object namespace")?;
    if object_file.is_empty() || object_file.contains('/') {
        bail!("share object path must be a direct object file");
    }
    let relative = Path::new(object_path);
    if relative.is_absolute() {
        bail!("share object path must be relative");
    }
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            bail!("share object path contains unsafe path components");
        }
    }
    Ok(relative)
}

/// Writes one JSON artifact below the cache workdir without following symlinks.
fn write_json_artifact_file<T: Serialize>(
    cache_path: &Path,
    relative_path: &str,
    value: &T,
) -> Result<()> {
    let content = serde_json::to_vec_pretty(value).context("failed to serialize JSON")?;
    write_artifact_file(cache_path, relative_path, &content)
}

/// Writes one artifact file below the cache workdir without following symlinks.
fn write_artifact_file(cache_path: &Path, relative_path: &str, content: &[u8]) -> Result<()> {
    let relative = validate_relative_artifact_path(relative_path)?;
    let target = cache_path.join(relative);
    let parent = target
        .parent()
        .context("share artifact path is missing a parent")?;
    create_safe_dir_all(cache_path, parent)?;
    if let Ok(metadata) = fs::symlink_metadata(&target)
        && metadata.file_type().is_symlink()
    {
        bail!("share artifact path is a symlink: {}", target.display());
    }
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("share artifact path is missing a file name")?;
    let temporary = parent.join(format!(
        ".{file_name}.darc-tmp-{}",
        &sha256_hex(content)[..16]
    ));
    remove_file_if_exists(&temporary)?;
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        file.write_all(content)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
    }
    fs::rename(&temporary, &target)
        .with_context(|| format!("failed to replace {}", target.display()))?;
    Ok(())
}

/// Creates one cache subdirectory after rejecting symlinks in existing ancestors.
fn create_safe_dir_all(cache_path: &Path, directory: &Path) -> Result<()> {
    match fs::symlink_metadata(cache_path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!("share cache path is a symlink: {}", cache_path.display());
            }
            if !file_type.is_dir() {
                bail!(
                    "share cache path is not a directory: {}",
                    cache_path.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(cache_path)
                .with_context(|| format!("failed to create {}", cache_path.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", cache_path.display()));
        }
    }
    let relative = directory.strip_prefix(cache_path).with_context(|| {
        format!(
            "share artifact directory {} is outside cache {}",
            directory.display(),
            cache_path.display()
        )
    })?;
    let mut current = cache_path.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            bail!("share artifact directory contains unsafe path components");
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_symlink() {
                    bail!(
                        "share artifact directory is a symlink: {}",
                        current.display()
                    );
                }
                if !file_type.is_dir() {
                    bail!(
                        "share artifact directory path is not a directory: {}",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)
                    .with_context(|| format!("failed to create {}", current.display()))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", current.display()));
            }
        }
    }
    Ok(())
}

/// Validates one cache-root-relative artifact path.
fn validate_relative_artifact_path(relative_path: &str) -> Result<&Path> {
    let relative = Path::new(relative_path);
    if relative.is_absolute() {
        bail!("share artifact path must be relative");
    }
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            bail!("share artifact path contains unsafe path components");
        }
    }
    Ok(relative)
}

/// Rejects symlinked existing parent directories for one cache artifact path.
fn ensure_safe_artifact_ancestors(cache_path: &Path, relative_path: &Path) -> Result<()> {
    if !ensure_safe_existing_cache_dir(cache_path)? {
        return Ok(());
    }
    let mut current = cache_path.to_path_buf();
    if let Some(parent) = relative_path.parent() {
        for component in parent.components() {
            let Component::Normal(name) = component else {
                bail!("share artifact path contains unsafe path components");
            };
            current.push(name);
            match fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    let file_type = metadata.file_type();
                    if file_type.is_symlink() {
                        bail!(
                            "share artifact ancestor is a symlink: {}",
                            current.display()
                        );
                    }
                    if !file_type.is_dir() {
                        bail!(
                            "share artifact ancestor is not a directory: {}",
                            current.display()
                        );
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect {}", current.display()));
                }
            }
        }
    }
    Ok(())
}

/// Writes one pretty JSON file.
#[cfg(test)]
fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("JSON path is missing a parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let content = serde_json::to_vec_pretty(value).context("failed to serialize JSON")?;
    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))
}

/// Reads one JSON file.
fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let content = read_regular_file(path, MAX_SHARE_MANIFEST_BYTES)?;
    serde_json::from_slice(&content).with_context(|| format!("failed to parse {}", path.display()))
}

/// Reads one regular artifact file after rejecting symlinks and oversized content.
fn read_regular_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!("share artifact path is a symlink: {}", path.display());
    }
    if !file_type.is_file() {
        bail!(
            "share artifact path is not a regular file: {}",
            path.display()
        );
    }
    if metadata.len() > max_bytes {
        bail!(
            "share artifact {} exceeds maximum supported size of {} bytes",
            path.display(),
            max_bytes
        );
    }
    fs::read(path).with_context(|| format!("failed to read {}", path.display()))
}

/// Reads a bounded prefix from a regular file after rejecting symlinks.
fn read_file_prefix(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!("share artifact path is a symlink: {}", path.display());
    }
    if !file_type.is_file() {
        bail!(
            "share artifact path is not a regular file: {}",
            path.display()
        );
    }
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut prefix = Vec::new();
    file.take(max_bytes)
        .read_to_end(&mut prefix)
        .with_context(|| format!("failed to read {}", path.display()))?;
    Ok(prefix)
}

/// Rejects a Git LFS pointer when an encrypted object should be hydrated.
fn ensure_not_lfs_pointer(content: &[u8], path: &Path) -> Result<()> {
    if content.starts_with(GIT_LFS_POINTER_PREFIX) {
        bail!(
            "share object {} is a Git LFS pointer; run `git lfs pull` for the share cache",
            path.display()
        );
    }
    Ok(())
}

/// Validates one user-facing share branch shorthand.
fn validate_share_branch_name(branch: &str) -> Result<()> {
    if branch.is_empty() {
        bail!("share branch name cannot be empty");
    }
    if branch.starts_with('/') || branch.ends_with('/') || branch.contains("//") {
        bail!("share branch name must not start, end, or repeat `/`");
    }
    if branch.contains("..") || branch.contains("@{") {
        bail!("share branch name is not a safe Git branch component");
    }
    for component in branch.split('/') {
        if component.is_empty()
            || component.starts_with('.')
            || component.ends_with('.')
            || component.ends_with(".lock")
        {
            bail!("share branch name is not a safe Git branch component");
        }
    }
    if !branch
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
    {
        bail!("share branch name may only contain ASCII letters, digits, `/`, `-`, `_`, or `.`");
    }
    Ok(())
}

/// Derives one stable authenticated share user id.
fn derive_user_id(signing_public_key: &str) -> String {
    format!(
        "usr-{}",
        &sha256_hex(format!("signing-key:{}", signing_public_key.trim()).as_bytes())[..16]
    )
}

/// Returns one lowercase hex SHA-256 digest.
fn sha256_hex(input: &[u8]) -> String {
    hex_encode(&Sha256::digest(input))
}

/// Returns one lowercase hex string.
fn hex_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for byte in input {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

/// Decodes one fixed-size lowercase or uppercase hex string.
fn hex_decode_fixed<const N: usize>(input: &str) -> Result<[u8; N]> {
    let trimmed = input.trim();
    if trimmed.len() != N * 2 {
        bail!("expected {} hex characters", N * 2);
    }
    let mut out = [0_u8; N];
    for (index, chunk) in trimmed.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_value(chunk[0]).context("invalid hex digit")?;
        let low = hex_value(chunk[1]).context("invalid hex digit")?;
        out[index] = (high << 4) | low;
    }
    Ok(out)
}

/// Returns one nibble value for a hex byte.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use darc_paths::SourceKind;
    use darc_test_utils::{
        IndexedSessionFixture, IndexedTurnFixture, insert_indexed_session, insert_indexed_turn,
        unique_test_dir,
    };

    use super::*;

    #[test]
    fn share_branch_maps_to_darc_prefix() {
        assert_eq!(share_git_branch("team").unwrap(), "darc/team");
        assert_eq!(share_git_branch("team/a").unwrap(), "darc/team/a");
        assert!(share_git_branch("bad:name").is_err());
        assert!(share_git_branch("../bad").is_err());
        assert!(share_git_branch("team.lock/foo").is_err());
        assert!(share_git_branch("team/leaf.lock").is_err());
        assert!(share_git_branch(".team").is_err());
        assert!(share_git_branch("team.").is_err());
        assert!(share_git_branch("team/.leaf").is_err());
        assert!(share_git_branch("team/leaf.").is_err());
    }

    #[test]
    fn normalizes_common_git_urls() {
        assert_eq!(
            normalize_git_url("git@github.com:Example/Darc.git").unwrap(),
            "https://github.com/Example/Darc"
        );
        assert_eq!(
            normalize_git_url("github.com:Example/Darc.git").unwrap(),
            "https://github.com/Example/Darc"
        );
        assert_eq!(
            normalize_git_url("https://github.com/Example/Darc.git/").unwrap(),
            "https://github.com/Example/Darc"
        );
        assert_eq!(
            normalize_git_url("HTTPS://github.com/Example/Darc.git").unwrap(),
            "https://github.com/Example/Darc"
        );
        assert_eq!(
            normalize_git_url("https://user:token@github.com/Example/Darc.git/").unwrap(),
            "https://github.com/Example/Darc"
        );
        assert_eq!(
            normalize_git_url(
                "https://user:token@github.com/Example/Darc.git?access_token=secret#frag"
            )
            .unwrap(),
            "https://github.com/Example/Darc"
        );
        assert_eq!(
            normalize_git_url("ssh://deploy@github.com/Team/App.git").unwrap(),
            "https://github.com/Team/App"
        );
        assert!(normalize_git_url("file:///Users/alice/repo.git").is_err());
        assert!(normalize_git_url("/Users/alice/repo.git").is_err());
        assert_eq!(
            sanitize_git_url_for_display(
                "https://user:token@github.com/Example/Darc.git?access_token=secret#frag"
            ),
            "https://github.com/Example/Darc.git"
        );
        assert_eq!(
            sanitize_git_url_for_display("https://user:token@example.invalid?access_token=secret"),
            "https://example.invalid"
        );
        assert_eq!(
            cache_repo_path(
                Path::new("/tmp/darc-root"),
                "git@github.com:Example/Darc.git",
                "darc/team"
            ),
            cache_repo_path(
                Path::new("/tmp/darc-root"),
                "https://github.com/Example/Darc.git?access_token=secret",
                "darc/team"
            )
        );
    }

    #[test]
    fn share_remote_urls_reject_persisted_credentials() {
        validate_share_remote_url("git@github.com:Example/Darc.git").unwrap();
        validate_share_remote_url("github.com:Example/Darc.git").unwrap();
        validate_share_remote_url("ssh://git@github.com/Example/Darc.git").unwrap();
        assert!(validate_share_remote_url("https://user@example.invalid/team/share.git").is_err());
        assert!(
            validate_share_remote_url("https://example.invalid/team/share.git?token=secret")
                .is_err()
        );
        assert!(
            validate_share_remote_url("git://user:token@example.invalid/team/share.git").is_err()
        );
        assert!(validate_share_remote_url("git://user@example.invalid/team/share.git").is_err());
        assert!(
            validate_share_remote_url("ssh://user:pass@example.invalid/team/share.git").is_err()
        );
    }

    #[test]
    fn derives_user_id_from_signing_key_not_email() {
        let left = derive_user_id("001122");
        let right = derive_user_id("aabbcc");
        assert_ne!(left, right);
    }

    #[test]
    fn encrypts_and_decrypts_payload() {
        let identity = Identity::generate();
        let recipient = identity.to_public();
        let encrypted = encrypt_payload(b"payload", &[recipient]).unwrap();
        let decrypted = decrypt_payload(&encrypted, &identity).unwrap();
        assert_eq!(decrypted, b"payload");
    }

    #[test]
    fn cached_manifest_reads_stop_at_count_cap() {
        let root = unique_test_dir("share-manifest-count-cap");
        let exporter_root = root.join(ARTIFACT_ROOT).join(EXPORTERS_DIR);
        for index in 0..(MAX_CACHED_SHARE_MANIFESTS + 2) {
            let manifest = exporter_root
                .join(format!("exporter-{index:02}"))
                .join(LEGACY_MANIFEST_FILE);
            fs::create_dir_all(manifest.parent().unwrap()).unwrap();
            fs::write(manifest, b"{}").unwrap();
        }

        let read = read_cached_manifests(&root).unwrap();

        assert!(read.manifests.is_empty());
        assert!(
            read.warnings
                .iter()
                .any(|warning| warning.contains("cached manifest count exceeds")),
            "warnings should mention count cap: {:?}",
            read.warnings
        );
        assert!(read.warnings.len() <= MAX_CACHED_SHARE_MANIFESTS + 1);
    }

    #[test]
    fn cached_manifest_reads_stop_at_exporter_directory_cap() {
        let root = unique_test_dir("share-exporter-dir-count-cap");
        let exporter_root = root.join(ARTIFACT_ROOT).join(EXPORTERS_DIR);
        for index in 0..(MAX_CACHED_SHARE_EXPORTER_DIRS + 2) {
            fs::create_dir_all(exporter_root.join(format!("exporter-{index:02}"))).unwrap();
        }

        let read = read_cached_manifests(&root).unwrap();

        assert!(read.manifests.is_empty());
        assert!(
            read.warnings
                .iter()
                .any(|warning| warning.contains("cached exporter directory count exceeds")),
            "warnings should mention exporter directory cap: {:?}",
            read.warnings
        );
        assert_eq!(read.warnings.len(), 1);
    }

    #[test]
    fn cached_manifest_reads_stop_at_aggregate_byte_cap() {
        let root = unique_test_dir("share-manifest-byte-cap");
        let exporter_root = root.join(ARTIFACT_ROOT).join(EXPORTERS_DIR);
        for index in 0..3 {
            let manifest = exporter_root
                .join(format!("exporter-{index:02}"))
                .join(LEGACY_MANIFEST_FILE);
            fs::create_dir_all(manifest.parent().unwrap()).unwrap();
            fs::write(manifest, " ".repeat(10_000)).unwrap();
        }

        let read = read_cached_manifests(&root).unwrap();

        assert!(read.manifests.is_empty());
        assert!(
            read.warnings
                .iter()
                .any(|warning| warning.contains("cached manifest bytes exceed")),
            "warnings should mention aggregate byte cap: {:?}",
            read.warnings
        );
    }

    #[cfg(unix)]
    #[test]
    fn share_key_file_uses_private_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = unique_test_dir("share-key-permissions");
        let key = ensure_share_key(&root).unwrap();
        let mode = fs::metadata(&key.key_path).unwrap().permissions().mode() & 0o777;

        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn share_key_read_rejects_symlinked_age_key() {
        use std::os::unix::fs::symlink;

        let root = unique_test_dir("share-key-symlink-age");
        let outside = root.join("outside.agekey");
        let key_path = root.join("keys").join(KEY_FILE_NAME);
        fs::create_dir_all(key_path.parent().unwrap()).unwrap();
        fs::write(&outside, Identity::generate().to_string().expose_secret()).unwrap();
        symlink(&outside, &key_path).unwrap();

        let error = ensure_share_key(&root).unwrap_err();

        assert!(
            error.to_string().contains("symlink"),
            "error should reject symlinked age key: {error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn share_key_read_rejects_symlinked_signing_key() {
        use std::os::unix::fs::symlink;

        let root = unique_test_dir("share-key-symlink-signing");
        let outside = root.join("outside.signingkey");
        let key_path = root.join("keys").join(SIGNING_KEY_FILE_NAME);
        fs::create_dir_all(key_path.parent().unwrap()).unwrap();
        fs::write(&outside, hex_encode(&Sha256::digest(b"synthetic-seed"))).unwrap();
        symlink(&outside, &key_path).unwrap();

        let error = ensure_share_signing_key(&root).unwrap_err();

        assert!(
            error.to_string().contains("symlink"),
            "error should reject symlinked signing key: {error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn share_key_creation_rejects_symlinked_key_directory_for_age_key() {
        use std::os::unix::fs::symlink;

        let root = unique_test_dir("share-key-dir-symlink-age");
        let outside = root.join("outside-keys");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("keys")).unwrap();

        let error = ensure_share_key(&root).unwrap_err();

        assert!(
            error.to_string().contains("symlink"),
            "error should reject symlinked key directory: {error:#}"
        );
        assert!(!outside.join(KEY_FILE_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn share_key_creation_rejects_symlinked_key_directory_for_signing_key() {
        use std::os::unix::fs::symlink;

        let root = unique_test_dir("share-key-dir-symlink-signing");
        let outside = root.join("outside-keys");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("keys")).unwrap();

        let error = ensure_share_signing_key(&root).unwrap_err();

        assert!(
            error.to_string().contains("symlink"),
            "error should reject symlinked key directory: {error:#}"
        );
        assert!(!outside.join(SIGNING_KEY_FILE_NAME).exists());
    }

    #[test]
    fn recipient_set_changes_encrypt_to_a_new_object_path() {
        let workspace = unique_test_dir("share-recipient-fingerprint");
        let index_db_path = workspace.join("index.sqlite");
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: index_db_path.clone(),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: workspace.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &index_db_path,
            "repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&context, SharePolicy::All).unwrap();
        let connection = open_index_database_writer(&index_db_path).unwrap();
        let turns = query_share_export_turns(&connection, "repo").unwrap();
        let age_identity = Identity::generate();
        let signing_key = test_signing_key(&age_identity);
        let identity = test_share_identity(&age_identity);
        let first = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            signing_key: &signing_key,
            branch: "team",
            turns: turns.clone(),
        })
        .unwrap();
        let extra_identity = Identity::generate();
        let settings = ShareSettings {
            remotes: Vec::new(),
            recipients: vec![ShareRecipient {
                recipient: extra_identity.to_public().to_string(),
            }],
        };
        let second = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &settings,
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            signing_key: &signing_key,
            branch: "team",
            turns,
        })
        .unwrap();

        assert_ne!(
            first.manifest.chunks[0].object_path,
            second.manifest.chunks[0].object_path
        );
        assert!(
            !first
                .objects
                .contains_key(&second.manifest.chunks[0].object_path)
        );
        let plaintext = decrypt_payload(
            &second.objects[&second.manifest.chunks[0].object_path],
            &extra_identity,
        )
        .unwrap();
        assert_eq!(
            sha256_hex(&plaintext),
            second.manifest.chunks[0].plaintext_hash
        );
    }

    #[test]
    fn export_rejects_single_turn_chunk_over_pull_size_limit() {
        let workspace = unique_test_dir("share-oversize-single-turn");
        let age_identity = Identity::generate();
        let signing_key = test_signing_key(&age_identity);
        let identity = test_share_identity(&age_identity);
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: workspace.join("index.sqlite"),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: workspace.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let mut turn = synthetic_share_turn("repo", "00000000-0000-4000-8000-000000000303", 1);
        let oversized_message_len = usize::try_from(MAX_SHARE_CHUNK_DECOMPRESSED_BYTES).unwrap();
        turn.user_message = "x".repeat(oversized_message_len);

        let error = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            signing_key: &signing_key,
            branch: "team",
            turns: vec![turn],
        })
        .err()
        .expect("oversized single-turn chunk should fail before push");

        assert!(
            error
                .to_string()
                .contains("share chunk exceeds maximum supported decompressed size")
        );
    }

    #[test]
    fn trusted_object_cache_reuses_unchanged_export_bytes() {
        let workspace = unique_test_dir("share-object-cache-reuse");
        let trusted_cache = workspace.join("trusted-cache");
        let index_db_path = workspace.join("index.sqlite");
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: index_db_path.clone(),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: workspace.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &index_db_path,
            "repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&context, SharePolicy::All).unwrap();
        let connection = open_index_database_writer(&index_db_path).unwrap();
        let turns = query_share_export_turns(&connection, "repo").unwrap();
        let age_identity = Identity::generate();
        let signing_key = test_signing_key(&age_identity);
        let identity = test_share_identity(&age_identity);
        let first = build_export_artifact_with_reuse(
            ExportBuildRequest {
                context: &context,
                settings: &ShareSettings::default(),
                project_key: "git:https://example.invalid/team/repo",
                identity: &identity,
                signing_key: &signing_key,
                branch: "team",
                turns: turns.clone(),
            },
            ExportReuseContext {
                trusted_object_cache_path: Some(&trusted_cache),
                decryption_identity: Some(&age_identity),
                previous_project: None,
                previous_manifest: None,
            },
        )
        .unwrap();

        let second = build_export_artifact_with_reuse(
            ExportBuildRequest {
                context: &context,
                settings: &ShareSettings::default(),
                project_key: "git:https://example.invalid/team/repo",
                identity: &identity,
                signing_key: &signing_key,
                branch: "team",
                turns,
            },
            ExportReuseContext {
                trusted_object_cache_path: Some(&trusted_cache),
                decryption_identity: Some(&age_identity),
                previous_project: Some(&first.project),
                previous_manifest: Some(&first.manifest),
            },
        )
        .unwrap();

        assert_eq!(first.objects, second.objects);
        assert_eq!(first.project, second.project);
        assert_eq!(first.manifest, second.manifest);
    }

    #[test]
    fn unchanged_previous_export_reuses_manifest_without_turn_rebuild() {
        let workspace = unique_test_dir("share-unchanged-export-reuse");
        let cache = workspace.join("cache");
        let index_db_path = workspace.join("index.sqlite");
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: index_db_path.clone(),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: workspace.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &index_db_path,
            "repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&context, SharePolicy::All).unwrap();
        let connection = open_index_database_writer(&index_db_path).unwrap();
        let turns = query_share_export_turns(&connection, "repo").unwrap();
        let export_fingerprint = share_export_fingerprint(&turns).unwrap();
        let selected_sessions = query_share_export_session_states(&connection, "repo").unwrap();
        let age_identity = Identity::generate();
        let signing_key = test_signing_key(&age_identity);
        let identity = test_share_identity(&age_identity);
        let recipient_fingerprint = encryption_recipient_fingerprint(
            &encryption_recipient_strings(&identity, &ShareSettings::default()),
        );
        let first = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            signing_key: &signing_key,
            branch: "team",
            turns,
        })
        .unwrap();
        write_export_artifact(&cache, &first).unwrap();

        let reused = unchanged_previous_export_artifact(
            &cache,
            Some(&first.project),
            Some(&first.manifest),
            "git:https://example.invalid/team/repo",
            "repo",
            "team",
            &recipient_fingerprint,
            &export_fingerprint,
            &identity,
            &age_identity,
            &selected_sessions,
        )
        .unwrap()
        .expect("unchanged export should be reusable");

        assert_eq!(reused.project, first.project);
        assert_eq!(reused.manifest, first.manifest);
        assert_eq!(reused.object_paths, first.object_paths);
        assert_eq!(reused.exported_turn_count, first.exported_turn_count);
        assert_eq!(reused.exported_session_count, first.exported_session_count);
        assert!(reused.objects.is_empty());
    }

    #[test]
    fn unchanged_empty_export_reuses_manifest_without_turn_rebuild() {
        let workspace = unique_test_dir("share-empty-export-reuse");
        let cache = workspace.join("cache");
        let age_identity = Identity::generate();
        let signing_key = test_signing_key(&age_identity);
        let identity = test_share_identity(&age_identity);
        let recipient_fingerprint = encryption_recipient_fingerprint(
            &encryption_recipient_strings(&identity, &ShareSettings::default()),
        );
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: workspace.join("index.sqlite"),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: workspace.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let turns: Vec<ShareTurnExport> = Vec::new();
        let export_fingerprint = share_export_fingerprint(&turns).unwrap();
        let selected_sessions: Vec<ShareSessionExportState> = Vec::new();
        let first = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            signing_key: &signing_key,
            branch: "team",
            turns,
        })
        .unwrap();
        write_export_artifact(&cache, &first).unwrap();

        let reused = unchanged_previous_export_artifact(
            &cache,
            Some(&first.project),
            Some(&first.manifest),
            "git:https://example.invalid/team/repo",
            "repo",
            "team",
            &recipient_fingerprint,
            &export_fingerprint,
            &identity,
            &age_identity,
            &selected_sessions,
        )
        .unwrap()
        .expect("unchanged empty export should be reusable");

        assert!(reused.manifest.turns.is_empty());
        assert!(reused.manifest.chunks.is_empty());
        assert_eq!(reused.project, first.project);
        assert_eq!(reused.manifest, first.manifest);
        assert_eq!(reused.object_paths, first.object_paths);
        assert_eq!(reused.exported_turn_count, 0);
        assert_eq!(reused.exported_session_count, 0);
    }

    #[test]
    fn unchanged_previous_export_rebuilds_when_redacted_rows_change() {
        let workspace = unique_test_dir("share-unchanged-export-redacted-row-change");
        let cache = workspace.join("cache");
        let index_db_path = workspace.join("index.sqlite");
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: index_db_path.clone(),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: workspace.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &index_db_path,
            "repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&context, SharePolicy::All).unwrap();
        let connection = open_index_database_writer(&index_db_path).unwrap();
        let turns = query_share_export_turns(&connection, "repo").unwrap();
        let age_identity = Identity::generate();
        let signing_key = test_signing_key(&age_identity);
        let identity = test_share_identity(&age_identity);
        let recipient_fingerprint = encryption_recipient_fingerprint(
            &encryption_recipient_strings(&identity, &ShareSettings::default()),
        );
        let first = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            signing_key: &signing_key,
            branch: "team",
            turns,
        })
        .unwrap();
        write_export_artifact(&cache, &first).unwrap();
        connection
            .execute(
                "UPDATE turns SET user_message = 'synthetic changed prompt' WHERE project_id = 'repo'",
                [],
            )
            .unwrap();
        let selected_sessions = query_share_export_session_states(&connection, "repo").unwrap();
        let changed_turns = query_share_export_turns(&connection, "repo").unwrap();
        let changed_export_fingerprint = share_export_fingerprint(&changed_turns).unwrap();

        let reused = unchanged_previous_export_artifact(
            &cache,
            Some(&first.project),
            Some(&first.manifest),
            "git:https://example.invalid/team/repo",
            "repo",
            "team",
            &recipient_fingerprint,
            &changed_export_fingerprint,
            &identity,
            &age_identity,
            &selected_sessions,
        )
        .unwrap();

        assert!(reused.is_none());
    }

    #[test]
    fn unchanged_previous_export_rebuilds_when_source_metadata_changes() {
        let workspace = unique_test_dir("share-unchanged-export-source-change");
        let cache = workspace.join("cache");
        let index_db_path = workspace.join("index.sqlite");
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: index_db_path.clone(),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: workspace.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &index_db_path,
            "repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&context, SharePolicy::All).unwrap();
        let connection = open_index_database_writer(&index_db_path).unwrap();
        let turns = query_share_export_turns(&connection, "repo").unwrap();
        let age_identity = Identity::generate();
        let signing_key = test_signing_key(&age_identity);
        let identity = test_share_identity(&age_identity);
        let recipient_fingerprint = encryption_recipient_fingerprint(
            &encryption_recipient_strings(&identity, &ShareSettings::default()),
        );
        let first = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            signing_key: &signing_key,
            branch: "team",
            turns,
        })
        .unwrap();
        write_export_artifact(&cache, &first).unwrap();
        connection
            .execute(
                "UPDATE sessions SET source_mtime_ms = 2 WHERE project_id = 'repo'",
                [],
            )
            .unwrap();
        let selected_sessions = query_share_export_session_states(&connection, "repo").unwrap();
        let changed_turns = query_share_export_turns(&connection, "repo").unwrap();
        let changed_export_fingerprint = share_export_fingerprint(&changed_turns).unwrap();

        let reused = unchanged_previous_export_artifact(
            &cache,
            Some(&first.project),
            Some(&first.manifest),
            "git:https://example.invalid/team/repo",
            "repo",
            "team",
            &recipient_fingerprint,
            &changed_export_fingerprint,
            &identity,
            &age_identity,
            &selected_sessions,
        )
        .unwrap();

        assert!(reused.is_none());

        let mut forged_manifest = first.manifest.clone();
        forged_manifest.chunks[0].ciphertext_hash = sha256_hex(b"corrupted chunk");
        let reused_forged_metadata = unchanged_previous_export_artifact(
            &cache,
            Some(&first.project),
            Some(&forged_manifest),
            "git:https://example.invalid/team/repo",
            "repo",
            "team",
            &recipient_fingerprint,
            &changed_export_fingerprint,
            &identity,
            &age_identity,
            &selected_sessions,
        )
        .unwrap();

        assert!(reused_forged_metadata.is_none());
    }

    #[test]
    fn unchanged_previous_export_rebuilds_when_recipients_change() {
        let workspace = unique_test_dir("share-unchanged-export-recipient-change");
        let cache = workspace.join("cache");
        let index_db_path = workspace.join("index.sqlite");
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: index_db_path.clone(),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: workspace.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &index_db_path,
            "repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&context, SharePolicy::All).unwrap();
        let connection = open_index_database_writer(&index_db_path).unwrap();
        let turns = query_share_export_turns(&connection, "repo").unwrap();
        let export_fingerprint = share_export_fingerprint(&turns).unwrap();
        let selected_sessions = query_share_export_session_states(&connection, "repo").unwrap();
        let age_identity = Identity::generate();
        let signing_key = test_signing_key(&age_identity);
        let identity = test_share_identity(&age_identity);
        let first = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            signing_key: &signing_key,
            branch: "team",
            turns,
        })
        .unwrap();
        write_export_artifact(&cache, &first).unwrap();
        let changed_settings = ShareSettings {
            remotes: Vec::new(),
            recipients: vec![ShareRecipient {
                recipient: Identity::generate().to_public().to_string(),
            }],
        };
        let changed_recipient_fingerprint = encryption_recipient_fingerprint(
            &encryption_recipient_strings(&identity, &changed_settings),
        );

        let reused = unchanged_previous_export_artifact(
            &cache,
            Some(&first.project),
            Some(&first.manifest),
            "git:https://example.invalid/team/repo",
            "repo",
            "team",
            &changed_recipient_fingerprint,
            &export_fingerprint,
            &identity,
            &age_identity,
            &selected_sessions,
        )
        .unwrap();

        assert!(reused.is_none());
    }

    #[test]
    fn unchanged_previous_export_rebuilds_when_chunk_is_corrupted() {
        let workspace = unique_test_dir("share-unchanged-export-corrupt-chunk");
        let cache = workspace.join("cache");
        let index_db_path = workspace.join("index.sqlite");
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: index_db_path.clone(),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: workspace.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &index_db_path,
            "repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&context, SharePolicy::All).unwrap();
        let connection = open_index_database_writer(&index_db_path).unwrap();
        let turns = query_share_export_turns(&connection, "repo").unwrap();
        let export_fingerprint = share_export_fingerprint(&turns).unwrap();
        let selected_sessions = query_share_export_session_states(&connection, "repo").unwrap();
        let age_identity = Identity::generate();
        let signing_key = test_signing_key(&age_identity);
        let identity = test_share_identity(&age_identity);
        let recipient_fingerprint = encryption_recipient_fingerprint(
            &encryption_recipient_strings(&identity, &ShareSettings::default()),
        );
        let first = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            signing_key: &signing_key,
            branch: "team",
            turns,
        })
        .unwrap();
        write_export_artifact(&cache, &first).unwrap();
        fs::write(
            cache.join(&first.manifest.chunks[0].object_path),
            b"corrupted chunk",
        )
        .unwrap();

        let reused = unchanged_previous_export_artifact(
            &cache,
            Some(&first.project),
            Some(&first.manifest),
            "git:https://example.invalid/team/repo",
            "repo",
            "team",
            &recipient_fingerprint,
            &export_fingerprint,
            &identity,
            &age_identity,
            &selected_sessions,
        )
        .unwrap();

        assert!(reused.is_none());
    }

    #[test]
    fn corrupted_cached_object_is_not_reused() {
        let workspace = unique_test_dir("share-corrupted-cache");
        let trusted_cache = workspace.join("trusted-cache");
        let index_db_path = workspace.join("index.sqlite");
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: index_db_path.clone(),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: workspace.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &index_db_path,
            "repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&context, SharePolicy::All).unwrap();
        let connection = open_index_database_writer(&index_db_path).unwrap();
        let turns = query_share_export_turns(&connection, "repo").unwrap();
        let age_identity = Identity::generate();
        let signing_key = test_signing_key(&age_identity);
        let identity = test_share_identity(&age_identity);
        let first = build_export_artifact_with_reuse(
            ExportBuildRequest {
                context: &context,
                settings: &ShareSettings::default(),
                project_key: "git:https://example.invalid/team/repo",
                identity: &identity,
                signing_key: &signing_key,
                branch: "team",
                turns: turns.clone(),
            },
            ExportReuseContext {
                trusted_object_cache_path: Some(&trusted_cache),
                decryption_identity: Some(&age_identity),
                previous_project: None,
                previous_manifest: None,
            },
        )
        .unwrap();
        let object_path = first.manifest.chunks[0].object_path.clone();
        let target = trusted_export_object_path(&trusted_cache, &object_path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"not an age payload").unwrap();

        let second = build_export_artifact_with_reuse(
            ExportBuildRequest {
                context: &context,
                settings: &ShareSettings::default(),
                project_key: "git:https://example.invalid/team/repo",
                identity: &identity,
                signing_key: &signing_key,
                branch: "team",
                turns,
            },
            ExportReuseContext {
                trusted_object_cache_path: Some(&trusted_cache),
                decryption_identity: Some(&age_identity),
                previous_project: Some(&first.project),
                previous_manifest: Some(&first.manifest),
            },
        )
        .unwrap();

        assert_ne!(second.objects[&object_path], b"not an age payload");
        let plaintext = decrypt_payload(&second.objects[&object_path], &age_identity).unwrap();
        assert_eq!(
            sha256_hex(&plaintext),
            first.manifest.chunks[0].plaintext_hash
        );
    }

    #[test]
    fn resolve_remote_rejects_credentialed_url() {
        let workspace = unique_test_dir("share-sanitized-remote-report");
        let repo = workspace.join("repo");
        init_test_git_repo(&repo);
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: workspace.join("index.sqlite"),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: repo,
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let settings = ShareSettings {
            remotes: vec![ShareRemote {
                name: "team".to_owned(),
                url: "https://user:token@example.invalid/team/share.git".to_owned(),
            }],
            recipients: Vec::new(),
        };

        let error = resolve_remote(&context, &settings, Some("team")).unwrap_err();

        assert!(format!("{error:#}").contains("must not include URL credentials"));
    }

    #[test]
    fn origin_fallback_uses_resolved_url_without_storing_it() {
        let workspace = unique_test_dir("share-origin-fallback-rewrite");
        let repo = workspace.join("repo");
        init_test_git_repo(&repo);
        run_git(
            &repo,
            ["remote", "add", DEFAULT_REMOTE_NAME, "gh:Example/Darc.git"],
            "failed to add synthetic origin",
        )
        .unwrap();
        run_git(
            &repo,
            [
                "config",
                "url.https://user:token@github.com/.insteadOf",
                "gh:",
            ],
            "failed to add synthetic URL rewrite",
        )
        .unwrap();
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: workspace.join("index.sqlite"),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: repo,
            git_upstream: None,
        };

        let remote = resolve_remote(&context, &ShareSettings::default(), None).unwrap();

        assert_eq!(remote.url, "gh:Example/Darc.git");
        assert_eq!(
            remote.resolved_url,
            "https://user:token@github.com/Example/Darc.git"
        );
        assert_eq!(remote.cache_url, "https://github.com/Example/Darc.git");
        let cache = workspace.join("cache");
        let identity = test_share_identity(&Identity::generate());
        prepare_cache_repository(&cache, &remote.cache_url, &context.local_path, &identity)
            .unwrap();
        let cache_remote = run_git(
            &cache,
            ["remote", "get-url", DEFAULT_REMOTE_NAME],
            "failed to read synthetic cache remote",
        )
        .unwrap()
        .stdout
        .trim()
        .to_owned();
        assert_eq!(cache_remote, "https://github.com/Example/Darc.git");
        assert_eq!(
            project_key(&context).unwrap(),
            "git:https://github.com/Example/Darc"
        );
        assert_eq!(
            cache_repo_path(&workspace, &remote.resolved_url, "darc/team"),
            cache_repo_path(
                &workspace,
                "https://github.com/Example/Darc.git",
                "darc/team"
            )
        );
    }

    #[test]
    fn relative_share_remote_is_resolved_against_project_path() {
        let workspace = unique_test_dir("share-relative-remote");
        let repo = workspace.join("repo");
        let cache = workspace.join("cache");
        init_test_git_repo(&repo);
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: workspace.join("index.sqlite"),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: repo.clone(),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let settings = ShareSettings {
            remotes: vec![ShareRemote {
                name: "team".to_owned(),
                url: "../share.git".to_owned(),
            }],
            recipients: Vec::new(),
        };
        let identity = test_share_identity(&Identity::generate());

        let remote = resolve_remote(&context, &settings, Some("team")).unwrap();
        prepare_cache_repository(&cache, &remote.cache_url, &repo, &identity).unwrap();
        let cache_remote = run_git(
            &cache,
            ["remote", "get-url", DEFAULT_REMOTE_NAME],
            "failed to read synthetic cache remote",
        )
        .unwrap()
        .stdout
        .trim()
        .to_owned();

        assert_eq!(remote.url, "../share.git");
        assert_eq!(
            remote.resolved_url,
            repo.join("../share.git").to_string_lossy().into_owned()
        );
        assert_eq!(remote.cache_url, remote.resolved_url);
        assert_eq!(cache_remote, remote.resolved_url);
    }

    #[test]
    fn merge_skips_malformed_payload_with_warning() {
        let root = unique_test_dir("share-malformed-payload");
        let cache = root.join("cache");
        let key = ensure_share_key(&root).unwrap();
        let identity = read_share_identity_key(&key.key_path).unwrap();
        let signing_key = test_signing_key(&identity);
        let exporter = test_share_identity(&identity);
        let object_path = cache.join(ARTIFACT_ROOT).join("objects").join("bad.age");
        fs::create_dir_all(object_path.parent().unwrap()).unwrap();
        fs::write(&object_path, b"not an age payload").unwrap();
        let turn = TurnManifestEntry {
            provider: SourceKind::Codex,
            session_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            turn_ordinal: 1,
            started_at: "2026-05-15T00:00:00Z".to_owned(),
            payload_hash: "bad-hash".to_owned(),
            object_path: format!("{ARTIFACT_ROOT}/objects/bad.age"),
            chunk_id: None,
            chunk_record_index: None,
        };
        let sync = write_test_sync_object(
            &cache,
            &identity,
            &signing_key,
            &exporter,
            "git:https://example.invalid/team/repo",
            vec![sync_entry_from_manifest(&turn)],
        );
        write_json_file(
            &cache.join(ARTIFACT_ROOT).join("manifest.json"),
            &ManifestArtifact {
                schema: MANIFEST_SCHEMA.to_owned(),
                version: 1,
                project_key: "git:https://example.invalid/team/repo".to_owned(),
                branch: "team".to_owned(),
                exported_at: "2026-05-15T00:00:00Z".to_owned(),
                exporter,
                sync,
                chunks: Vec::new(),
                turns: vec![turn],
            },
        )
        .unwrap();
        let context = ShareProjectContext {
            root: root.clone(),
            index_db_path: root.join("index.sqlite"),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };

        let report = import_from_cache(
            &context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.skipped_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("failed to decrypt share object"),
            "warning should explain decrypt failure: {:?}",
            report.warnings
        );
    }

    #[test]
    fn merge_skips_bad_exporter_sync_and_imports_valid_exporters() {
        let workspace = unique_test_dir("share-bad-sync-continues");
        let cache = workspace.join("cache");
        let source_root = workspace.join("source-root");
        let target_root = workspace.join("target-root");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        let target_key = ensure_share_key(&target_root).unwrap();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let source_age_identity = Identity::generate();
        let source_signing_key = test_signing_key(&source_age_identity);
        let source_identity = test_share_identity(&source_age_identity);
        let source = open_index_database_writer(&source_context.index_db_path).unwrap();
        let turns = query_share_export_turns(&source, "source-repo").unwrap();
        let artifact = build_export_artifact(ExportBuildRequest {
            context: &source_context,
            settings: &ShareSettings {
                remotes: Vec::new(),
                recipients: vec![ShareRecipient {
                    recipient: target_key.public_key,
                }],
            },
            project_key: "git:https://example.invalid/team/repo",
            identity: &source_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
        })
        .unwrap();
        write_export_artifact(&cache, &artifact).unwrap();
        let bad_age_identity = Identity::generate();
        let bad_exporter = test_share_identity(&bad_age_identity);
        write_json_file(
            &cache
                .join(ARTIFACT_ROOT)
                .join(EXPORTERS_DIR)
                .join("bad-sync")
                .join(LEGACY_MANIFEST_FILE),
            &ManifestArtifact {
                schema: MANIFEST_SCHEMA.to_owned(),
                version: 1,
                project_key: "git:https://example.invalid/team/repo".to_owned(),
                branch: "team".to_owned(),
                exported_at: "2026-05-15T00:00:00Z".to_owned(),
                exporter: bad_exporter,
                sync: SyncManifestEntry {
                    payload_hash: "bad".to_owned(),
                    object_path: format!("{ARTIFACT_ROOT}/objects/missing.age"),
                },
                chunks: Vec::new(),
                turns: Vec::new(),
            },
        )
        .unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("bad-sync"),
            "warning should mention skipped bad exporter: {:?}",
            report.warnings
        );
    }

    #[test]
    fn merge_skips_bad_visible_manifests_and_imports_valid_exporters() {
        let workspace = unique_test_dir("share-bad-manifests-continue");
        let cache = workspace.join("cache");
        let source_root = workspace.join("source-root");
        let target_root = workspace.join("target-root");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        let target_key = ensure_share_key(&target_root).unwrap();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let source_age_identity = Identity::generate();
        let source_signing_key = test_signing_key(&source_age_identity);
        let source_identity = test_share_identity(&source_age_identity);
        let source = open_index_database_writer(&source_context.index_db_path).unwrap();
        let turns = query_share_export_turns(&source, "source-repo").unwrap();
        let artifact = build_export_artifact(ExportBuildRequest {
            context: &source_context,
            settings: &ShareSettings {
                remotes: Vec::new(),
                recipients: vec![ShareRecipient {
                    recipient: target_key.public_key,
                }],
            },
            project_key: "git:https://example.invalid/team/repo",
            identity: &source_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
        })
        .unwrap();
        write_export_artifact(&cache, &artifact).unwrap();
        let bad_json_path = cache
            .join(ARTIFACT_ROOT)
            .join(EXPORTERS_DIR)
            .join("bad-json")
            .join(LEGACY_MANIFEST_FILE);
        fs::create_dir_all(bad_json_path.parent().unwrap()).unwrap();
        fs::write(&bad_json_path, b"not json").unwrap();
        for (exporter_dir, schema, version, project_key) in [
            (
                "future-schema",
                "darc.share.manifest.v999",
                1,
                "git:https://example.invalid/team/repo",
            ),
            (
                "future-version",
                MANIFEST_SCHEMA,
                2,
                "git:https://example.invalid/team/repo",
            ),
            (
                "foreign-project",
                MANIFEST_SCHEMA,
                1,
                "git:https://example.invalid/other/repo",
            ),
        ] {
            let bad_identity = Identity::generate();
            write_json_file(
                &cache
                    .join(ARTIFACT_ROOT)
                    .join(EXPORTERS_DIR)
                    .join(exporter_dir)
                    .join(LEGACY_MANIFEST_FILE),
                &ManifestArtifact {
                    schema: schema.to_owned(),
                    version,
                    project_key: project_key.to_owned(),
                    branch: "team".to_owned(),
                    exported_at: "2026-05-15T00:00:00Z".to_owned(),
                    exporter: test_share_identity(&bad_identity),
                    sync: SyncManifestEntry {
                        payload_hash: "bad".to_owned(),
                        object_path: format!("{ARTIFACT_ROOT}/objects/missing.age"),
                    },
                    chunks: Vec::new(),
                    turns: Vec::new(),
                },
            )
            .unwrap();
        }

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 1);
        assert_eq!(report.warning_count, 4);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("bad-json")),
            "warnings should mention bad JSON manifest: {:?}",
            report.warnings
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("future-schema")),
            "warnings should mention future schema manifest: {:?}",
            report.warnings
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("future-version")),
            "warnings should mention future version manifest: {:?}",
            report.warnings
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("foreign-project")),
            "warnings should mention foreign project manifest: {:?}",
            report.warnings
        );
    }

    #[test]
    fn merge_skips_malformed_exporter_root_and_imports_legacy_manifest() {
        let workspace = unique_test_dir("share-bad-exporter-root-continue");
        let cache = workspace.join("cache");
        let source_root = workspace.join("source-root");
        let target_root = workspace.join("target-root");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        let target_key = ensure_share_key(&target_root).unwrap();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let source_age_identity = Identity::generate();
        let source_signing_key = test_signing_key(&source_age_identity);
        let source_identity = test_share_identity(&source_age_identity);
        let source = open_index_database_writer(&source_context.index_db_path).unwrap();
        let turns = query_share_export_turns(&source, "source-repo").unwrap();
        let artifact = build_export_artifact(ExportBuildRequest {
            context: &source_context,
            settings: &ShareSettings {
                remotes: Vec::new(),
                recipients: vec![ShareRecipient {
                    recipient: target_key.public_key,
                }],
            },
            project_key: "git:https://example.invalid/team/repo",
            identity: &source_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
        })
        .unwrap();
        write_export_artifact(&cache, &artifact).unwrap();
        write_json_file(
            &cache.join(ARTIFACT_ROOT).join(LEGACY_MANIFEST_FILE),
            &artifact.manifest,
        )
        .unwrap();
        let exporter_root = cache.join(ARTIFACT_ROOT).join(EXPORTERS_DIR);
        fs::remove_dir_all(&exporter_root).unwrap();
        fs::write(&exporter_root, b"not a directory").unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("exporter root"),
            "warning should mention malformed exporter root: {:?}",
            report.warnings
        );
    }

    #[test]
    fn merge_skips_forged_sync_signature() {
        let workspace = unique_test_dir("share-forged-sync-signature");
        let cache = workspace.join("cache");
        let source_root = workspace.join("source-root");
        let target_root = workspace.join("target-root");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        let target_key = ensure_share_key(&target_root).unwrap();
        let target_identity = read_share_identity_key(&target_key.key_path).unwrap();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let source_age_identity = Identity::generate();
        let source_signing_key = test_signing_key(&source_age_identity);
        let source_identity = test_share_identity(&source_age_identity);
        let source = open_index_database_writer(&source_context.index_db_path).unwrap();
        let turns = query_share_export_turns(&source, "source-repo").unwrap();
        let artifact = build_export_artifact(ExportBuildRequest {
            context: &source_context,
            settings: &ShareSettings {
                remotes: Vec::new(),
                recipients: vec![ShareRecipient {
                    recipient: target_key.public_key,
                }],
            },
            project_key: "git:https://example.invalid/team/repo",
            identity: &source_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
        })
        .unwrap();
        write_export_artifact(&cache, &artifact).unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let attacker = Identity::generate();
        let attacker_signing_key = test_signing_key(&attacker);
        let mut forged_sync = EncryptedSyncPayload {
            schema: SYNC_PAYLOAD_SCHEMA.to_owned(),
            version: SYNC_PAYLOAD_VERSION,
            project_key: "git:https://example.invalid/team/repo".to_owned(),
            export_fingerprint: String::new(),
            exporter: source_identity,
            signature: None,
            sessions: Vec::new(),
            chunks: Vec::new(),
            turns: manifest
                .turns
                .iter()
                .map(sync_entry_from_manifest)
                .collect(),
        };
        sign_sync_payload(&mut forged_sync, &attacker_signing_key).unwrap();
        let forged_plaintext = serde_json::to_vec(&forged_sync).unwrap();
        let forged_object_path = format!(
            "{ARTIFACT_ROOT}/objects/forged-{}.age",
            &sha256_hex(&forged_plaintext)[..16]
        );
        let forged_target = cache.join(&forged_object_path);
        fs::create_dir_all(forged_target.parent().unwrap()).unwrap();
        fs::write(
            &forged_target,
            encrypt_payload(&forged_plaintext, &[target_identity.to_public()]).unwrap(),
        )
        .unwrap();
        manifest.sync = SyncManifestEntry {
            payload_hash: sha256_hex(&forged_plaintext),
            object_path: forged_object_path,
        };
        write_json_file(&manifest_path, &manifest).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("signature"),
            "warning should reject forged signature: {:?}",
            report.warnings
        );
    }

    #[test]
    fn merge_uses_valid_legacy_manifest_after_invalid_exporter_manifest() {
        let TestShareArtifact {
            cache,
            target_context,
            source_identity,
            mut artifact,
            ..
        } = build_single_turn_test_artifact("share-invalid-exporter-valid-legacy");
        write_export_artifact(&cache, &artifact).unwrap();
        write_json_file(
            &cache.join(ARTIFACT_ROOT).join(LEGACY_MANIFEST_FILE),
            &artifact.manifest,
        )
        .unwrap();
        artifact.manifest.turns[0].payload_hash = "invalid-visible-hash".to_owned();
        write_json_file(
            &cache.join(exporter_manifest_relative_path(&source_identity)),
            &artifact.manifest,
        )
        .unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 1);
        assert_eq!(report.skipped_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("signed sync entries"),
            "warning should reject the invalid exporter manifest before importing legacy fallback: {:?}",
            report.warnings
        );
    }

    #[test]
    fn merge_skips_forged_turn_signature() {
        let workspace = unique_test_dir("share-forged-turn-signature");
        let cache = workspace.join("cache");
        let source_root = workspace.join("source-root");
        let target_root = workspace.join("target-root");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        let target_key = ensure_share_key(&target_root).unwrap();
        let target_identity = read_share_identity_key(&target_key.key_path).unwrap();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let source_age_identity = Identity::generate();
        let source_signing_key = test_signing_key(&source_age_identity);
        let source_identity = test_share_identity(&source_age_identity);
        let source = open_index_database_writer(&source_context.index_db_path).unwrap();
        let turns = query_share_export_turns(&source, "source-repo").unwrap();
        let artifact = build_export_artifact(ExportBuildRequest {
            context: &source_context,
            settings: &ShareSettings {
                remotes: Vec::new(),
                recipients: vec![ShareRecipient {
                    recipient: target_key.public_key,
                }],
            },
            project_key: "git:https://example.invalid/team/repo",
            identity: &source_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
        })
        .unwrap();
        write_export_artifact(&cache, &artifact).unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let mut chunk_payload = read_test_chunk_payload(&cache, &manifest, &target_identity);
        chunk_payload.turns[0].turn.user_message = "forged task".to_owned();
        let forged_plaintext = serde_json::to_vec(&chunk_payload.turns[0]).unwrap();
        manifest.turns[0].payload_hash = sha256_hex(&forged_plaintext);
        write_test_chunk_payload(
            &cache,
            &mut manifest,
            &chunk_payload,
            &[target_identity.to_public()],
        );
        manifest.sync = write_test_sync_object_with_chunks(
            &cache,
            &target_identity,
            &source_signing_key,
            &source_identity,
            "git:https://example.invalid/team/repo",
            sync_chunks_from_manifest(&manifest),
            vec![sync_entry_from_manifest(&manifest.turns[0])],
        );
        write_json_file(&manifest_path, &manifest).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.skipped_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("signature")),
            "warning should reject forged turn signature: {:?}",
            report.warnings
        );
    }

    #[test]
    fn merge_skips_unsupported_payload_versions() {
        let TestShareArtifact {
            cache,
            target_context,
            target_identity,
            source_identity,
            source_signing_key,
            artifact,
        } = build_single_turn_test_artifact("share-unsupported-payload-version");
        write_export_artifact(&cache, &artifact).unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let sync_ciphertext = fs::read(cache.join(&manifest.sync.object_path)).unwrap();
        let sync_plaintext = decrypt_payload(&sync_ciphertext, &target_identity).unwrap();
        let mut sync_payload: EncryptedSyncPayload =
            serde_json::from_slice(&sync_plaintext).unwrap();
        sync_payload.version = SYNC_PAYLOAD_VERSION + 1;
        let sync_plaintext = serde_json::to_vec(&sync_payload).unwrap();
        let sync_object_path = format!(
            "{ARTIFACT_ROOT}/objects/sync-v3-{}.age",
            &sha256_hex(&sync_plaintext)[..16]
        );
        let sync_target = cache.join(&sync_object_path);
        fs::create_dir_all(sync_target.parent().unwrap()).unwrap();
        fs::write(
            &sync_target,
            encrypt_payload(&sync_plaintext, &[target_identity.to_public()]).unwrap(),
        )
        .unwrap();
        manifest.sync = SyncManifestEntry {
            payload_hash: sha256_hex(&sync_plaintext),
            object_path: sync_object_path,
        };
        write_json_file(&manifest_path, &manifest).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.skipped_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("sync payload version"),
            "warning should reject unsupported sync payload version: {:?}",
            report.warnings
        );

        write_export_artifact(&cache, &artifact).unwrap();
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let mut chunk_payload = read_test_chunk_payload(&cache, &manifest, &target_identity);
        chunk_payload.turns[0].version = 2;
        let turn_plaintext = serde_json::to_vec(&chunk_payload.turns[0]).unwrap();
        manifest.turns[0].payload_hash = sha256_hex(&turn_plaintext);
        write_test_chunk_payload(
            &cache,
            &mut manifest,
            &chunk_payload,
            &[target_identity.to_public()],
        );
        manifest.sync = write_test_sync_object_with_chunks(
            &cache,
            &target_identity,
            &source_signing_key,
            &source_identity,
            "git:https://example.invalid/team/repo",
            sync_chunks_from_manifest(&manifest),
            vec![sync_entry_from_manifest(&manifest.turns[0])],
        );
        write_json_file(&manifest_path, &manifest).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.skipped_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("share payload version")),
            "warning should reject unsupported turn payload version: {:?}",
            report.warnings
        );
    }

    #[test]
    fn merge_uses_canonical_remote_provenance_across_aliases() {
        let TestShareArtifact {
            cache,
            target_context,
            artifact,
            ..
        } = build_single_turn_test_artifact("share-canonical-remote-alias");
        write_export_artifact(&cache, &artifact).unwrap();
        let first_remote = "git@example.invalid:team/share.git";
        let second_remote = "https://bob:token@example.invalid/team/share.git";

        let first = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "first-alias",
            first_remote,
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();
        let second = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "second-alias",
            second_remote,
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        let target = open_index_database_writer(&target_context.index_db_path).unwrap();
        let stored: (i64, String) = target
            .query_row(
                "
                SELECT COUNT(*), MIN(origin_remote)
                FROM sessions
                WHERE project_id = 'target-repo'
                    AND origin_kind = 'shared'
                ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(first.imported_turn_count, 1);
        assert_eq!(first.warning_count, 0);
        assert_eq!(second.imported_turn_count, 1);
        assert_eq!(second.warning_count, 0);
        assert_eq!(stored.0, 1);
        assert_eq!(stored.1, share_origin_remote(first_remote, "darc/team"));
        assert_eq!(stored.1, share_origin_remote(second_remote, "darc/team"));
        assert!(!stored.1.contains("first-alias"));
        assert!(!stored.1.contains("second-alias"));
    }

    #[test]
    fn merge_retargeted_alias_does_not_prune_previous_remote_rows() {
        let TestShareArtifact {
            cache,
            target_context,
            target_identity,
            source_identity,
            source_signing_key,
            artifact,
        } = build_single_turn_test_artifact("share-canonical-remote-retarget");
        write_export_artifact(&cache, &artifact).unwrap();
        let first_remote = "https://example.invalid/team/share-a.git";
        let second_remote = "https://example.invalid/team/share-b.git";
        import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "share",
            first_remote,
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();
        let mut empty_manifest = artifact.manifest.clone();
        empty_manifest.chunks.clear();
        empty_manifest.sync = write_test_sync_object(
            &cache,
            &target_identity,
            &source_signing_key,
            &source_identity,
            "git:https://example.invalid/team/repo",
            Vec::new(),
        );
        empty_manifest.turns.clear();
        write_json_file(
            &cache.join(exporter_manifest_relative_path(&source_identity)),
            &empty_manifest,
        )
        .unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "share",
            second_remote,
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        let target = open_index_database_writer(&target_context.index_db_path).unwrap();
        let stored: (i64, String) = target
            .query_row(
                "
                SELECT COUNT(*), MIN(origin_remote)
                FROM sessions
                WHERE project_id = 'target-repo'
                    AND origin_kind = 'shared'
                ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.warning_count, 0);
        assert_eq!(stored.0, 1);
        assert_eq!(stored.1, share_origin_remote(first_remote, "darc/team"));
        assert_ne!(stored.1, share_origin_remote(second_remote, "darc/team"));
    }

    #[test]
    fn merge_skips_manifest_turns_missing_from_signed_sync_payload() {
        let TestShareArtifact {
            cache,
            target_context,
            target_identity,
            source_identity,
            source_signing_key,
            artifact,
        } = build_single_turn_test_artifact("share-unauthenticated-manifest-turn");
        write_export_artifact(&cache, &artifact).unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let original_turn = manifest.turns[0].clone();
        let mut chunk_payload = read_test_chunk_payload(&cache, &manifest, &target_identity);
        let mut extra_payload = chunk_payload.turns[0].clone();
        extra_payload.turn.turn_ordinal = 99;
        extra_payload.turn.turn_id = Some("turn-extra".to_owned());
        extra_payload.turn.started_at = "2026-05-15T23:59:59Z".to_owned();
        sign_turn_payload(&mut extra_payload, &source_signing_key).unwrap();
        let extra_plaintext = serde_json::to_vec(&extra_payload).unwrap();
        let extra_record_index = u32::try_from(chunk_payload.turns.len()).unwrap();
        chunk_payload.turns.push(extra_payload.clone());
        manifest.turns.push(TurnManifestEntry {
            provider: original_turn.provider,
            session_id: original_turn.session_id,
            turn_ordinal: extra_payload.turn.turn_ordinal,
            started_at: extra_payload.turn.started_at,
            payload_hash: sha256_hex(&extra_plaintext),
            object_path: original_turn.object_path,
            chunk_id: original_turn.chunk_id,
            chunk_record_index: Some(extra_record_index),
        });
        write_test_chunk_payload(
            &cache,
            &mut manifest,
            &chunk_payload,
            &[target_identity.to_public()],
        );
        manifest.sync = write_test_sync_object_with_chunks(
            &cache,
            &target_identity,
            &source_signing_key,
            &source_identity,
            "git:https://example.invalid/team/repo",
            sync_chunks_from_manifest(&manifest),
            vec![sync_entry_from_manifest(&manifest.turns[0])],
        );
        write_json_file(&manifest_path, &manifest).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        let target = open_index_database_writer(&target_context.index_db_path).unwrap();
        let turn_count: i64 = target
            .query_row(
                "
                SELECT COUNT(*)
                FROM turns
                JOIN sessions
                    ON sessions.project_id = turns.project_id
                    AND sessions.provider = turns.provider
                    AND sessions.session_id = turns.session_id
                WHERE sessions.project_id = 'target-repo'
                    AND sessions.origin_kind = 'shared'
                ",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.skipped_turn_count, 2);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("do not match visible manifest entries"),
            "warning should reject unauthenticated manifest turn: {:?}",
            report.warnings
        );
        assert_eq!(turn_count, 0);
    }

    #[test]
    fn merge_rejects_extra_signed_sync_entries() {
        let TestShareArtifact {
            cache,
            target_context,
            target_identity,
            source_identity,
            source_signing_key,
            artifact,
        } = build_single_turn_test_artifact("share-extra-sync-entry");
        write_export_artifact(&cache, &artifact).unwrap();
        let first_report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let stale_entry = sync_entry_from_manifest(&manifest.turns[0]);
        manifest.turns.clear();
        manifest.sync = write_test_sync_object_with_chunks(
            &cache,
            &target_identity,
            &source_signing_key,
            &source_identity,
            "git:https://example.invalid/team/repo",
            sync_chunks_from_manifest(&manifest),
            vec![stale_entry],
        );
        write_json_file(&manifest_path, &manifest).unwrap();

        let second_report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        let connection = open_index_database_writer(&target_context.index_db_path).unwrap();
        let imported_turn_count: i64 = connection
            .query_row(
                "
                SELECT COUNT(*)
                FROM turns
                JOIN sessions
                    ON sessions.project_id = turns.project_id
                    AND sessions.provider = turns.provider
                    AND sessions.session_id = turns.session_id
                WHERE sessions.project_id = 'target-repo'
                    AND sessions.origin_kind = 'shared'
                ",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(first_report.imported_turn_count, 1);
        assert_eq!(second_report.imported_turn_count, 0);
        assert_eq!(second_report.warning_count, 1);
        assert!(
            second_report.warnings[0].contains("do not match visible manifest entries"),
            "warning should reject extra signed sync entries: {:?}",
            second_report.warnings
        );
        assert_eq!(imported_turn_count, 1);
    }

    #[test]
    fn merge_preserves_stale_turn_when_replacement_object_is_invalid() {
        let TestShareArtifact {
            cache,
            target_context,
            target_identity,
            source_identity,
            source_signing_key,
            artifact,
        } = build_single_turn_test_artifact("share-corrupt-replacement-prunes");
        write_export_artifact(&cache, &artifact).unwrap();
        let first_report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let object_path = manifest_object_path(&cache, &manifest.turns[0]).unwrap();
        fs::write(&object_path, b"not an age payload").unwrap();
        manifest.chunks[0].ciphertext_hash = sha256_hex(b"not an age payload");
        manifest.chunks[0].ciphertext_bytes = u64::try_from(b"not an age payload".len()).unwrap();
        manifest.sync = write_test_sync_object_with_chunks(
            &cache,
            &target_identity,
            &source_signing_key,
            &source_identity,
            "git:https://example.invalid/team/repo",
            sync_chunks_from_manifest(&manifest),
            vec![sync_entry_from_manifest(&manifest.turns[0])],
        );
        write_json_file(&manifest_path, &manifest).unwrap();

        let second_report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        let connection = open_index_database_writer(&target_context.index_db_path).unwrap();
        let imported_turn_count: i64 = connection
            .query_row(
                "
                SELECT COUNT(*)
                FROM turns
                JOIN sessions
                    ON sessions.project_id = turns.project_id
                    AND sessions.provider = turns.provider
                    AND sessions.session_id = turns.session_id
                WHERE sessions.project_id = 'target-repo'
                    AND sessions.origin_kind = 'shared'
                ",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(first_report.imported_turn_count, 1);
        assert_eq!(second_report.imported_turn_count, 0);
        assert_eq!(second_report.skipped_turn_count, 1);
        assert_eq!(second_report.warning_count, 1);
        assert!(
            second_report
                .warnings
                .iter()
                .any(|warning| warning.contains("failed to decrypt share chunk")),
            "warning should identify the invalid replacement object: {:?}",
            second_report.warnings
        );
        assert_eq!(imported_turn_count, 1);
    }

    #[test]
    fn merge_imports_valid_chunks_when_one_chunk_is_bad() {
        let TestShareArtifact {
            cache,
            target_context,
            target_identity,
            source_identity,
            source_signing_key,
            artifact,
            ..
        } = build_multi_chunk_test_artifact("share-bad-chunk-isolation");
        write_export_artifact(&cache, &artifact).unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let bad_chunk_id = manifest.chunks[1].chunk_id.clone();
        let bad_object_path = manifest.chunks[1].object_path.clone();
        fs::write(cache.join(&bad_object_path), b"not an age payload").unwrap();
        manifest.chunks[1].ciphertext_hash = sha256_hex(b"not an age payload");
        manifest.chunks[1].ciphertext_bytes = u64::try_from(b"not an age payload".len()).unwrap();
        manifest.sync = write_test_sync_object_with_chunks(
            &cache,
            &target_identity,
            &source_signing_key,
            &source_identity,
            "git:https://example.invalid/team/repo",
            sync_chunks_from_manifest(&manifest),
            manifest
                .turns
                .iter()
                .map(sync_entry_from_manifest)
                .collect(),
        );
        write_json_file(&manifest_path, &manifest).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();
        let expected_imported_count = manifest
            .turns
            .iter()
            .filter(|turn| turn.chunk_id.as_deref() != Some(bad_chunk_id.as_str()))
            .count();

        assert_eq!(
            report.imported_turn_count,
            u64::try_from(expected_imported_count).unwrap()
        );
        assert_eq!(
            report.skipped_turn_count,
            u64::try_from(manifest.turns.len() - expected_imported_count).unwrap()
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains(&bad_chunk_id)
                    && warning.contains("failed to decrypt share chunk")),
            "warning should identify the bad chunk: {:?}",
            report.warnings
        );
    }

    #[test]
    fn merge_rejects_oversized_decompressed_chunks() {
        let TestShareArtifact {
            cache,
            target_context,
            target_identity,
            source_identity,
            source_signing_key,
            artifact,
            ..
        } = build_single_turn_test_artifact("share-oversized-chunk");
        write_export_artifact(&cache, &artifact).unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let oversized_plaintext =
            vec![b'x'; usize::try_from(MAX_SHARE_CHUNK_DECOMPRESSED_BYTES + 1).unwrap()];
        let compressed = gzip_compress(&oversized_plaintext).unwrap();
        let ciphertext = encrypt_payload(&compressed, &[target_identity.to_public()]).unwrap();
        fs::write(cache.join(&manifest.chunks[0].object_path), &ciphertext).unwrap();
        manifest.chunks[0].plaintext_hash = sha256_hex(&compressed);
        manifest.chunks[0].ciphertext_hash = sha256_hex(&ciphertext);
        manifest.chunks[0].plaintext_bytes = u64::try_from(compressed.len()).unwrap();
        manifest.chunks[0].ciphertext_bytes = u64::try_from(ciphertext.len()).unwrap();
        manifest.sync = write_test_sync_object_with_chunks(
            &cache,
            &target_identity,
            &source_signing_key,
            &source_identity,
            "git:https://example.invalid/team/repo",
            sync_chunks_from_manifest(&manifest),
            vec![sync_entry_from_manifest(&manifest.turns[0])],
        );
        write_json_file(&manifest_path, &manifest).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.skipped_turn_count, 1);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("decompressed share chunk exceeds")),
            "warning should identify decompression cap: {:?}",
            report.warnings
        );
    }

    #[test]
    fn retention_ignores_unreferenced_chunk_entries() {
        let TestShareArtifact {
            cache,
            target_identity,
            artifact,
            ..
        } = build_single_turn_test_artifact("share-unreferenced-chunk");
        write_export_artifact(&cache, &artifact).unwrap();
        let mut manifest = artifact.manifest.clone();
        let orphan_path = format!("{ARTIFACT_ROOT}/objects/orphan.age");
        manifest.chunks.push(ChunkManifestEntry {
            chunk_id: "orphan-chunk".to_owned(),
            object_path: orphan_path.clone(),
            compression: "gzip".to_owned(),
            plaintext_hash: "bad".to_owned(),
            ciphertext_hash: "bad".to_owned(),
            plaintext_bytes: 1,
            ciphertext_bytes: 1,
            turn_count: 1,
        });

        verify_cached_manifest_payloads(
            &cache,
            &manifest,
            "git:https://example.invalid/team/repo",
            &target_identity,
        )
        .unwrap();

        assert!(!manifest_object_paths(&manifest).contains(&orphan_path));
    }

    #[test]
    fn merge_rejects_unsigned_unreferenced_chunks_on_legacy_manifest() {
        let root = unique_test_dir("share-legacy-extra-chunk");
        let cache = root.join("cache");
        let key = ensure_share_key(&root).unwrap();
        let identity = read_share_identity_key(&key.key_path).unwrap();
        let signing_key = test_signing_key(&identity);
        let exporter = test_share_identity(&identity);
        let turn_payload = EncryptedTurnPayload {
            schema: TURN_PAYLOAD_SCHEMA.to_owned(),
            version: 1,
            project_key: "git:https://example.invalid/team/repo".to_owned(),
            exporter: exporter.clone(),
            signature: None,
            turn: synthetic_share_turn("repo", "00000000-0000-4000-8000-000000000001", 1),
        };
        let mut turn_payload = turn_payload;
        sign_turn_payload(&mut turn_payload, &signing_key).unwrap();
        let plaintext = serde_json::to_vec(&turn_payload).unwrap();
        let object_path = format!(
            "{ARTIFACT_ROOT}/objects/legacy-turn-{}.age",
            &sha256_hex(&plaintext)[..16]
        );
        let target = cache.join(&object_path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(
            &target,
            encrypt_payload(&plaintext, &[identity.to_public()]).unwrap(),
        )
        .unwrap();
        let turn = TurnManifestEntry {
            provider: SourceKind::Codex,
            session_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            turn_ordinal: 1,
            started_at: "2026-05-15T00:00:00Z".to_owned(),
            payload_hash: sha256_hex(&plaintext),
            object_path,
            chunk_id: None,
            chunk_record_index: None,
        };
        let sync = write_test_sync_object(
            &cache,
            &identity,
            &signing_key,
            &exporter,
            "git:https://example.invalid/team/repo",
            vec![sync_entry_from_manifest(&turn)],
        );
        write_json_file(
            &cache.join(ARTIFACT_ROOT).join("manifest.json"),
            &ManifestArtifact {
                schema: MANIFEST_SCHEMA.to_owned(),
                version: 1,
                project_key: "git:https://example.invalid/team/repo".to_owned(),
                branch: "team".to_owned(),
                exported_at: "2026-05-15T00:00:00Z".to_owned(),
                exporter,
                sync,
                chunks: vec![ChunkManifestEntry {
                    chunk_id: "unsigned-orphan".to_owned(),
                    object_path: format!("{ARTIFACT_ROOT}/objects/missing-orphan.age"),
                    compression: "gzip".to_owned(),
                    plaintext_hash: "bad".to_owned(),
                    ciphertext_hash: "bad".to_owned(),
                    plaintext_bytes: 1,
                    ciphertext_bytes: 1,
                    turn_count: 1,
                }],
                turns: vec![turn],
            },
        )
        .unwrap();
        let context = ShareProjectContext {
            root: root.clone(),
            index_db_path: root.join("index.sqlite"),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };

        let report = import_from_cache(
            &context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.skipped_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("signed sync chunks do not match visible manifest chunks"),
            "warning should reject unsigned chunk metadata: {:?}",
            report.warnings
        );
    }

    #[test]
    fn merge_skips_chunked_manifest_without_visible_chunk_metadata() {
        let TestShareArtifact {
            cache,
            target_context,
            source_identity,
            artifact,
            ..
        } = build_single_turn_test_artifact("share-missing-chunk-metadata");
        write_export_artifact(&cache, &artifact).unwrap();
        let first_report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        manifest.chunks.clear();
        write_json_file(&manifest_path, &manifest).unwrap();

        let second_report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();
        let connection = open_index_database_writer(&target_context.index_db_path).unwrap();
        let imported_turn_count: i64 = connection
            .query_row(
                "
                SELECT COUNT(*)
                FROM turns
                JOIN sessions
                    ON sessions.project_id = turns.project_id
                    AND sessions.provider = turns.provider
                    AND sessions.session_id = turns.session_id
                WHERE sessions.project_id = 'target-repo'
                    AND sessions.origin_kind = 'shared'
                ",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(first_report.imported_turn_count, 1);
        assert_eq!(second_report.imported_turn_count, 0);
        assert_eq!(second_report.skipped_turn_count, 1);
        assert_eq!(second_report.warning_count, 1);
        assert!(
            second_report.warnings[0]
                .contains("signed sync chunks do not match visible manifest chunks"),
            "warning should reject unsigned chunk metadata edits: {:?}",
            second_report.warnings
        );
        assert_eq!(imported_turn_count, 1);
    }

    #[test]
    fn merge_rejects_replayed_manifest_entry_hashes() {
        let TestShareArtifact {
            cache,
            target_context,
            target_identity,
            source_identity,
            source_signing_key,
            artifact,
        } = build_single_turn_test_artifact("share-replayed-manifest-entry");
        write_export_artifact(&cache, &artifact).unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let mut current_entry = manifest.turns[0].clone();
        current_entry.payload_hash = sha256_hex(b"newer-current-turn-payload");
        current_entry.object_path = format!("{ARTIFACT_ROOT}/objects/newer-current-turn.age");
        manifest.sync = write_test_sync_object_with_chunks(
            &cache,
            &target_identity,
            &source_signing_key,
            &source_identity,
            "git:https://example.invalid/team/repo",
            sync_chunks_from_manifest(&manifest),
            vec![sync_entry_from_manifest(&current_entry)],
        );
        write_json_file(&manifest_path, &manifest).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.skipped_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("do not match visible manifest entries"),
            "warning should reject stale manifest metadata replay: {:?}",
            report.warnings
        );
    }

    #[test]
    fn merge_authenticates_duplicate_exporter_before_deduping() {
        let TestShareArtifact {
            cache,
            target_context,
            source_identity,
            artifact,
            ..
        } = build_single_turn_test_artifact("share-duplicate-exporter-auth");
        write_export_artifact(&cache, &artifact).unwrap();
        let mut bogus_manifest = artifact.manifest.clone();
        bogus_manifest.sync = SyncManifestEntry {
            payload_hash: "missing-sync-payload".to_owned(),
            object_path: format!("{ARTIFACT_ROOT}/objects/missing-sync.age"),
        };
        let bogus_manifest_path = cache
            .join(ARTIFACT_ROOT)
            .join(EXPORTERS_DIR)
            .join("0000-bogus")
            .join(LEGACY_MANIFEST_FILE);
        fs::create_dir_all(bogus_manifest_path.parent().unwrap()).unwrap();
        write_json_file(&bogus_manifest_path, &bogus_manifest).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        let target = open_index_database_writer(&target_context.index_db_path).unwrap();
        let imported_sessions: i64 = target
            .query_row(
                "
                SELECT COUNT(*)
                FROM sessions
                WHERE project_id = 'target-repo'
                    AND origin_kind = 'shared'
                    AND origin_user_id = ?1
                ",
                [&source_identity.user_id],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(report.imported_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("missing-sync"),
            "warning should describe bogus duplicate sync failure: {:?}",
            report.warnings
        );
        assert_eq!(imported_sessions, 1);
    }

    #[test]
    fn merge_rejects_manifest_in_wrong_exporter_directory() {
        let TestShareArtifact {
            cache,
            target_context,
            source_identity,
            artifact,
            ..
        } = build_single_turn_test_artifact("share-wrong-exporter-dir");
        write_export_artifact(&cache, &artifact).unwrap();
        let canonical_manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let bogus_manifest_path = cache
            .join(ARTIFACT_ROOT)
            .join(EXPORTERS_DIR)
            .join("0000-bogus")
            .join(LEGACY_MANIFEST_FILE);
        fs::create_dir_all(bogus_manifest_path.parent().unwrap()).unwrap();
        fs::copy(&canonical_manifest_path, &bogus_manifest_path).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("manifest path does not match exporter identity"),
            "warning should reject wrong exporter directory: {:?}",
            report.warnings
        );
    }

    #[test]
    fn retention_skips_unsupported_visible_manifest_versions() {
        let TestShareArtifact {
            cache,
            target_identity,
            source_identity,
            artifact,
            ..
        } = build_single_turn_test_artifact("share-retain-visible-version");
        write_export_artifact(&cache, &artifact).unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        manifest.version = 2;
        write_json_file(&manifest_path, &manifest).unwrap();

        let cached = read_cached_manifests(&cache).unwrap();
        let next_exporter = test_share_identity(&Identity::generate());
        let retained = authenticated_retained_manifests(
            &cache,
            &cached.manifests,
            "git:https://example.invalid/team/repo",
            &next_exporter,
            &target_identity,
        );

        assert!(retained.unwrap().is_empty());
    }

    #[test]
    fn retention_rejects_extra_signed_sync_entries() {
        let TestShareArtifact {
            cache,
            target_identity,
            source_identity,
            source_signing_key,
            artifact,
            ..
        } = build_single_turn_test_artifact("share-retain-extra-sync");
        write_export_artifact(&cache, &artifact).unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let extra_entry = sync_entry_from_manifest(&manifest.turns[0]);
        manifest.turns.clear();
        manifest.sync = write_test_sync_object_with_chunks(
            &cache,
            &target_identity,
            &source_signing_key,
            &source_identity,
            "git:https://example.invalid/team/repo",
            sync_chunks_from_manifest(&manifest),
            vec![extra_entry],
        );
        write_json_file(&manifest_path, &manifest).unwrap();

        let cached = read_cached_manifests(&cache).unwrap();
        let next_exporter = test_share_identity(&Identity::generate());
        let retained = authenticated_retained_manifests(
            &cache,
            &cached.manifests,
            "git:https://example.invalid/team/repo",
            &next_exporter,
            &target_identity,
        );

        let Err(error) = retained else {
            panic!("retention should reject unauthenticated metadata");
        };
        assert!(
            error
                .to_string()
                .contains("failed to authenticate retained share manifest"),
            "retention should fail closed on unauthenticated metadata: {error:#}"
        );
    }

    #[test]
    fn merge_rejects_manifest_started_at_mismatch() {
        let TestShareArtifact {
            cache,
            target_context,
            target_identity,
            source_identity,
            source_signing_key,
            artifact,
        } = build_single_turn_test_artifact("share-started-at-mismatch");
        write_export_artifact(&cache, &artifact).unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        manifest.turns[0].started_at = "2026-05-15T23:59:59Z".to_owned();
        manifest.sync = write_test_sync_object_with_chunks(
            &cache,
            &target_identity,
            &source_signing_key,
            &source_identity,
            "git:https://example.invalid/team/repo",
            sync_chunks_from_manifest(&manifest),
            vec![sync_entry_from_manifest(&manifest.turns[0])],
        );
        write_json_file(&manifest_path, &manifest).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.skipped_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("identity does not match"),
            "warning should reject started_at mismatch: {:?}",
            report.warnings
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_rejects_symlinked_object_ancestors() {
        use std::os::unix::fs::symlink;

        let TestShareArtifact {
            cache,
            target_context,
            artifact,
            ..
        } = build_single_turn_test_artifact("share-symlink-object-ancestor");
        write_export_artifact(&cache, &artifact).unwrap();
        let outside = cache
            .parent()
            .expect("cache should have parent")
            .join("outside-objects");
        let object_root = cache.join(ARTIFACT_ROOT).join("objects");
        fs::remove_dir_all(&object_root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("outside.age"), b"outside").unwrap();
        symlink(&outside, &object_root).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("ancestor is a symlink"),
            "warning should reject symlinked object ancestor: {:?}",
            report.warnings
        );
        assert!(outside.join("outside.age").exists());
    }

    #[cfg(unix)]
    #[test]
    fn object_removal_rejects_symlinked_artifact_ancestors() {
        use std::os::unix::fs::symlink;

        let workspace = unique_test_dir("share-remove-symlink-object-ancestor");
        let cache = workspace.join("cache");
        let outside = workspace.join("outside-objects");
        let object_root = cache.join(ARTIFACT_ROOT).join("objects");
        fs::create_dir_all(object_root.parent().unwrap()).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("orphan.age"), b"outside").unwrap();
        symlink(&outside, &object_root).unwrap();

        let error = remove_artifact_object(&cache, &format!("{ARTIFACT_ROOT}/objects/orphan.age"))
            .unwrap_err();

        assert!(
            error.to_string().contains("ancestor is a symlink"),
            "removal should reject symlinked object ancestor: {error:#}"
        );
        assert!(outside.join("orphan.age").exists());
    }

    #[test]
    fn merge_skips_unsafe_manifest_object_paths_with_warning() {
        let root = unique_test_dir("share-unsafe-object-path");
        let cache = root.join("cache");
        let key = ensure_share_key(&root).unwrap();
        let identity = read_share_identity_key(&key.key_path).unwrap();
        let signing_key = test_signing_key(&identity);
        let exporter = test_share_identity(&identity);
        let turn = TurnManifestEntry {
            provider: SourceKind::Codex,
            session_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            turn_ordinal: 1,
            started_at: "2026-05-15T00:00:00Z".to_owned(),
            payload_hash: "bad-hash".to_owned(),
            object_path: format!("{ARTIFACT_ROOT}/objects/../bad.age"),
            chunk_id: None,
            chunk_record_index: None,
        };
        let sync = write_test_sync_object(
            &cache,
            &identity,
            &signing_key,
            &exporter,
            "git:https://example.invalid/team/repo",
            vec![sync_entry_from_manifest(&turn)],
        );
        write_json_file(
            &cache.join(ARTIFACT_ROOT).join("manifest.json"),
            &ManifestArtifact {
                schema: MANIFEST_SCHEMA.to_owned(),
                version: 1,
                project_key: "git:https://example.invalid/team/repo".to_owned(),
                branch: "team".to_owned(),
                exported_at: "2026-05-15T00:00:00Z".to_owned(),
                exporter,
                sync,
                chunks: Vec::new(),
                turns: vec![turn],
            },
        )
        .unwrap();
        let context = ShareProjectContext {
            root: root.clone(),
            index_db_path: root.join("index.sqlite"),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };

        let report = import_from_cache(
            &context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.skipped_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("direct object file"),
            "warning should explain path validation failure: {:?}",
            report.warnings
        );
    }

    #[test]
    fn merge_skips_nested_manifest_object_paths_with_warning() {
        let TestShareArtifact {
            cache,
            target_context,
            target_identity,
            source_identity,
            source_signing_key,
            artifact,
        } = build_single_turn_test_artifact("share-nested-object-path");
        write_export_artifact(&cache, &artifact).unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let old_object_path = manifest.turns[0].object_path.clone();
        let nested_object_path = format!("{ARTIFACT_ROOT}/objects/nested/copied.age");
        let nested_target = cache.join(&nested_object_path);
        fs::create_dir_all(nested_target.parent().unwrap()).unwrap();
        fs::copy(cache.join(old_object_path), &nested_target).unwrap();
        manifest.turns[0].object_path = nested_object_path;
        manifest.sync = write_test_sync_object_with_chunks(
            &cache,
            &target_identity,
            &source_signing_key,
            &source_identity,
            "git:https://example.invalid/team/repo",
            sync_chunks_from_manifest(&manifest),
            vec![sync_entry_from_manifest(&manifest.turns[0])],
        );
        write_json_file(&manifest_path, &manifest).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.skipped_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("object path"),
            "warning should reject nested object path: {:?}",
            report.warnings
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_skips_symlinked_visible_manifests() {
        use std::os::unix::fs::symlink;

        let root = unique_test_dir("share-symlink-artifacts");
        let cache = root.join("cache");
        let key = ensure_share_key(&root).unwrap();
        let identity = read_share_identity_key(&key.key_path).unwrap();
        let signing_key = test_signing_key(&identity);
        let exporter = test_share_identity(&identity);
        let turn = TurnManifestEntry {
            provider: SourceKind::Codex,
            session_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            turn_ordinal: 1,
            started_at: "2026-05-15T00:00:00Z".to_owned(),
            payload_hash: "bad-hash".to_owned(),
            object_path: format!("{ARTIFACT_ROOT}/objects/bad.age"),
            chunk_id: None,
            chunk_record_index: None,
        };
        let sync = write_test_sync_object(
            &cache,
            &identity,
            &signing_key,
            &exporter,
            "git:https://example.invalid/team/repo",
            vec![sync_entry_from_manifest(&turn)],
        );
        let target = cache.join("target.json");
        write_json_file(
            &target,
            &ManifestArtifact {
                schema: MANIFEST_SCHEMA.to_owned(),
                version: 1,
                project_key: "git:https://example.invalid/team/repo".to_owned(),
                branch: "team".to_owned(),
                exported_at: "2026-05-15T00:00:00Z".to_owned(),
                exporter,
                sync,
                chunks: Vec::new(),
                turns: vec![turn],
            },
        )
        .unwrap();
        let manifest_path = cache.join(ARTIFACT_ROOT).join("manifest.json");
        fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        symlink(&target, &manifest_path).unwrap();
        let context = ShareProjectContext {
            root: root.clone(),
            index_db_path: root.join("index.sqlite"),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };

        let report = import_from_cache(
            &context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("symlink"),
            "warning should reject symlinked manifest: {:?}",
            report.warnings
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_skips_symlinked_manifest_ancestors() {
        use std::os::unix::fs::symlink;

        let TestShareArtifact {
            cache,
            target_context,
            artifact,
            ..
        } = build_single_turn_test_artifact("share-symlink-manifest-ancestor");
        write_export_artifact(&cache, &artifact).unwrap();
        let outside = cache
            .parent()
            .expect("cache should have parent")
            .join("outside-artifacts");
        fs::remove_dir_all(cache.join("darc-share")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join(LEGACY_MANIFEST_FILE), b"{}").unwrap();
        symlink(&outside, cache.join("darc-share")).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("ancestor is a symlink"),
            "warning should reject symlinked manifest ancestor: {:?}",
            report.warnings
        );
        assert!(outside.join(LEGACY_MANIFEST_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn push_writes_reject_symlinked_artifact_parents() {
        use std::os::unix::fs::symlink;

        let workspace = unique_test_dir("share-symlink-write");
        let cache = workspace.join("cache");
        let outside = workspace.join("outside");
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, cache.join("darc-share")).unwrap();
        let index_db_path = workspace.join("index.sqlite");
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: index_db_path.clone(),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: workspace.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &index_db_path,
            "repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&context, SharePolicy::All).unwrap();
        let connection = open_index_database_writer(&index_db_path).unwrap();
        let turns = query_share_export_turns(&connection, "repo").unwrap();
        let age_identity = Identity::generate();
        let signing_key = test_signing_key(&age_identity);
        let identity = test_share_identity(&age_identity);
        let artifact = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            signing_key: &signing_key,
            branch: "team",
            turns,
        })
        .unwrap();

        let error = write_export_artifact(&cache, &artifact).unwrap_err();

        assert!(
            error.to_string().contains("symlink"),
            "error should reject symlinked artifact parent: {error:#}"
        );
        assert!(!outside.join("v1").join(PROJECT_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn cache_cleanup_rejects_symlinked_cache_root() {
        use std::os::unix::fs::symlink;

        let workspace = unique_test_dir("share-cache-root-symlink");
        let cache_parent = workspace.join(SHARE_CACHE_DIR);
        let cache = cache_parent.join("cache-repo");
        let outside = workspace.join("outside");
        fs::create_dir_all(&cache_parent).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        symlink(&outside, &cache).unwrap();
        let identity = test_share_identity(&Identity::generate());

        let prepare_error = match prepare_cache_repository(
            &cache,
            "https://example.invalid/share.git",
            &workspace,
            &identity,
        ) {
            Ok(_) => panic!("prepare should reject symlinked cache root"),
            Err(error) => error,
        };
        let cleanup_error = clear_cache_worktree(&cache).unwrap_err();

        assert!(
            prepare_error.to_string().contains("symlink"),
            "prepare should reject symlinked cache root: {prepare_error:#}"
        );
        assert!(
            cleanup_error.to_string().contains("symlink"),
            "cleanup should reject symlinked cache root: {cleanup_error:#}"
        );
        assert!(outside.join("keep.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn merge_rejects_symlinked_cache_root() {
        use std::os::unix::fs::symlink;

        let workspace = unique_test_dir("share-merge-cache-root-symlink");
        let target_root = workspace.join("target-root");
        let outside = workspace.join("outside");
        let cache = workspace.join(SHARE_CACHE_DIR).join("cache-repo");
        fs::create_dir_all(&target_root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        symlink(&outside, &cache).unwrap();
        let context = ShareProjectContext {
            root: target_root,
            index_db_path: workspace.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: workspace.join("target-repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };

        let error = import_from_cache(
            &context,
            "team",
            "darc/team",
            "share",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("symlink"),
            "merge should reject symlinked cache root: {error:#}"
        );
        assert!(outside.join("keep.txt").exists());
    }

    #[test]
    fn lfs_hydration_skips_missing_cache_root() {
        let root = unique_test_dir("share-missing-lfs-cache-root");
        hydrate_lfs_objects(&root.join("missing-cache"), &BTreeSet::new()).unwrap();
    }

    #[test]
    fn lfs_pointer_objects_require_hydration_before_share_writes() {
        let root = unique_test_dir("share-lfs-pointer-detection");
        let object_path = root.join(ARTIFACT_ROOT).join("objects").join("pointer.age");
        fs::create_dir_all(object_path.parent().unwrap()).unwrap();
        fs::write(
            &object_path,
            b"version https://git-lfs.github.com/spec/v1\noid sha256:0000\nsize 123\n",
        )
        .unwrap();
        let object_paths = BTreeSet::from([format!("{ARTIFACT_ROOT}/objects/pointer.age")]);

        let error = reject_lfs_pointer_objects(&root, &object_paths).unwrap_err();

        assert!(
            error.to_string().contains("Git LFS pointer"),
            "error should require hydrated LFS objects: {error:#}"
        );
    }

    #[test]
    fn lfs_pointer_rejection_ignores_unreferenced_objects() {
        let root = unique_test_dir("share-lfs-pointer-ignored");
        let object_path = root.join(ARTIFACT_ROOT).join("objects").join("orphan.age");
        fs::create_dir_all(object_path.parent().unwrap()).unwrap();
        fs::write(
            &object_path,
            b"version https://git-lfs.github.com/spec/v1\noid sha256:0000\nsize 123\n",
        )
        .unwrap();
        let object_paths = BTreeSet::from([format!("{ARTIFACT_ROOT}/objects/referenced.age")]);

        reject_lfs_pointer_objects(&root, &object_paths).unwrap();
    }

    #[test]
    fn push_and_pull_round_trip_rebinds_refreshes_and_prunes_sessions() {
        let workspace = unique_test_dir("share-round-trip");
        let remote_path = workspace.join("share.git");
        init_bare_remote(&remote_path);
        let source_root = workspace.join("source-root");
        let source_repo = workspace.join("source-repo");
        let target_root = workspace.join("target-root");
        let target_repo = workspace.join("target-repo");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        init_test_git_repo(&source_repo);
        init_test_git_repo(&target_repo);
        let target_key = ensure_share_key(&target_root).unwrap();
        let remote_url = remote_path.to_string_lossy().into_owned();
        let project_upstream = "https://example.invalid/team/repo.git".to_owned();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_repo,
            git_upstream: Some(project_upstream.clone()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_repo,
            git_upstream: Some(project_upstream),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let settings = ShareSettings {
            remotes: vec![ShareRemote {
                name: "share".to_owned(),
                url: remote_url,
            }],
            recipients: vec![ShareRecipient {
                recipient: target_key.public_key,
            }],
        };

        let push = push_share_branch(&source_context, &settings, "team", Some("share")).unwrap();
        assert_eq!(push.exported_session_count, 1);
        assert_eq!(push.exported_turn_count, 1);

        let pull = pull_share_branch(&target_context, &settings, "team", Some("share")).unwrap();
        assert_eq!(pull.merge.imported_turn_count, 1);
        assert_eq!(pull.merge.skipped_turn_count, 0);
        let target = open_index_database_writer(&target_context.index_db_path).unwrap();
        let imported: (String, String) = target
            .query_row(
                "
                SELECT project_id, origin_kind
                FROM sessions
                WHERE project_id = 'target-repo'
                    AND session_id = '00000000-0000-4000-8000-000000000303'
                ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(imported, ("target-repo".to_owned(), "shared".to_owned()));

        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000304",
        );
        let second_push =
            push_share_branch(&source_context, &settings, "team", Some("share")).unwrap();
        assert_eq!(second_push.exported_session_count, 2);
        let second_pull =
            pull_share_branch(&target_context, &settings, "team", Some("share")).unwrap();
        assert_eq!(second_pull.merge.warning_count, 0);
        let target_session_count: i64 = target
            .query_row(
                "
                SELECT COUNT(*)
                FROM sessions
                WHERE project_id = 'target-repo'
                    AND origin_kind = 'shared'
                ",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(target_session_count, 2);

        let old_source_identity = local_share_identity(&source_context).unwrap();
        fs::remove_file(source_root.join("keys").join(KEY_FILE_NAME)).unwrap();
        let rotated_source_key = ensure_share_key(&source_root).unwrap();
        let rotated_source_identity = local_share_identity(&source_context).unwrap();
        assert_ne!(
            old_source_identity.public_key,
            rotated_source_key.public_key
        );
        assert_eq!(old_source_identity.user_id, rotated_source_identity.user_id);

        exclude_all_sessions(&source_context).unwrap();
        let empty_push =
            push_share_branch(&source_context, &settings, "team", Some("share")).unwrap();
        assert_eq!(empty_push.exported_turn_count, 0);
        let empty_pull =
            pull_share_branch(&target_context, &settings, "team", Some("share")).unwrap();
        assert_eq!(empty_pull.merge.warning_count, 0);
        let target_session_count: i64 = target
            .query_row(
                "
                SELECT COUNT(*)
                FROM sessions
                WHERE project_id = 'target-repo'
                    AND origin_kind = 'shared'
                ",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(target_session_count, 0);
        let object_paths = remote_tip_blob_paths(&remote_path, "darc/team")
            .into_iter()
            .filter(|path| path.starts_with(&format!("{ARTIFACT_ROOT}/objects/")))
            .collect::<Vec<_>>();
        assert_eq!(object_paths.len(), 1);
        assert!(object_paths[0].contains("/sync-"));
    }

    #[test]
    fn push_share_branch_with_progress_emits_producer_events() {
        let workspace = unique_test_dir("share-push-progress-events");
        let remote_path = workspace.join("share.git");
        init_bare_remote(&remote_path);
        let source_root = workspace.join("source-root");
        let source_repo = workspace.join("source-repo");
        let target_root = workspace.join("target-root");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        init_test_git_repo(&source_repo);
        let target_key = ensure_share_key(&target_root).unwrap();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_repo,
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000808",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let settings = ShareSettings {
            remotes: vec![ShareRemote {
                name: "share".to_owned(),
                url: remote_path.to_string_lossy().into_owned(),
            }],
            recipients: vec![ShareRecipient {
                recipient: target_key.public_key,
            }],
        };
        let mut events = Vec::new();

        let report = push_share_branch_with_progress(
            &source_context,
            &settings,
            "team",
            Some("share"),
            |event| events.push(event),
        )
        .unwrap();

        assert!(matches!(
            events.first(),
            Some(SharePushProgress::Started { .. })
        ));
        assert!(events.iter().any(|event| {
            matches!(event, SharePushProgress::BuildingExport { total_turns: 1 })
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                SharePushProgress::ExportingSessions {
                    exported_sessions: 1,
                    total_sessions: 1
                }
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                SharePushProgress::ExportingTurns {
                    exported_turns: 1,
                    total_turns: 1
                }
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                SharePushProgress::Uploading {
                    kind: ShareUploadKind::Git
                }
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(event, SharePushProgress::Finished { commit_id } if commit_id == &report.commit_id)
        }));
    }

    #[test]
    fn export_session_progress_counts_completed_sessions_without_grouped_turns() {
        let workspace = unique_test_dir("share-export-progress-ungrouped");
        let index_db_path = workspace.join("index.sqlite");
        let session_a = "00000000-0000-4000-8000-000000000901";
        let session_b = "00000000-0000-4000-8000-000000000902";
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: index_db_path.clone(),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: workspace.join("source-repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(&index_db_path, "source-repo", session_a);
        seed_share_export_session(&index_db_path, "source-repo", session_b);
        let connection = open_index_database_writer(&index_db_path).unwrap();
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture {
                user_message: "synthetic second prompt",
                final_answer_text: Some("synthetic second answer"),
                has_final_answer: true,
                ..IndexedTurnFixture::new(
                    "source-repo",
                    SourceKind::Codex,
                    session_a,
                    2,
                    "2026-05-15T12:01:00Z",
                    "completed",
                    "[]",
                )
            },
        )
        .unwrap();
        update_share_policy(&context, SharePolicy::All).unwrap();
        let turns = query_share_export_turns(&connection, "source-repo").unwrap();
        let turn_a1 = turns
            .iter()
            .find(|turn| turn.session.session_id == session_a && turn.turn_ordinal == 1)
            .unwrap()
            .clone();
        let turn_a2 = turns
            .iter()
            .find(|turn| turn.session.session_id == session_a && turn.turn_ordinal == 2)
            .unwrap()
            .clone();
        let turn_b1 = turns
            .iter()
            .find(|turn| turn.session.session_id == session_b && turn.turn_ordinal == 1)
            .unwrap()
            .clone();
        let age_identity = Identity::generate();
        let signing_key = test_signing_key(&age_identity);
        let identity = test_share_identity(&age_identity);
        let mut events = Vec::new();

        let artifact = build_export_artifact_with_progress(
            ExportBuildRequest {
                context: &context,
                settings: &ShareSettings::default(),
                project_key: "git:https://example.invalid/team/repo",
                identity: &identity,
                signing_key: &signing_key,
                branch: "team",
                turns: vec![turn_a1, turn_b1, turn_a2],
            },
            &mut |event| events.push(event),
        )
        .unwrap();

        assert_eq!(artifact.exported_session_count, 2);
        let session_events = events
            .iter()
            .filter_map(|event| match event {
                SharePushProgress::ExportingSessions {
                    exported_sessions,
                    total_sessions,
                } => Some((*exported_sessions, *total_sessions)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            session_events,
            vec![(0_u64, 2_u64), (1_u64, 2_u64), (2_u64, 2_u64)]
        );
    }

    #[test]
    fn pull_share_branch_with_progress_emits_session_events() {
        let workspace = unique_test_dir("share-pull-progress-events");
        let remote_path = workspace.join("share.git");
        init_bare_remote(&remote_path);
        let source_root = workspace.join("source-root");
        let source_repo = workspace.join("source-repo");
        let target_root = workspace.join("target-root");
        let target_repo = workspace.join("target-repo");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        init_test_git_repo(&source_repo);
        init_test_git_repo(&target_repo);
        let target_key = ensure_share_key(&target_root).unwrap();
        let project_upstream = "https://example.invalid/team/repo.git".to_owned();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_repo,
            git_upstream: Some(project_upstream.clone()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_repo,
            git_upstream: Some(project_upstream),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000909",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let settings = ShareSettings {
            remotes: vec![ShareRemote {
                name: "share".to_owned(),
                url: remote_path.to_string_lossy().into_owned(),
            }],
            recipients: vec![ShareRecipient {
                recipient: target_key.public_key,
            }],
        };
        push_share_branch(&source_context, &settings, "team", Some("share")).unwrap();
        let mut events = Vec::new();

        let report = pull_share_branch_with_progress(
            &target_context,
            &settings,
            "team",
            Some("share"),
            |event| events.push(event),
        )
        .unwrap();

        assert!(matches!(
            events.first(),
            Some(SharePullProgress::Started { .. })
        ));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                SharePullProgress::ImportingSessions {
                    processed_sessions: 0,
                    total_sessions: 1
                }
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                SharePullProgress::ImportingSessions {
                    processed_sessions: 1,
                    total_sessions: 1
                }
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                SharePullProgress::Finished {
                    imported_turn_count: 1,
                    skipped_turn_count: 0,
                    warning_count: 0
                }
            )
        }));
        assert_eq!(report.merge.imported_turn_count, 1);
    }

    #[test]
    fn fetch_cleans_untracked_cache_artifacts_before_merge() {
        let workspace = unique_test_dir("share-fetch-cleans-untracked");
        let remote_path = workspace.join("share.git");
        init_bare_remote(&remote_path);
        let source_root = workspace.join("source-root");
        let source_repo = workspace.join("source-repo");
        let target_root = workspace.join("target-root");
        let target_repo = workspace.join("target-repo");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        init_test_git_repo(&source_repo);
        init_test_git_repo(&target_repo);
        let target_key = ensure_share_key(&target_root).unwrap();
        let remote_url = remote_path.to_string_lossy().into_owned();
        let project_upstream = "https://example.invalid/team/repo.git".to_owned();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_repo,
            git_upstream: Some(project_upstream.clone()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_repo,
            git_upstream: Some(project_upstream),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let settings = ShareSettings {
            remotes: vec![ShareRemote {
                name: "share".to_owned(),
                url: remote_url.clone(),
            }],
            recipients: vec![ShareRecipient {
                recipient: target_key.public_key,
            }],
        };

        push_share_branch(&source_context, &settings, "team", Some("share")).unwrap();
        fetch_share_branch(&target_context, &settings, "team", Some("share")).unwrap();
        let cache = cache_repo_path(&target_root, &remote_url, "darc/team");
        let outside_worktree = workspace.join("outside-worktree");
        fs::create_dir_all(&outside_worktree).unwrap();
        fs::write(outside_worktree.join("keep.txt"), b"keep").unwrap();
        run_cache_git(
            &cache,
            [
                "config",
                "core.worktree",
                outside_worktree.to_str().unwrap(),
            ],
            "failed to poison synthetic cache worktree",
        )
        .unwrap();
        let injected_manifest = cache
            .join(ARTIFACT_ROOT)
            .join(EXPORTERS_DIR)
            .join("injected")
            .join(LEGACY_MANIFEST_FILE);
        fs::create_dir_all(injected_manifest.parent().unwrap()).unwrap();
        fs::write(&injected_manifest, b"not json").unwrap();
        let injected_object = cache
            .join(ARTIFACT_ROOT)
            .join("objects")
            .join("injected.age");
        fs::write(&injected_object, b"not an age payload").unwrap();
        let nested_git = cache
            .join(ARTIFACT_ROOT)
            .join(EXPORTERS_DIR)
            .join("nested-git")
            .join(".git");
        fs::create_dir_all(&nested_git).unwrap();
        fs::write(
            nested_git.parent().unwrap().join(LEGACY_MANIFEST_FILE),
            b"not json",
        )
        .unwrap();
        assert!(injected_manifest.exists());
        assert!(injected_object.exists());
        assert!(nested_git.exists());

        fetch_share_branch(&target_context, &settings, "team", Some("share")).unwrap();

        assert!(!injected_manifest.exists());
        assert!(!injected_object.exists());
        assert!(!nested_git.exists());
        assert!(outside_worktree.join("keep.txt").exists());
        let merge = merge_share_branch(&target_context, &settings, "team", Some("share")).unwrap();
        assert_eq!(merge.imported_turn_count, 1);
        assert_eq!(merge.warning_count, 0);
    }

    #[test]
    fn fetch_and_merge_prune_tracked_non_artifacts_before_lfs_hydration() {
        let workspace = unique_test_dir("share-prunes-lfs-config");
        let remote_path = workspace.join("share.git");
        init_bare_remote(&remote_path);
        let source_root = workspace.join("source-root");
        let source_repo = workspace.join("source-repo");
        let target_root = workspace.join("target-root");
        let target_repo = workspace.join("target-repo");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        init_test_git_repo(&source_repo);
        init_test_git_repo(&target_repo);
        let target_key = ensure_share_key(&target_root).unwrap();
        let remote_url = remote_path.to_string_lossy().into_owned();
        let project_upstream = "https://example.invalid/team/repo.git".to_owned();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_repo,
            git_upstream: Some(project_upstream.clone()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_repo,
            git_upstream: Some(project_upstream),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let settings = ShareSettings {
            remotes: vec![ShareRemote {
                name: "share".to_owned(),
                url: remote_url.clone(),
            }],
            recipients: vec![ShareRecipient {
                recipient: target_key.public_key,
            }],
        };

        push_share_branch(&source_context, &settings, "team", Some("share")).unwrap();
        let source_cache = cache_repo_path(&source_root, &remote_url, "darc/team");
        fs::write(
            source_cache.join(".lfsconfig"),
            "[lfs]\nurl = https://example.invalid/lfs\n",
        )
        .unwrap();
        let orphan_object = source_cache
            .join(ARTIFACT_ROOT)
            .join("objects")
            .join("orphan.age");
        fs::write(
            &orphan_object,
            b"version https://git-lfs.github.com/spec/v1\noid sha256:0000\nsize 123\n",
        )
        .unwrap();
        run_cache_git_with_lfs_filter_override(
            &source_cache,
            ["add", ".lfsconfig", orphan_object.to_str().unwrap()],
            "failed to stage synthetic LFS files",
            true,
        )
        .unwrap();
        run_cache_git(
            &source_cache,
            ["commit", "--no-gpg-sign", "-m", "test: add lfs config"],
            "failed to commit synthetic LFS config",
        )
        .unwrap();
        run_cache_git(
            &source_cache,
            [
                "push",
                DEFAULT_REMOTE_NAME,
                "refs/heads/darc/team:refs/heads/darc/team",
            ],
            "failed to push synthetic LFS config",
        )
        .unwrap();

        fetch_share_branch(&target_context, &settings, "team", Some("share")).unwrap();
        let target_cache = cache_repo_path(&target_root, &remote_url, "darc/team");
        assert!(!target_cache.join(".lfsconfig").exists());
        assert!(
            target_cache
                .join(ARTIFACT_ROOT)
                .join("objects")
                .join("orphan.age")
                .exists()
        );
        assert!(target_cache.join(ARTIFACT_ROOT).join(PROJECT_FILE).exists());

        let merge = merge_share_branch(&target_context, &settings, "team", Some("share")).unwrap();

        assert!(!target_cache.join(".lfsconfig").exists());
        assert_eq!(merge.imported_turn_count, 1);
        assert_eq!(merge.warning_count, 0);
    }

    #[test]
    fn merge_resets_tracked_cache_modifications_before_import() {
        let workspace = unique_test_dir("share-merge-resets-tracked");
        let remote_path = workspace.join("share.git");
        init_bare_remote(&remote_path);
        let source_root = workspace.join("source-root");
        let source_repo = workspace.join("source-repo");
        let target_root = workspace.join("target-root");
        let target_repo = workspace.join("target-repo");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        init_test_git_repo(&source_repo);
        init_test_git_repo(&target_repo);
        let target_key = ensure_share_key(&target_root).unwrap();
        let remote_url = remote_path.to_string_lossy().into_owned();
        let project_upstream = "https://example.invalid/team/repo.git".to_owned();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_repo,
            git_upstream: Some(project_upstream.clone()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_repo,
            git_upstream: Some(project_upstream),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let settings = ShareSettings {
            remotes: vec![ShareRemote {
                name: "share".to_owned(),
                url: remote_url.clone(),
            }],
            recipients: vec![ShareRecipient {
                recipient: target_key.public_key,
            }],
        };

        push_share_branch(&source_context, &settings, "team", Some("share")).unwrap();
        fetch_share_branch(&target_context, &settings, "team", Some("share")).unwrap();
        let cache = cache_repo_path(&target_root, &remote_url, "darc/team");
        fs::write(first_exporter_manifest_path(&cache), b"not json").unwrap();
        fs::write(
            cache.join("darc-share").join("v1").join("project.json"),
            b"not json",
        )
        .unwrap();

        let merge = merge_share_branch(&target_context, &settings, "team", Some("share")).unwrap();

        assert_eq!(merge.imported_turn_count, 1);
        assert_eq!(merge.warning_count, 0);
    }

    #[test]
    fn push_drops_unexpected_cache_files_from_branch_tip() {
        let workspace = unique_test_dir("share-artifact-only-tip");
        let remote_path = workspace.join("share.git");
        init_bare_remote(&remote_path);
        let source_root = workspace.join("source-root");
        let source_repo = workspace.join("source-repo");
        fs::create_dir_all(&source_root).unwrap();
        init_test_git_repo(&source_repo);
        let remote_url = remote_path.to_string_lossy().into_owned();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_repo,
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let settings = ShareSettings {
            remotes: vec![ShareRemote {
                name: "share".to_owned(),
                url: remote_url.clone(),
            }],
            recipients: Vec::new(),
        };
        push_share_branch(&source_context, &settings, "team", Some("share")).unwrap();
        let cache = cache_repo_path(&source_root, &remote_url, "darc/team");
        fs::write(cache.join(".git").join("info").join("exclude"), "*.age\n").unwrap();
        fs::write(cache.join("plaintext.txt"), b"do not publish").unwrap();
        let rogue_object = cache
            .join(ARTIFACT_ROOT)
            .join("objects")
            .join("plaintext.txt");
        fs::write(&rogue_object, b"do not publish").unwrap();
        let rogue_nested = cache
            .join(ARTIFACT_ROOT)
            .join("unexpected")
            .join("file.txt");
        fs::create_dir_all(rogue_nested.parent().unwrap()).unwrap();
        fs::write(&rogue_nested, b"do not publish").unwrap();
        let rogue_age_object = cache.join(ARTIFACT_ROOT).join("objects").join("orphan.age");
        fs::write(&rogue_age_object, b"do not publish").unwrap();
        let rogue_manifest = cache
            .join(ARTIFACT_ROOT)
            .join(EXPORTERS_DIR)
            .join("orphan")
            .join(LEGACY_MANIFEST_FILE);
        fs::create_dir_all(rogue_manifest.parent().unwrap()).unwrap();
        fs::write(&rogue_manifest, b"not json").unwrap();

        push_share_branch(&source_context, &settings, "team", Some("share")).unwrap();

        let paths = remote_tip_blob_paths(&remote_path, "darc/team");
        assert!(
            paths
                .iter()
                .all(|path| allowed_share_cache_file(Path::new(path))),
            "remote branch should contain only Darc share artifacts: {paths:?}"
        );
        assert!(!paths.iter().any(|path| path.contains("plaintext")));
        assert!(!paths.iter().any(|path| path.contains("unexpected")));
        assert!(!paths.iter().any(|path| path.contains("orphan")));
        assert!(paths.iter().any(|path| path.ends_with(".age")));
    }

    #[test]
    fn push_preserves_same_email_exporters_at_branch_tip() {
        let workspace = unique_test_dir("share-same-email-author-tip");
        let remote_path = workspace.join("share.git");
        init_bare_remote(&remote_path);
        let first_root = workspace.join("first-root");
        let first_repo = workspace.join("first-repo");
        let second_root = workspace.join("second-root");
        let second_repo = workspace.join("second-repo");
        let target_root = workspace.join("target-root");
        let target_repo = workspace.join("target-repo");
        fs::create_dir_all(&first_root).unwrap();
        fs::create_dir_all(&second_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        init_test_git_repo(&first_repo);
        init_test_git_repo(&second_repo);
        init_test_git_repo(&target_repo);
        let first_key = ensure_share_key(&first_root).unwrap();
        let second_key = ensure_share_key(&second_root).unwrap();
        let target_key = ensure_share_key(&target_root).unwrap();
        let remote_url = remote_path.to_string_lossy().into_owned();
        let project_upstream = "https://example.invalid/team/repo.git".to_owned();
        let first_context = ShareProjectContext {
            root: first_root.clone(),
            index_db_path: first_root.join("index.sqlite"),
            project_id: "first-repo".to_owned(),
            project_name: "first-repo".to_owned(),
            local_path: first_repo,
            git_upstream: Some(project_upstream.clone()),
        };
        let second_context = ShareProjectContext {
            root: second_root.clone(),
            index_db_path: second_root.join("index.sqlite"),
            project_id: "second-repo".to_owned(),
            project_name: "second-repo".to_owned(),
            local_path: second_repo,
            git_upstream: Some(project_upstream.clone()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_repo,
            git_upstream: Some(project_upstream),
        };
        seed_share_export_session(
            &first_context.index_db_path,
            "first-repo",
            "00000000-0000-4000-8000-000000000301",
        );
        seed_share_export_session(
            &second_context.index_db_path,
            "second-repo",
            "00000000-0000-4000-8000-000000000302",
        );
        update_share_policy(&first_context, SharePolicy::All).unwrap();
        update_share_policy(&second_context, SharePolicy::All).unwrap();
        let settings = ShareSettings {
            remotes: vec![ShareRemote {
                name: "share".to_owned(),
                url: remote_url,
            }],
            recipients: vec![
                ShareRecipient {
                    recipient: first_key.public_key,
                },
                ShareRecipient {
                    recipient: second_key.public_key,
                },
                ShareRecipient {
                    recipient: target_key.public_key,
                },
            ],
        };

        push_share_branch(&first_context, &settings, "team", Some("share")).unwrap();
        push_share_branch(&second_context, &settings, "team", Some("share")).unwrap();
        let pull = pull_share_branch(&target_context, &settings, "team", Some("share")).unwrap();
        let manifest_paths = remote_tip_blob_paths(&remote_path, "darc/team")
            .into_iter()
            .filter(|path| {
                path.starts_with(&format!("{ARTIFACT_ROOT}/{EXPORTERS_DIR}/"))
                    && path.ends_with(LEGACY_MANIFEST_FILE)
            })
            .collect::<Vec<_>>();
        let target = open_index_database_writer(&target_context.index_db_path).unwrap();
        let imported_sessions: i64 = target
            .query_row(
                "
                SELECT COUNT(*)
                FROM sessions
                WHERE project_id = 'target-repo'
                    AND origin_kind = 'shared'
                ",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(manifest_paths.len(), 2);
        assert_eq!(pull.merge.imported_turn_count, 2);
        assert_eq!(pull.merge.warning_count, 0);
        assert_eq!(imported_sessions, 2);
    }

    #[test]
    fn push_fails_closed_when_retained_manifest_cannot_decrypt() {
        let workspace = unique_test_dir("share-retained-manifest-fails-closed");
        let remote_path = workspace.join("share.git");
        init_bare_remote(&remote_path);
        let first_root = workspace.join("first-root");
        let first_repo = workspace.join("first-repo");
        let second_root = workspace.join("second-root");
        let second_repo = workspace.join("second-repo");
        fs::create_dir_all(&first_root).unwrap();
        fs::create_dir_all(&second_root).unwrap();
        init_test_git_repo(&first_repo);
        init_test_git_repo(&second_repo);
        let remote_url = remote_path.to_string_lossy().into_owned();
        let project_upstream = "https://example.invalid/team/repo.git".to_owned();
        let first_context = ShareProjectContext {
            root: first_root,
            index_db_path: workspace.join("first-index.sqlite"),
            project_id: "first-repo".to_owned(),
            project_name: "first-repo".to_owned(),
            local_path: first_repo,
            git_upstream: Some(project_upstream.clone()),
        };
        let second_context = ShareProjectContext {
            root: second_root,
            index_db_path: workspace.join("second-index.sqlite"),
            project_id: "second-repo".to_owned(),
            project_name: "second-repo".to_owned(),
            local_path: second_repo,
            git_upstream: Some(project_upstream),
        };
        seed_share_export_session(
            &first_context.index_db_path,
            "first-repo",
            "00000000-0000-4000-8000-000000000301",
        );
        seed_share_export_session(
            &second_context.index_db_path,
            "second-repo",
            "00000000-0000-4000-8000-000000000302",
        );
        update_share_policy(&first_context, SharePolicy::All).unwrap();
        update_share_policy(&second_context, SharePolicy::All).unwrap();
        let settings = ShareSettings {
            remotes: vec![ShareRemote {
                name: "share".to_owned(),
                url: remote_url,
            }],
            recipients: Vec::new(),
        };

        push_share_branch(&first_context, &settings, "team", Some("share")).unwrap();
        let error = push_share_branch(&second_context, &settings, "team", Some("share"))
            .expect_err("second exporter must not prune an unreadable first manifest");
        let manifest_paths = remote_tip_blob_paths(&remote_path, "darc/team")
            .into_iter()
            .filter(|path| {
                path.starts_with(&format!("{ARTIFACT_ROOT}/{EXPORTERS_DIR}/"))
                    && path.ends_with(LEGACY_MANIFEST_FILE)
            })
            .collect::<Vec<_>>();

        assert!(
            error
                .to_string()
                .contains("failed to authenticate retained share manifest"),
            "push should fail closed on unreadable retained manifests: {error:#}"
        );
        assert_eq!(manifest_paths.len(), 1);
    }

    #[test]
    fn include_all_clears_previous_session_exclusions() {
        let workspace = unique_test_dir("share-include-all-clears-exclusions");
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: workspace.join("index.sqlite"),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: workspace.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let session_id = "00000000-0000-4000-8000-000000000303";
        seed_share_export_session(&context.index_db_path, "repo", session_id);
        update_session_share_state(
            &context,
            SourceKind::Codex,
            session_id,
            ShareState::Excluded,
        )
        .unwrap();

        include_all_sessions(&context).unwrap();

        let connection = open_index_database_writer(&context.index_db_path).unwrap();
        let status = query_share_status(&connection, "repo").unwrap();
        let turns = query_share_export_turns(&connection, "repo").unwrap();
        assert_eq!(status.excluded_session_count, 0);
        assert_eq!(status.selected_session_count, 1);
        assert_eq!(turns.len(), 1);
    }

    #[test]
    fn exclude_all_clears_previous_session_inclusions() {
        let workspace = unique_test_dir("share-exclude-all-clears-inclusions");
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: workspace.join("index.sqlite"),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: workspace.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let session_id = "00000000-0000-4000-8000-000000000303";
        seed_share_export_session(&context.index_db_path, "repo", session_id);
        update_session_share_state(
            &context,
            SourceKind::Codex,
            session_id,
            ShareState::Included,
        )
        .unwrap();

        exclude_all_sessions(&context).unwrap();

        let connection = open_index_database_writer(&context.index_db_path).unwrap();
        let status = query_share_status(&connection, "repo").unwrap();
        let turns = query_share_export_turns(&connection, "repo").unwrap();
        assert_eq!(status.included_session_count, 0);
        assert_eq!(status.selected_session_count, 0);
        assert!(turns.is_empty());
    }

    #[test]
    fn missing_remote_branch_drops_stale_cache_parent() {
        let workspace = unique_test_dir("share-missing-remote-branch");
        let remote_path = workspace.join("share.git");
        init_bare_remote(&remote_path);
        let source_root = workspace.join("source-root");
        let source_repo = workspace.join("source-repo");
        fs::create_dir_all(&source_root).unwrap();
        init_test_git_repo(&source_repo);
        let remote_url = remote_path.to_string_lossy().into_owned();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_repo,
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();

        let settings = ShareSettings {
            remotes: vec![ShareRemote {
                name: "share".to_owned(),
                url: remote_url,
            }],
            recipients: Vec::new(),
        };

        push_share_branch(&source_context, &settings, "team", Some("share")).unwrap();
        delete_remote_branch(&remote_path, "darc/team");
        let stale_manifest = cache_repo_path(&source_root, &settings.remotes[0].url, "darc/team")
            .join(ARTIFACT_ROOT)
            .join(EXPORTERS_DIR)
            .join("stale")
            .join(LEGACY_MANIFEST_FILE);
        fs::create_dir_all(stale_manifest.parent().unwrap()).unwrap();
        fs::write(&stale_manifest, b"stale manifest").unwrap();
        push_share_branch(&source_context, &settings, "team", Some("share")).unwrap();

        assert_eq!(remote_tip_parent_count(&remote_path, "darc/team"), 0);
        assert!(
            !remote_tip_blob_paths(&remote_path, "darc/team")
                .iter()
                .any(|path| path.contains("/stale/")),
            "recreated branch should not include stale local cache files"
        );
    }

    #[test]
    fn merge_skips_mismatched_sync_exporter_before_pruning() {
        let workspace = unique_test_dir("share-authenticated-prune");
        let cache = workspace.join("cache");
        let source_root = workspace.join("source-root");
        let target_root = workspace.join("target-root");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        let target_key = ensure_share_key(&target_root).unwrap();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let source_age_identity = Identity::generate();
        let source_signing_key = test_signing_key(&source_age_identity);
        let source_identity = test_share_identity(&source_age_identity);
        let settings = ShareSettings {
            remotes: Vec::new(),
            recipients: vec![ShareRecipient {
                recipient: target_key.public_key,
            }],
        };
        let source = open_index_database_writer(&source_context.index_db_path).unwrap();
        let turns = query_share_export_turns(&source, "source-repo").unwrap();
        let artifact = build_export_artifact(ExportBuildRequest {
            context: &source_context,
            settings: &settings,
            project_key: "git:https://example.invalid/team/repo",
            identity: &source_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
        })
        .unwrap();
        write_export_artifact(&cache, &artifact).unwrap();
        import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        manifest.exporter.user_id = "usr-attacker".to_owned();
        manifest.turns.clear();
        write_json_file(&manifest_path, &manifest).unwrap();

        let report = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();
        let target = open_index_database_writer(&target_context.index_db_path).unwrap();
        let session_count: i64 = target
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE origin_kind = 'shared'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(report.imported_turn_count, 0);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("exporter"),
            "warning should reject mismatched exporter: {:?}",
            report.warnings
        );
        assert_eq!(session_count, 1);
    }

    #[test]
    fn merge_prunes_removed_turns_inside_kept_sessions() {
        let workspace = unique_test_dir("share-prune-removed-turns");
        let cache = workspace.join("cache");
        let source_root = workspace.join("source-root");
        let target_root = workspace.join("target-root");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        let target_key = ensure_share_key(&target_root).unwrap();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let session_id = "00000000-0000-4000-8000-000000000303";
        seed_share_export_session(&source_context.index_db_path, "source-repo", session_id);
        let source_connection = open_index_database_writer(&source_context.index_db_path).unwrap();
        insert_indexed_turn(
            &source_connection,
            IndexedTurnFixture::new(
                "source-repo",
                SourceKind::Codex,
                session_id,
                2,
                "2026-05-15T13:00:00Z",
                "completed",
                "[]",
            ),
        )
        .unwrap();
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let source_age_identity = Identity::generate();
        let source_signing_key = test_signing_key(&source_age_identity);
        let source_identity = test_share_identity(&source_age_identity);
        let settings = ShareSettings {
            remotes: Vec::new(),
            recipients: vec![ShareRecipient {
                recipient: target_key.public_key,
            }],
        };
        let turns = query_share_export_turns(&source_connection, "source-repo").unwrap();
        let full = build_export_artifact(ExportBuildRequest {
            context: &source_context,
            settings: &settings,
            project_key: "git:https://example.invalid/team/repo",
            identity: &source_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns: turns.clone(),
        })
        .unwrap();
        write_export_artifact(&cache, &full).unwrap();
        import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();
        let shortened = build_export_artifact(ExportBuildRequest {
            context: &source_context,
            settings: &settings,
            project_key: "git:https://example.invalid/team/repo",
            identity: &source_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns: turns.into_iter().take(1).collect(),
        })
        .unwrap();
        write_export_artifact(&cache, &shortened).unwrap();

        import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "https://example.invalid/team/share.git",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();

        let target = open_index_database_writer(&target_context.index_db_path).unwrap();
        let turn_count: i64 = target
            .query_row(
                "
                SELECT COUNT(*)
                FROM turns
                JOIN sessions
                    ON sessions.project_id = turns.project_id
                    AND sessions.provider = turns.provider
                    AND sessions.session_id = turns.session_id
                WHERE sessions.origin_kind = 'shared'
                ",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(turn_count, 1);
    }

    /// Carries one synthetic one-turn export plus the target import context.
    struct TestShareArtifact {
        cache: PathBuf,
        target_context: ShareProjectContext,
        target_identity: Identity,
        source_identity: ShareIdentity,
        source_signing_key: SigningKey,
        artifact: BuiltExportArtifact,
    }

    /// Builds one encrypted one-turn artifact for share import tests.
    fn build_single_turn_test_artifact(prefix: &str) -> TestShareArtifact {
        let workspace = unique_test_dir(prefix);
        let cache = workspace.join("cache");
        let source_root = workspace.join("source-root");
        let target_root = workspace.join("target-root");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        let target_key = ensure_share_key(&target_root).unwrap();
        let target_identity = read_share_identity_key(&target_key.key_path).unwrap();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let source_age_identity = Identity::generate();
        let source_signing_key = test_signing_key(&source_age_identity);
        let source_identity = test_share_identity(&source_age_identity);
        let source = open_index_database_writer(&source_context.index_db_path).unwrap();
        let turns = query_share_export_turns(&source, "source-repo").unwrap();
        let artifact = build_export_artifact(ExportBuildRequest {
            context: &source_context,
            settings: &ShareSettings {
                remotes: Vec::new(),
                recipients: vec![ShareRecipient {
                    recipient: target_key.public_key,
                }],
            },
            project_key: "git:https://example.invalid/team/repo",
            identity: &source_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
        })
        .unwrap();
        TestShareArtifact {
            cache,
            target_context,
            target_identity,
            source_identity,
            source_signing_key,
            artifact,
        }
    }

    /// Builds a synthetic artifact whose turn payloads span multiple chunks.
    fn build_multi_chunk_test_artifact(prefix: &str) -> TestShareArtifact {
        let workspace = unique_test_dir(prefix);
        let cache = workspace.join("cache");
        let source_root = workspace.join("source-root");
        let target_root = workspace.join("target-root");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        let target_key = ensure_share_key(&target_root).unwrap();
        let target_identity = read_share_identity_key(&target_key.key_path).unwrap();
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_root.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let session_id = "00000000-0000-4000-8000-000000000303";
        let connection = open_index_database_writer(&source_context.index_db_path).unwrap();
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new("source-repo", SourceKind::Codex, session_id, "/tmp/repo"),
        )
        .unwrap();
        let large_message = "synthetic share export prompt ".repeat(300);
        for ordinal in 1..=3 {
            let started_at = format!("2026-05-15T12:00:0{ordinal}Z");
            insert_indexed_turn(
                &connection,
                IndexedTurnFixture {
                    user_message: &large_message,
                    final_answer_text: Some("synthetic share export answer"),
                    has_final_answer: true,
                    ..IndexedTurnFixture::new(
                        "source-repo",
                        SourceKind::Codex,
                        session_id,
                        ordinal,
                        &started_at,
                        "completed",
                        "[]",
                    )
                },
            )
            .unwrap();
        }
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let source_age_identity = Identity::generate();
        let source_signing_key = test_signing_key(&source_age_identity);
        let source_identity = test_share_identity(&source_age_identity);
        let turns = query_share_export_turns(&connection, "source-repo").unwrap();
        let artifact = build_export_artifact(ExportBuildRequest {
            context: &source_context,
            settings: &ShareSettings {
                remotes: Vec::new(),
                recipients: vec![ShareRecipient {
                    recipient: target_key.public_key,
                }],
            },
            project_key: "git:https://example.invalid/team/repo",
            identity: &source_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
        })
        .unwrap();
        assert!(
            artifact.manifest.chunks.len() > 1,
            "test fixture should span multiple chunks"
        );
        TestShareArtifact {
            cache,
            target_context,
            target_identity,
            source_identity,
            source_signing_key,
            artifact,
        }
    }

    /// Reads the first chunk payload from a synthetic test manifest.
    fn read_test_chunk_payload(
        cache: &Path,
        manifest: &ManifestArtifact,
        identity: &Identity,
    ) -> ShareChunkPayload {
        let chunk = manifest
            .chunks
            .first()
            .expect("test artifact should contain a chunk");
        let ciphertext = fs::read(cache.join(&chunk.object_path)).unwrap();
        let compressed = decrypt_payload(&ciphertext, identity).unwrap();
        let plaintext = gzip_decompress(&compressed).unwrap();
        serde_json::from_slice(&plaintext).unwrap()
    }

    /// Rewrites the first encrypted chunk in a synthetic test manifest.
    fn write_test_chunk_payload(
        cache: &Path,
        manifest: &mut ManifestArtifact,
        payload: &ShareChunkPayload,
        recipients: &[Recipient],
    ) {
        let chunk = manifest
            .chunks
            .first_mut()
            .expect("test artifact should contain a chunk");
        let plaintext = serde_json::to_vec(payload).unwrap();
        let compressed = gzip_compress(&plaintext).unwrap();
        let ciphertext = encrypt_payload(&compressed, recipients).unwrap();
        fs::write(cache.join(&chunk.object_path), &ciphertext).unwrap();
        chunk.plaintext_hash = sha256_hex(&compressed);
        chunk.ciphertext_hash = sha256_hex(&ciphertext);
        chunk.plaintext_bytes = u64::try_from(compressed.len()).unwrap();
        chunk.ciphertext_bytes = u64::try_from(ciphertext.len()).unwrap();
        chunk.turn_count = u64::try_from(payload.turns.len()).unwrap();
    }

    /// Builds one synthetic shared turn export for legacy artifact tests.
    fn synthetic_share_turn(
        project_id: &str,
        session_id: &str,
        turn_ordinal: i64,
    ) -> ShareTurnExport {
        ShareTurnExport {
            session: ShareSessionExport {
                project_id: project_id.to_owned(),
                provider: SourceKind::Codex,
                session_id: session_id.to_owned(),
                parent_session_id: None,
                session_kind: "primary".to_owned(),
                archive_path: "synthetic.jsonl".to_owned(),
                cwd: "/tmp/synthetic-repo".to_owned(),
                cli_version: Some("0.1.0".to_owned()),
                schema_id: Some("codex:test".to_owned()),
                determinism: Some("exact".to_owned()),
                source_size: Some(1),
                source_mtime_ms: Some(1),
            },
            turn_ordinal,
            turn_id: Some(format!("turn-{turn_ordinal}")),
            started_at: "2026-05-15T00:00:00Z".to_owned(),
            completed_at: Some("2026-05-15T00:00:01Z".to_owned()),
            status: "completed".to_owned(),
            user_message: "synthetic prompt".to_owned(),
            final_answer_at: Some("2026-05-15T00:00:01Z".to_owned()),
            final_answer_text: Some("synthetic answer".to_owned()),
            steps_json: "[]".to_owned(),
            step_count: 0,
            tool_call_count: 0,
            tool_output_count: 0,
            attachment_count: 0,
            delegation_count: 0,
            hook_summary_count: 0,
            has_final_answer: 1,
            duration_ms: Some(1),
            effective_agent_runtime_ms: Some(1),
            provider_total_token_count: Some(1),
            input_uncached_token_count: Some(1),
            cache_read_token_count: Some(0),
            cache_write_token_count: Some(0),
            output_token_count: Some(1),
            reasoning_token_count: Some(0),
            total_token_count: Some(1),
            primary_model: Some("synthetic-model".to_owned()),
            changed_file_count: 0,
            added_line_count: 0,
            removed_line_count: 0,
        }
    }

    fn init_test_git_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        run_git(path, ["init"], "failed to init synthetic Git repo").unwrap();
        run_git(
            path,
            ["config", "user.name", "Synthetic User"],
            "failed to set synthetic Git user.name",
        )
        .unwrap();
        run_git(
            path,
            ["config", "user.email", "synthetic@example.invalid"],
            "failed to set synthetic Git user.email",
        )
        .unwrap();
    }

    fn init_bare_remote(path: &Path) {
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        run_git(
            parent,
            ["init", "--bare", path.to_str().unwrap()],
            "failed to init synthetic bare Git repo",
        )
        .unwrap();
    }

    fn delete_remote_branch(remote_path: &Path, git_branch: &str) {
        let reference = format!("refs/heads/{git_branch}");
        run_git(
            remote_path,
            ["update-ref", "-d", &reference],
            "failed to delete synthetic remote branch",
        )
        .unwrap();
    }

    fn test_share_identity(identity: &Identity) -> ShareIdentity {
        let signing_key = test_signing_key(identity);
        let signing_public_key = signing_public_key_hex(&signing_key);
        ShareIdentity {
            user_id: derive_user_id(&signing_public_key),
            display_name: Some("Synthetic User".to_owned()),
            email: Some("synthetic@example.invalid".to_owned()),
            public_key: identity.to_public().to_string(),
            signing_public_key,
        }
    }

    fn test_signing_key(identity: &Identity) -> SigningKey {
        let secret = identity.to_string();
        let seed = Sha256::digest(secret.expose_secret().as_bytes());
        let mut bytes = [0_u8; 32];
        bytes.copy_from_slice(&seed);
        SigningKey::from_bytes(&bytes)
    }

    /// Builds signed sync chunk entries from one visible manifest fixture.
    fn sync_chunks_from_manifest(manifest: &ManifestArtifact) -> Vec<SyncChunkEntry> {
        manifest
            .chunks
            .iter()
            .map(sync_chunk_from_manifest)
            .collect()
    }

    fn write_test_sync_object(
        cache: &Path,
        identity: &Identity,
        signing_key: &SigningKey,
        exporter: &ShareIdentity,
        project_key: &str,
        turns: Vec<SyncTurnEntry>,
    ) -> SyncManifestEntry {
        write_test_sync_object_with_chunks(
            cache,
            identity,
            signing_key,
            exporter,
            project_key,
            Vec::new(),
            turns,
        )
    }

    fn write_test_sync_object_with_chunks(
        cache: &Path,
        identity: &Identity,
        signing_key: &SigningKey,
        exporter: &ShareIdentity,
        project_key: &str,
        chunks: Vec<SyncChunkEntry>,
        turns: Vec<SyncTurnEntry>,
    ) -> SyncManifestEntry {
        let mut payload = EncryptedSyncPayload {
            schema: SYNC_PAYLOAD_SCHEMA.to_owned(),
            version: SYNC_PAYLOAD_VERSION,
            project_key: project_key.to_owned(),
            export_fingerprint: String::new(),
            exporter: exporter.clone(),
            signature: None,
            sessions: Vec::new(),
            chunks,
            turns,
        };
        sign_sync_payload(&mut payload, signing_key).unwrap();
        let plaintext = serde_json::to_vec(&payload).unwrap();
        let payload_hash = sha256_hex(&plaintext);
        let object_path = format!("{ARTIFACT_ROOT}/objects/sync-test-{payload_hash}.age");
        let target = cache.join(&object_path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        let encrypted = encrypt_payload(&plaintext, &[identity.to_public()]).unwrap();
        fs::write(&target, encrypted).unwrap();
        SyncManifestEntry {
            payload_hash,
            object_path,
        }
    }

    fn remote_tip_parent_count(remote_path: &Path, git_branch: &str) -> usize {
        let reference = format!("refs/heads/{git_branch}");
        let output = run_git(
            remote_path,
            ["rev-list", "--parents", "-n", "1", &reference],
            "failed to inspect synthetic remote commit parents",
        )
        .unwrap();
        output.stdout.split_whitespace().count().saturating_sub(1)
    }

    fn remote_tip_blob_paths(remote_path: &Path, git_branch: &str) -> Vec<String> {
        let reference = format!("refs/heads/{git_branch}");
        let output = run_git(
            remote_path,
            ["ls-tree", "-r", "--name-only", &reference],
            "failed to inspect synthetic remote tree",
        )
        .unwrap();
        let mut paths = output.stdout.lines().map(str::to_owned).collect::<Vec<_>>();
        paths.sort();
        paths
    }

    #[test]
    fn git_failure_messages_redact_credentialed_urls() {
        let output = GitCommandOutput {
            status: failure_exit_status(),
            stdout: "https://user:token@example.invalid/repo.git?access_token=secret".to_owned(),
            stderr: "fatal: could not read Username for 'https://user:token@example.invalid/repo.git?access_token=secret'".to_owned(),
        };
        let message = git_failure_message("failed synthetic git command", &output);

        assert!(message.contains("https://example.invalid/repo.git"));
        assert!(!message.contains("user:token"));
        assert!(!message.contains("access_token"));
        assert!(!message.contains("secret"));

        let display = git_args_display(&[
            OsString::from("remote"),
            OsString::from("add"),
            OsString::from("https://user:token@example.invalid/repo.git?access_token=secret"),
        ]);
        assert!(display.contains("https://example.invalid/repo.git"));
        assert!(!display.contains("user:token"));
        assert!(!display.contains("access_token"));
        assert!(!display.contains("secret"));
    }

    #[test]
    fn git_failure_message_keeps_streamed_stderr_in_returned_error() {
        let output = GitCommandOutput {
            status: failure_exit_status(),
            stdout: String::new(),
            stderr: "fatal: synthetic upload failure".to_owned(),
        };

        let message = git_failure_message("failed synthetic git command", &output);

        assert!(message.contains("failed synthetic git command"));
        assert!(message.contains("synthetic upload failure"));
    }

    #[test]
    fn git_progress_stderr_reader_emits_carriage_return_fragments() {
        let mut input: &[u8] = b"Writing objects: 50% (1/2)\rWriting objects: 100% (2/2)\n";
        let mut events = Vec::new();

        let stderr = read_git_progress_stderr(&mut input, ShareUploadKind::Git, &mut |event| {
            events.push(event);
        })
        .unwrap();

        assert!(String::from_utf8_lossy(&stderr).contains("Writing objects: 100%"));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                SharePushProgress::GitProgress { message, .. }
                    if message.contains("50%")
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                SharePushProgress::GitProgress { message, .. }
                    if message.contains("100%")
            )
        }));
    }

    #[test]
    fn spawned_git_progress_reader_emits_child_stderr_fragments() {
        let child = Command::new("sh")
            .arg("-c")
            .arg("printf '%s\\r' 'Writing objects: 25% (1/4)' >&2; printf stdout")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut events = Vec::new();

        let output = collect_streaming_command_output(child, ShareUploadKind::Git, &mut |event| {
            events.push(event);
        })
        .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout, "stdout");
        assert!(output.stderr.contains("Writing objects: 25%"));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                SharePushProgress::GitProgress {
                    kind: ShareUploadKind::Git,
                    message
                } if message.contains("25%")
            )
        }));
    }

    #[test]
    fn push_branch_commands_cover_lfs_and_git_upload_progress() {
        let commands = push_branch_commands("darc/team", true);
        assert_eq!(
            commands
                .iter()
                .map(|command| command.kind)
                .collect::<Vec<_>>(),
            vec![ShareUploadKind::Lfs, ShareUploadKind::Git]
        );
        assert_eq!(
            commands[0]
                .progress_args
                .iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>(),
            vec!["lfs", "push", DEFAULT_REMOTE_NAME, "refs/heads/darc/team"]
        );
        assert_eq!(
            commands[1]
                .progress_args
                .iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>(),
            vec![
                "push",
                "--progress",
                DEFAULT_REMOTE_NAME,
                "refs/heads/darc/team:refs/heads/darc/team"
            ]
        );

        let git_only = push_branch_commands("darc/team", false);
        assert_eq!(git_only.len(), 1);
        assert_eq!(git_only[0].kind, ShareUploadKind::Git);
        assert!(!git_only[0].quiet_args.iter().any(|arg| arg == "--progress"));
    }

    #[test]
    fn lfs_publish_requires_explicit_opt_in() {
        assert!(!git_lfs_publish_enabled_from_env(None, None, true));
        assert!(git_lfs_publish_enabled_from_env(
            Some(OsString::from("1")),
            None,
            true
        ));
        assert!(!git_lfs_publish_enabled_from_env(
            Some(OsString::from("1")),
            Some(OsString::from("1")),
            true
        ));
        assert!(!git_lfs_publish_enabled_from_env(
            Some(OsString::from("1")),
            None,
            false
        ));
    }

    #[test]
    fn reset_cached_checkout_disables_lfs_smudge_filters() {
        let workspace = unique_test_dir("share-reset-disables-lfs-smudge");
        init_test_git_repo(&workspace);
        fs::write(workspace.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
        fs::write(workspace.join("object.bin"), "synthetic payload").unwrap();
        run_git(
            &workspace,
            ["config", "filter.lfs.clean", "cat"],
            "failed to configure synthetic LFS clean filter",
        )
        .unwrap();
        run_git(
            &workspace,
            [
                "config",
                "filter.lfs.smudge",
                "darc-synthetic-missing-smudge",
            ],
            "failed to configure synthetic LFS smudge filter",
        )
        .unwrap();
        run_git(
            &workspace,
            ["config", "filter.lfs.required", "true"],
            "failed to configure synthetic LFS required filter",
        )
        .unwrap();
        run_git(
            &workspace,
            ["add", ".gitattributes", "object.bin"],
            "failed to stage synthetic LFS file",
        )
        .unwrap();
        run_git(
            &workspace,
            ["commit", "-m", "test: add synthetic file"],
            "failed to commit synthetic LFS file",
        )
        .unwrap();
        fs::remove_file(workspace.join("object.bin")).unwrap();

        reset_cached_checkout(&workspace).unwrap();

        assert_eq!(
            fs::read_to_string(workspace.join("object.bin")).unwrap(),
            "synthetic payload"
        );
    }

    #[test]
    fn ssh_command_runs_in_batch_mode() {
        assert_eq!(
            noninteractive_ssh_command(None),
            OsString::from("ssh -o BatchMode=yes")
        );
        assert_eq!(
            noninteractive_ssh_command(Some(OsString::from("ssh -F config"))),
            OsString::from("ssh -o BatchMode=yes -F config")
        );
        assert_eq!(
            noninteractive_ssh_command(Some(OsString::from("ssh -o BatchMode=yes"))),
            OsString::from("ssh -o BatchMode=yes")
        );
        assert_eq!(
            noninteractive_ssh_command(Some(OsString::from("ssh -o BatchMode=no"))),
            OsString::from("ssh -o BatchMode=yes")
        );
        assert_eq!(
            noninteractive_ssh_command(Some(OsString::from("DARC_SSH_OPTION=1 ssh -F config"))),
            OsString::from("DARC_SSH_OPTION=1 ssh -F config -o BatchMode=yes")
        );
        assert_eq!(
            noninteractive_ssh_command(Some(OsString::from("\"/tmp/synthetic ssh\" -F config"))),
            OsString::from("\"/tmp/synthetic ssh\" -F config -o BatchMode=yes")
        );
        assert_eq!(
            noninteractive_ssh_command(Some(OsString::from("/usr/bin/ssh -F config"))),
            OsString::from("/usr/bin/ssh -F config -o BatchMode=yes")
        );
    }

    #[test]
    fn ssh_command_preserves_core_ssh_command() {
        let workspace = unique_test_dir("share-core-ssh-command");
        init_test_git_repo(&workspace);
        run_git(
            &workspace,
            ["config", "core.sshCommand", "ssh -i synthetic_key"],
            "failed to configure core.sshCommand",
        )
        .unwrap();

        assert_eq!(
            git_ssh_command_with_env(&workspace, None),
            OsString::from("ssh -o BatchMode=yes -i synthetic_key")
        );
        assert_eq!(
            git_ssh_command_with_env(&workspace, Some(OsString::from("ssh -F env_config"))),
            OsString::from("ssh -o BatchMode=yes -F env_config")
        );
    }

    #[test]
    fn prepare_cache_repository_copies_project_ssh_command() {
        let workspace = unique_test_dir("share-cache-project-ssh-command");
        let source = workspace.join("source");
        let cache = workspace.join("cache");
        init_test_git_repo(&source);
        run_git(
            &source,
            ["config", "core.sshCommand", "ssh -i synthetic_key"],
            "failed to configure source core.sshCommand",
        )
        .unwrap();
        let identity = test_share_identity(&Identity::generate());

        prepare_cache_repository(
            &cache,
            "https://example.invalid/share.git",
            &source,
            &identity,
        )
        .unwrap();

        assert_eq!(
            git_core_ssh_command(&cache),
            Some(OsString::from("ssh -i synthetic_key"))
        );
        assert_eq!(
            git_ssh_command_with_env(&cache, None),
            OsString::from("ssh -o BatchMode=yes -i synthetic_key")
        );

        run_git(
            &source,
            ["config", "--unset", "core.sshCommand"],
            "failed to clear source core.sshCommand",
        )
        .unwrap();
        prepare_cache_repository(
            &cache,
            "https://example.invalid/share.git",
            &source,
            &identity,
        )
        .unwrap();

        assert_eq!(git_core_ssh_command(&cache), None);
    }

    #[cfg(unix)]
    fn failure_exit_status() -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;

        ExitStatus::from_raw(1 << 8)
    }

    #[cfg(windows)]
    fn failure_exit_status() -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;

        ExitStatus::from_raw(1)
    }

    /// Returns the first per-exporter manifest path in one cache checkout.
    fn first_exporter_manifest_path(cache: &Path) -> PathBuf {
        fs::read_dir(cache.join(ARTIFACT_ROOT).join(EXPORTERS_DIR))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().join(LEGACY_MANIFEST_FILE))
            .find(|path| path.exists())
            .expect("cache should contain an exporter manifest")
    }

    fn seed_share_export_session(index_db_path: &Path, project_id: &str, session_id: &str) {
        let connection = open_index_database_writer(index_db_path).unwrap();
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new(project_id, SourceKind::Codex, session_id, "/tmp/repo"),
        )
        .unwrap();
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture {
                user_message: "synthetic share export prompt",
                final_answer_text: Some("synthetic share export answer"),
                has_final_answer: true,
                ..IndexedTurnFixture::new(
                    project_id,
                    SourceKind::Codex,
                    session_id,
                    1,
                    "2026-05-15T12:00:00Z",
                    "completed",
                    "[]",
                )
            },
        )
        .unwrap();
    }
}
