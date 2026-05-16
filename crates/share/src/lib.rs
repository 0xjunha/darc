//! Git-backed encrypted sharing for redacted Darc index projections.

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    str::FromStr,
};

use age::{
    secrecy::ExposeSecret,
    x25519::{Identity, Recipient},
};
use anyhow::{Context, Result, bail};
use darc_paths::current_utc_timestamp;
use darc_store::{
    SharePolicy, ShareState, ShareTurnExport, ShareTurnImport, ShareUserRecord,
    clear_project_share_states, import_shared_turn, open_index_database_writer, prune_shared_turns,
    query_share_export_turns, query_share_status, set_project_share_policy,
    set_session_share_state,
};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use git2::{
    BranchType, Cred, FetchOptions, FetchPrune, IndexAddOption, PushOptions, RemoteCallbacks,
    Repository, ResetType, Status, StatusOptions,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const ARTIFACT_ROOT: &str = "darc-share/v1";
const PROJECT_SCHEMA: &str = "darc.share.project.v1";
const MANIFEST_SCHEMA: &str = "darc.share.manifest.v1";
const TURN_PAYLOAD_SCHEMA: &str = "darc.share.turn.v1";
const SYNC_PAYLOAD_SCHEMA: &str = "darc.share.sync.v1";
const LEGACY_MANIFEST_FILE: &str = "manifest.json";
const PROJECT_FILE: &str = "project.json";
const EXPORTERS_DIR: &str = "exporters";
const KEY_FILE_NAME: &str = "share.agekey";
const SIGNING_KEY_FILE_NAME: &str = "share.signingkey";
const SHARE_CACHE_DIR: &str = "share-cache";
const DEFAULT_REMOTE_NAME: &str = "origin";
const MAX_SHARE_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
#[cfg(not(test))]
const MAX_CACHED_SHARE_MANIFESTS: usize = 1024;
#[cfg(test)]
const MAX_CACHED_SHARE_MANIFESTS: usize = 8;
#[cfg(not(test))]
const MAX_CACHED_SHARE_MANIFEST_BYTES: u64 = 128 * 1024 * 1024;
#[cfg(test)]
const MAX_CACHED_SHARE_MANIFEST_BYTES: u64 = 16 * 1024;
const MAX_SHARE_OBJECT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SHARE_EXPORT_OBJECTS: usize = 100_000;
const MAX_SHARE_EXPORT_BYTES: usize = 512 * 1024 * 1024;
const TURN_SIGNATURE_DOMAIN: &[u8] = b"darc.share.turn.signature.v1";
const SYNC_SIGNATURE_DOMAIN: &[u8] = b"darc.share.sync.signature.v1";

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

/// Stores one encrypted export manifest used to authenticate pruning inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EncryptedSyncPayload {
    schema: String,
    version: u32,
    project_key: String,
    exporter: ShareIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    turns: Vec<SyncTurnEntry>,
}

/// Stores one authenticated exported turn identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct SyncTurnEntry {
    provider: darc_paths::SourceKind,
    session_id: String,
    turn_ordinal: i64,
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
        let parent = key_path
            .parent()
            .context("share key path is missing a parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
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
        let parent = key_path
            .parent()
            .context("share signing key path is missing a parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let entropy = Identity::generate().to_string();
        let seed = Sha256::digest(entropy.expose_secret().as_bytes());
        write_share_private_key(&key_path, &hex_encode(&seed))?;
    }
    harden_private_key_permissions(&key_path)?;
    read_share_signing_key(&key_path)
}

/// Builds the local share identity from Git config and the Darc public key.
pub fn local_share_identity(context: &ShareProjectContext) -> Result<ShareIdentity> {
    let key = ensure_share_key(&context.root)?;
    let signing_key = ensure_share_signing_key(&context.root)?;
    let signing_public_key = signing_public_key_hex(&signing_key);
    let repository = Repository::discover(&context.local_path).with_context(|| {
        format!(
            "failed to discover Git repository from {}",
            context.local_path.display()
        )
    })?;
    let config = repository.config().context("failed to read Git config")?;
    let display_name = config.get_string("user.name").ok();
    let email = config.get_string("user.email").ok();
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
    let connection = open_index_database_writer(&context.index_db_path)?;
    set_project_share_policy(
        &connection,
        &context.project_id,
        SharePolicy::All,
        &current_utc_timestamp(),
    )?;
    clear_project_share_states(&connection, &context.project_id)?;
    Ok(())
}

/// Excludes all local sessions by switching to manual policy and clearing overrides.
pub fn exclude_all_sessions(context: &ShareProjectContext) -> Result<()> {
    let connection = open_index_database_writer(&context.index_db_path)?;
    set_project_share_policy(
        &connection,
        &context.project_id,
        SharePolicy::Manual,
        &current_utc_timestamp(),
    )?;
    clear_project_share_states(&connection, &context.project_id)?;
    Ok(())
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
    let git_branch = share_git_branch(branch)?;
    let remote = resolve_remote(context, settings, remote_name)?;
    let project_key = project_key(context)?;
    let identity = local_share_identity(context)?;
    let identity_key = ensure_share_key(&context.root)?;
    let decryption_identity = read_share_identity_key(&identity_key.key_path)?;
    let signing_key = ensure_share_signing_key(&context.root)?;
    let connection = open_index_database_writer(&context.index_db_path)?;
    let turns = query_share_export_turns(&connection, &context.project_id)?;
    let cache_path = cache_repo_path(&context.root, &remote.url, &git_branch);
    let repository = prepare_cache_repository(&cache_path, &remote.url, &identity)?;
    let branch_exists = fetch_branch_if_exists(&repository, &remote.url, &git_branch)?;
    if !branch_exists {
        clear_cache_worktree(&cache_path)?;
    }
    checkout_share_branch(&repository, &git_branch)?;
    clean_untracked_cache_worktree(&repository, &cache_path)?;
    let cached_manifest_read = read_cached_manifests(&cache_path)?;
    let retained_manifests = authenticated_retained_manifests(
        &cache_path,
        &cached_manifest_read.manifests,
        &project_key,
        &identity,
        &decryption_identity,
    );
    let artifact = build_export_artifact(ExportBuildRequest {
        context,
        settings,
        project_key: &project_key,
        identity: &identity,
        decryption_identity: &decryption_identity,
        signing_key: &signing_key,
        branch,
        turns,
        cache_path: &cache_path,
    })?;
    remove_replaced_exporter_artifacts(
        &cache_path,
        &identity,
        &cached_manifest_read.manifests,
        &retained_manifests,
        &artifact,
    )?;
    write_export_artifact(&cache_path, &artifact)?;
    let allowed_paths = allowed_share_cache_paths(&artifact, &retained_manifests);
    clean_unexpected_share_cache_files(&cache_path, &allowed_paths)?;
    let commit_id = commit_cache_repository(&repository, &identity, &git_branch)?;
    push_branch(&repository, &remote.url, &git_branch)?;
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

/// Fetches one share branch into the local Darc share cache.
pub fn fetch_share_branch(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    branch: &str,
    remote_name: Option<&str>,
) -> Result<ShareFetchReport> {
    let git_branch = share_git_branch(branch)?;
    let remote = resolve_remote(context, settings, remote_name)?;
    let identity = local_share_identity(context)?;
    let cache_path = cache_repo_path(&context.root, &remote.url, &git_branch);
    let repository = prepare_cache_repository(&cache_path, &remote.url, &identity)?;
    fetch_branch(&repository, &remote.url, &git_branch)?;
    checkout_share_branch(&repository, &git_branch)?;
    clean_untracked_cache_worktree(&repository, &cache_path)?;
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
    let git_branch = share_git_branch(branch)?;
    let remote = resolve_remote(context, settings, remote_name)?;
    let project_key = project_key(context)?;
    let cache_path = cache_repo_path(&context.root, &remote.url, &git_branch);
    clean_cached_checkout(&cache_path)?;
    import_from_cache(
        context,
        branch,
        &git_branch,
        &remote.name,
        &remote.url,
        &project_key,
        &cache_path,
    )
}

/// Fetches and imports one share branch.
pub fn pull_share_branch(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    branch: &str,
    remote_name: Option<&str>,
) -> Result<SharePullReport> {
    let fetch = fetch_share_branch(context, settings, branch, remote_name)?;
    let merge = merge_share_branch(context, settings, branch, remote_name)?;
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

/// Stores one in-memory export artifact before writing to disk.
struct BuiltExportArtifact {
    project: ProjectArtifact,
    manifest: ManifestArtifact,
    objects: BTreeMap<String, Vec<u8>>,
    exported_turn_count: u64,
    exported_session_count: u64,
    object_count: u64,
}

/// Stores one resolved remote target.
#[derive(Debug, Clone)]
struct ResolvedRemote {
    name: String,
    url: String,
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
    decryption_identity: &'a Identity,
    signing_key: &'a SigningKey,
    branch: &'a str,
    turns: Vec<ShareTurnExport>,
    cache_path: &'a Path,
}

/// Stores immutable context used while importing one manifest entry.
struct ImportEntryContext<'a> {
    expected_project_key: &'a str,
    project_id: &'a str,
    origin_remote: &'a str,
    expected_exporter: &'a ShareIdentity,
    identity: &'a Identity,
    cache_path: &'a Path,
}

/// Builds all share artifacts for the current export.
fn build_export_artifact(request: ExportBuildRequest<'_>) -> Result<BuiltExportArtifact> {
    let timestamp = current_utc_timestamp();
    let recipient_strings = encryption_recipient_strings(request.identity, request.settings);
    let recipient_fingerprint = encryption_recipient_fingerprint(&recipient_strings);
    let recipients = parse_encryption_recipients(&recipient_strings)?;
    let mut objects = BTreeMap::new();
    let mut total_object_bytes = 0_usize;
    let mut manifest_turns = Vec::with_capacity(request.turns.len());
    let mut session_ids = BTreeSet::new();
    for turn in request.turns {
        session_ids.insert((turn.session.provider, turn.session.session_id.clone()));
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
        let object_path =
            format!("{ARTIFACT_ROOT}/objects/{recipient_fingerprint}-{payload_hash}.age");
        let encrypted = reusable_existing_object(
            read_existing_object(request.cache_path, &object_path)?.as_ref(),
            request.decryption_identity,
            &plaintext,
            &recipients,
        )?;
        insert_export_object(
            &mut objects,
            &mut total_object_bytes,
            object_path.clone(),
            encrypted,
        )?;
        manifest_turns.push(TurnManifestEntry {
            provider: payload.turn.session.provider,
            session_id: payload.turn.session.session_id,
            turn_ordinal: payload.turn.turn_ordinal,
            started_at: payload.turn.started_at,
            payload_hash,
            object_path,
        });
    }
    let mut sync_payload = EncryptedSyncPayload {
        schema: SYNC_PAYLOAD_SCHEMA.to_owned(),
        version: 1,
        project_key: request.project_key.to_owned(),
        exporter: request.identity.clone(),
        signature: None,
        turns: manifest_turns
            .iter()
            .map(|entry| SyncTurnEntry {
                provider: entry.provider,
                session_id: entry.session_id.clone(),
                turn_ordinal: entry.turn_ordinal,
            })
            .collect(),
    };
    sign_sync_payload(&mut sync_payload, request.signing_key)?;
    let sync_plaintext =
        serde_json::to_vec(&sync_payload).context("failed to serialize share sync payload")?;
    let sync_payload_hash = sha256_hex(&sync_plaintext);
    let sync_object_path =
        format!("{ARTIFACT_ROOT}/objects/sync-{recipient_fingerprint}-{sync_payload_hash}.age");
    let sync_encrypted = reusable_existing_object(
        read_existing_object(request.cache_path, &sync_object_path)?.as_ref(),
        request.decryption_identity,
        &sync_plaintext,
        &recipients,
    )?;
    insert_export_object(
        &mut objects,
        &mut total_object_bytes,
        sync_object_path.clone(),
        sync_encrypted,
    )?;
    let exported_turn_count =
        u64::try_from(manifest_turns.len()).context("turn count exceeds u64 range")?;
    Ok(BuiltExportArtifact {
        project: ProjectArtifact {
            schema: PROJECT_SCHEMA.to_owned(),
            version: 1,
            project_key: request.project_key.to_owned(),
            project_name: request.context.project_name.clone(),
            updated_at: timestamp.clone(),
        },
        manifest: ManifestArtifact {
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
            turns: manifest_turns,
        },
        exported_turn_count,
        exported_session_count: u64::try_from(session_ids.len())
            .context("session count exceeds u64 range")?,
        object_count: u64::try_from(objects.len()).context("object count exceeds u64 range")?,
        objects,
    })
}

/// Inserts one encrypted export object while enforcing in-memory export caps.
fn insert_export_object(
    objects: &mut BTreeMap<String, Vec<u8>>,
    total_object_bytes: &mut usize,
    object_path: String,
    content: Vec<u8>,
) -> Result<()> {
    if objects.len() >= MAX_SHARE_EXPORT_OBJECTS {
        bail!("share export exceeds {MAX_SHARE_EXPORT_OBJECTS} encrypted objects");
    }
    *total_object_bytes = total_object_bytes
        .checked_add(content.len())
        .context("share export object size overflow")?;
    if *total_object_bytes > MAX_SHARE_EXPORT_BYTES {
        bail!("share export exceeds {MAX_SHARE_EXPORT_BYTES} encrypted bytes");
    }
    objects.insert(object_path, content);
    Ok(())
}

/// Writes all share artifacts into a cache repository workdir.
fn write_export_artifact(path: &Path, artifact: &BuiltExportArtifact) -> Result<()> {
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
    for (relative, content) in &artifact.objects {
        write_artifact_file(path, relative, content)?;
    }
    Ok(())
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
    let exporter_root = cache_path.join(ARTIFACT_ROOT).join(EXPORTERS_DIR);
    match is_regular_directory(&exporter_root) {
        Ok(true) => {
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
                let Some(exporter_dir) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let relative_path = format!(
                    "{ARTIFACT_ROOT}/{EXPORTERS_DIR}/{exporter_dir}/{LEGACY_MANIFEST_FILE}"
                );
                let manifest_path = cache_path.join(&relative_path);
                if manifest_path.exists()
                    && !read_cached_manifest(
                        &mut manifests,
                        &mut warnings,
                        &mut manifest_count,
                        &mut manifest_bytes,
                        relative_path,
                        &manifest_path,
                    )
                {
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
    if legacy_path.exists() {
        read_cached_manifest(
            &mut manifests,
            &mut warnings,
            &mut manifest_count,
            &mut manifest_bytes,
            legacy_relative_path,
            &legacy_path,
        );
    }
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
    relative_path: String,
    manifest_path: &Path,
) -> bool {
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
    let current_object_paths = artifact.objects.keys().cloned().collect::<BTreeSet<_>>();
    let retained_object_paths = retained_manifests
        .iter()
        .flat_map(|cached| manifest_object_paths(&cached.manifest))
        .collect::<BTreeSet<_>>();
    let stale_object_paths = cached_manifests
        .iter()
        .filter(|cached| exporter_manifest_id(&cached.manifest.exporter) == current_exporter_id)
        .flat_map(|cached| manifest_object_paths(&cached.manifest))
        .filter(|path| {
            !current_object_paths.contains(path) && !retained_object_paths.contains(path)
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

/// Returns cached manifests whose encrypted payloads authenticate for retention.
fn authenticated_retained_manifests(
    cache_path: &Path,
    cached_manifests: &[CachedManifest],
    expected_project_key: &str,
    identity: &ShareIdentity,
    decryption_identity: &Identity,
) -> Vec<CachedManifest> {
    let current_exporter_id = exporter_manifest_id(identity);
    cached_manifests
        .iter()
        .filter(|cached| exporter_manifest_id(&cached.manifest.exporter) != current_exporter_id)
        .filter(|cached| cached.manifest.schema == MANIFEST_SCHEMA)
        .filter(|cached| cached.manifest.project_key == expected_project_key)
        .filter_map(|cached| {
            let sync_payload = read_sync_payload(
                cache_path,
                &cached.manifest,
                expected_project_key,
                decryption_identity,
            )
            .ok()?;
            let authenticated_turns = sync_payload.turns.iter().cloned().collect::<BTreeSet<_>>();
            let turns_are_authenticated = cached.manifest.turns.iter().all(|entry| {
                authenticated_turns.contains(&SyncTurnEntry {
                    provider: entry.provider,
                    session_id: entry.session_id.clone(),
                    turn_ordinal: entry.turn_ordinal,
                }) && verify_cached_turn_payload(
                    cache_path,
                    &cached.manifest,
                    expected_project_key,
                    decryption_identity,
                    entry,
                )
                .is_ok()
            });
            turns_are_authenticated.then(|| cached.clone())
        })
        .collect()
}

/// Verifies one cached turn object without importing it into SQLite.
fn verify_cached_turn_payload(
    cache_path: &Path,
    manifest: &ManifestArtifact,
    expected_project_key: &str,
    identity: &Identity,
    entry: &TurnManifestEntry,
) -> Result<()> {
    let object_path = manifest_object_path(cache_path, entry)?;
    let ciphertext = read_regular_file(&object_path, MAX_SHARE_OBJECT_BYTES)?;
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
fn import_from_cache(
    context: &ShareProjectContext,
    branch: &str,
    git_branch: &str,
    remote_name: &str,
    remote_url: &str,
    expected_project_key: &str,
    cache_path: &Path,
) -> Result<ShareMergeReport> {
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
            continue;
        }
        let exporter_id = exporter_manifest_id(&manifest.exporter);
        if !imported_exporters.insert(exporter_id) {
            warnings.push(format!(
                "skipped duplicate share manifest {} for exporter {}",
                cached.relative_path, manifest.exporter.user_id
            ));
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
                    continue;
                }
            };
        let keep_turns = sync_payload
            .turns
            .iter()
            .map(|entry| (entry.provider, entry.session_id.clone(), entry.turn_ordinal))
            .collect::<BTreeSet<_>>();
        let authenticated_turns = sync_payload.turns.iter().cloned().collect::<BTreeSet<_>>();
        let import_context = ImportEntryContext {
            expected_project_key,
            project_id: &context.project_id,
            origin_remote: &origin_remote,
            expected_exporter: &sync_payload.exporter,
            identity: &identity,
            cache_path,
        };
        for entry in &manifest.turns {
            if !authenticated_turns.contains(&SyncTurnEntry {
                provider: entry.provider,
                session_id: entry.session_id.clone(),
                turn_ordinal: entry.turn_ordinal,
            }) {
                skipped_turn_count += 1;
                warnings.push(format!(
                    "skipped {} session {} turn {}: share manifest entry is not authenticated by sync payload",
                    entry.provider.directory_name(),
                    entry.session_id,
                    entry.turn_ordinal
                ));
                continue;
            }
            match import_manifest_entry(&mut connection, &import_context, entry) {
                Ok(true) => imported_turn_count += 1,
                Ok(false) => skipped_turn_count += 1,
                Err(error) => {
                    skipped_turn_count += 1;
                    warnings.push(format!(
                        "skipped {} session {} turn {}: {error:#}",
                        entry.provider.directory_name(),
                        entry.session_id,
                        entry.turn_ordinal
                    ));
                }
            }
        }
        prune_shared_turns(
            &connection,
            &context.project_id,
            &origin_remote,
            &sync_payload.exporter.user_id,
            &keep_turns,
        )?;
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

/// Reads and validates the encrypted sync payload used for pruning.
fn read_sync_payload(
    cache_path: &Path,
    manifest: &ManifestArtifact,
    expected_project_key: &str,
    identity: &Identity,
) -> Result<EncryptedSyncPayload> {
    let sync_path = manifest_artifact_path(cache_path, &manifest.sync.object_path)?;
    let ciphertext = read_regular_file(&sync_path, MAX_SHARE_OBJECT_BYTES)?;
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
    if payload.version != 1 {
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

/// Imports one manifest entry from an encrypted object file.
fn import_manifest_entry(
    connection: &mut Connection,
    context: &ImportEntryContext<'_>,
    entry: &TurnManifestEntry,
) -> Result<bool> {
    let object_path = manifest_object_path(context.cache_path, entry)?;
    let ciphertext = read_regular_file(&object_path, MAX_SHARE_OBJECT_BYTES)?;
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
    {
        bail!("share payload identity does not match manifest entry");
    }
    let user = ShareUserRecord {
        user_id: payload.exporter.user_id,
        display_name: payload.exporter.display_name,
        email: payload.exporter.email,
        public_key: Some(payload.exporter.public_key),
        source: "share-manifest".to_owned(),
        updated_at: current_utc_timestamp(),
    };
    import_shared_turn(
        connection,
        ShareTurnImport {
            project_id: context.project_id,
            user: &user,
            remote_name: context.origin_remote,
            imported_at: &current_utc_timestamp(),
            turn: &payload.turn,
        },
    )
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
        return Ok(ResolvedRemote {
            name: remote.name.clone(),
            display_url: sanitize_git_url_for_display(&remote.url),
            url: remote.url.clone(),
        });
    }
    if let Some(url) = context.git_upstream.clone() {
        return Ok(ResolvedRemote {
            name: DEFAULT_REMOTE_NAME.to_owned(),
            display_url: sanitize_git_url_for_display(&url),
            url,
        });
    }
    let repository = Repository::discover(&context.local_path).with_context(|| {
        format!(
            "failed to discover Git repository from {}",
            context.local_path.display()
        )
    })?;
    let remote = repository
        .find_remote(DEFAULT_REMOTE_NAME)
        .context("active project has no git_upstream and no origin remote")?;
    let url = remote
        .url()
        .context("origin remote URL is not valid UTF-8")?
        .to_owned();
    Ok(ResolvedRemote {
        name: DEFAULT_REMOTE_NAME.to_owned(),
        display_url: sanitize_git_url_for_display(&url),
        url,
    })
}

/// Returns the canonical shared-project key for one active project.
fn project_key(context: &ShareProjectContext) -> Result<String> {
    let url = if let Some(url) = context.git_upstream.as_deref() {
        url.to_owned()
    } else {
        let repository = Repository::discover(&context.local_path).with_context(|| {
            format!(
                "failed to discover Git repository from {}",
                context.local_path.display()
            )
        })?;
        let remote = repository
            .find_remote(DEFAULT_REMOTE_NAME)
            .context("active project has no git_upstream and no origin remote")?;
        remote
            .url()
            .context("origin remote URL is not valid UTF-8")?
            .to_owned()
    };
    Ok(format!("git:{}", normalize_git_url(&url)?))
}

/// Normalizes one Git URL enough for Darc project matching.
fn normalize_git_url(url: &str) -> Result<String> {
    let trimmed = url.trim().trim_end_matches('/').trim_end_matches(".git");
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

/// Normalizes one SSH scp-like Git URL.
fn normalize_scp_like_git_url(url: &str) -> Option<String> {
    if url.contains("://") {
        return None;
    }
    let (user_host, path) = url.split_once(':')?;
    let (_, host) = user_host.rsplit_once('@')?;
    Some(format!(
        "https://{}/{}",
        host.to_ascii_lowercase(),
        path.trim_start_matches('/')
    ))
}

/// Normalizes one scheme Git URL while removing credential userinfo.
fn normalize_scheme_git_url(url: &str, input_scheme: &str, output_scheme: &str) -> Option<String> {
    let rest = url.strip_prefix(input_scheme)?;
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
    let trimmed = url.trim();
    if let Some((scheme, rest)) = trimmed.split_once("://")
        && let Some((authority, path)) = rest.split_once('/')
    {
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        return format!("{scheme}://{host}/{path}");
    }
    if let Some((user_host, path)) = trimmed.split_once(':')
        && !trimmed.contains("://")
        && let Some((user, host)) = user_host.rsplit_once('@')
    {
        return format!("{user}@{host}:{path}");
    }
    trimmed.to_owned()
}

/// Reads and parses one age identity file.
fn read_share_identity_key(path: &Path) -> Result<Identity> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Identity::from_str(content.trim()).map_err(|error| anyhow::anyhow!("{error}"))
}

/// Reads and parses one Ed25519 share signing key file.
fn read_share_signing_key(path: &Path) -> Result<SigningKey> {
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
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(path)
            .with_context(|| format!("failed to inspect {}", path.display()))?
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
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

/// Reuses existing ciphertext only when it decrypts to the expected plaintext.
fn reusable_existing_object(
    existing: Option<&Vec<u8>>,
    identity: &Identity,
    plaintext: &[u8],
    recipients: &[Recipient],
) -> Result<Vec<u8>> {
    if let Some(existing) = existing
        && decrypt_payload(existing, identity).is_ok_and(|decrypted| decrypted == plaintext)
    {
        return Ok(existing.clone());
    }
    encrypt_payload(plaintext, recipients)
}

/// Prepares one local Git cache repository.
fn prepare_cache_repository(
    path: &Path,
    remote_url: &str,
    identity: &ShareIdentity,
) -> Result<Repository> {
    create_safe_cache_repository_dir(path)?;
    let repository = if path.join(".git").exists() {
        Repository::open(path).with_context(|| format!("failed to open {}", path.display()))?
    } else {
        Repository::init(path).with_context(|| format!("failed to init {}", path.display()))?
    };
    configure_cache_repository(&repository, remote_url, identity)?;
    Ok(repository)
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

/// Configures remote and author identity for one cache repository.
fn configure_cache_repository(
    repository: &Repository,
    remote_url: &str,
    identity: &ShareIdentity,
) -> Result<()> {
    match repository.find_remote(DEFAULT_REMOTE_NAME) {
        Ok(_) => repository
            .remote_set_url(DEFAULT_REMOTE_NAME, remote_url)
            .context("failed to update share cache remote URL")?,
        Err(_) => {
            repository
                .remote(DEFAULT_REMOTE_NAME, remote_url)
                .context("failed to add share cache remote")?;
        }
    }
    let mut config = repository
        .config()
        .context("failed to open share cache config")?;
    config
        .set_str(
            "user.name",
            identity.display_name.as_deref().unwrap_or("Darc Share"),
        )
        .context("failed to set share cache user.name")?;
    config
        .set_str(
            "user.email",
            identity
                .email
                .as_deref()
                .unwrap_or("darc-share@example.invalid"),
        )
        .context("failed to set share cache user.email")?;
    config
        .set_bool("commit.gpgsign", false)
        .context("failed to disable share cache commit signing")?;
    Ok(())
}

/// Fetches a branch and treats a missing remote branch as a non-fatal first push case.
fn fetch_branch_if_exists(
    repository: &Repository,
    remote_url: &str,
    git_branch: &str,
) -> Result<bool> {
    match fetch_branch(repository, remote_url, git_branch) {
        Ok(()) => {
            let remote_ref = format!("refs/remotes/{DEFAULT_REMOTE_NAME}/{git_branch}");
            if repository.find_reference(&remote_ref).is_err() {
                clear_share_branch_refs(repository, git_branch)?;
                return Ok(false);
            }
            Ok(true)
        }
        Err(error)
            if error
                .root_cause()
                .to_string()
                .contains("couldn't find remote ref") =>
        {
            clear_share_branch_refs(repository, git_branch)?;
            Ok(false)
        }
        Err(error) if error.root_cause().to_string().contains("not found") => {
            clear_share_branch_refs(repository, git_branch)?;
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// Deletes stale local cache refs for one missing remote share branch.
fn clear_share_branch_refs(repository: &Repository, git_branch: &str) -> Result<()> {
    for reference_name in [
        format!("refs/heads/{git_branch}"),
        format!("refs/remotes/{DEFAULT_REMOTE_NAME}/{git_branch}"),
    ] {
        if let Ok(mut reference) = repository.find_reference(&reference_name) {
            reference.delete().with_context(|| {
                format!("failed to delete stale share cache ref `{reference_name}`")
            })?;
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
    let repository = Repository::open(path)
        .with_context(|| format!("failed to open share cache repository {}", path.display()))?;
    clean_untracked_cache_worktree(&repository, path)
}

/// Removes worktree files that are not present in the checked-out Git tree.
fn clean_untracked_cache_worktree(repository: &Repository, cache_path: &Path) -> Result<()> {
    if !ensure_safe_existing_cache_dir(cache_path)? {
        return Ok(());
    }
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(true);
    let statuses = repository
        .statuses(Some(&mut options))
        .context("failed to read share cache status")?;
    let mut paths = statuses
        .iter()
        .filter_map(|entry| {
            let status = entry.status();
            if !(status.intersects(Status::WT_NEW) || status.intersects(Status::IGNORED)) {
                return None;
            }
            let path = entry.path()?;
            if path == ".git" || path.starts_with(".git/") {
                return None;
            }
            Some(PathBuf::from(path))
        })
        .collect::<Vec<_>>();
    paths.sort_by(|left, right| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| left.cmp(right))
    });
    for relative in paths {
        let target = cache_path.join(&relative);
        let checked = target.strip_prefix(cache_path).with_context(|| {
            format!(
                "share cache path {} is outside cache {}",
                target.display(),
                cache_path.display()
            )
        })?;
        if cache_relative_path_components(checked).is_none() {
            bail!(
                "share cache untracked path is not a safe relative path: {}",
                checked.display()
            );
        }
        remove_cache_worktree_entry(&target)?;
    }
    Ok(())
}

/// Removes one cache worktree entry without following symlinks.
fn remove_cache_worktree_entry(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_dir() && !file_type.is_symlink() {
                fs::remove_dir_all(path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            } else {
                fs::remove_file(path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

/// Builds the exact cache-relative file set that may be published.
fn allowed_share_cache_paths(
    artifact: &BuiltExportArtifact,
    retained_manifests: &[CachedManifest],
) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    insert_allowed_share_cache_path(&mut paths, &format!("{ARTIFACT_ROOT}/{PROJECT_FILE}"));
    insert_allowed_share_cache_path(
        &mut paths,
        &exporter_manifest_relative_path(&artifact.manifest.exporter),
    );
    for object_path in artifact.objects.keys() {
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
    if !ensure_safe_existing_cache_dir(path)? {
        return Ok(());
    }
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry.with_context(|| format!("failed to read {}", path.display()))?;
        if entry.file_name() == ".git" {
            continue;
        }
        clean_unexpected_share_cache_entry(path, &entry.path(), allowed_paths)?;
    }
    Ok(())
}

/// Removes one unexpected cache entry and prunes empty directories.
fn clean_unexpected_share_cache_entry(
    cache_path: &Path,
    path: &Path,
    allowed_paths: &BTreeSet<String>,
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
            clean_unexpected_share_cache_entry(cache_path, &entry.path(), allowed_paths)?;
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
    if cache_relative_path_key(relative)
        .as_ref()
        .is_some_and(|relative| allowed_paths.contains(relative))
    {
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
fn fetch_branch(repository: &Repository, remote_url: &str, git_branch: &str) -> Result<()> {
    let mut remote = repository
        .find_remote(DEFAULT_REMOTE_NAME)
        .or_else(|_| repository.remote_anonymous(remote_url))
        .context("failed to resolve share remote")?;
    let refspec =
        format!("refs/heads/{git_branch}:refs/remotes/{DEFAULT_REMOTE_NAME}/{git_branch}");
    let callbacks = remote_callbacks(repository)?;
    let mut fetch_options = FetchOptions::new();
    fetch_options.remote_callbacks(callbacks);
    fetch_options.prune(FetchPrune::On);
    remote
        .fetch(&[refspec.as_str()], Some(&mut fetch_options), None)
        .with_context(|| format!("failed to fetch share branch `{git_branch}`"))?;
    Ok(())
}

/// Checks out one local share branch from remote state when possible.
fn checkout_share_branch(repository: &Repository, git_branch: &str) -> Result<()> {
    let local_ref = format!("refs/heads/{git_branch}");
    let remote_ref = format!("refs/remotes/{DEFAULT_REMOTE_NAME}/{git_branch}");
    if let Ok(reference) = repository.find_reference(&remote_ref) {
        let oid = reference
            .target()
            .context("fetched share branch did not point at a commit")?;
        repository
            .reference(&local_ref, oid, true, "update share branch from remote")
            .with_context(|| format!("failed to update local branch `{git_branch}`"))?;
    }
    if repository.find_reference(&local_ref).is_ok() {
        repository
            .set_head(&local_ref)
            .with_context(|| format!("failed to set HEAD to `{git_branch}`"))?;
        let object = repository
            .head()
            .context("failed to read share cache HEAD")?
            .peel(git2::ObjectType::Commit)
            .context("failed to peel share cache HEAD")?;
        repository
            .reset(&object, ResetType::Hard, None)
            .context("failed to reset share cache branch")?;
    } else {
        repository
            .set_head(&local_ref)
            .with_context(|| format!("failed to set unborn HEAD to `{git_branch}`"))?;
    }
    Ok(())
}

/// Commits the current cache repository workdir.
fn commit_cache_repository(
    repository: &Repository,
    identity: &ShareIdentity,
    git_branch: &str,
) -> Result<String> {
    let mut index = repository
        .index()
        .context("failed to open share cache index")?;
    index
        .remove_all(["*"].iter(), None)
        .context("failed to stage removed share artifacts")?;
    index
        .add_all([ARTIFACT_ROOT].iter(), IndexAddOption::DEFAULT, None)
        .context("failed to add share artifacts to index")?;
    index.write().context("failed to write share cache index")?;
    let tree_oid = index.write_tree().context("failed to write share tree")?;
    let tree = repository
        .find_tree(tree_oid)
        .context("failed to find share tree")?;
    let signature = repository
        .signature()
        .or_else(|_| {
            git2::Signature::now(
                identity.display_name.as_deref().unwrap_or("Darc Share"),
                identity
                    .email
                    .as_deref()
                    .unwrap_or("darc-share@example.invalid"),
            )
        })
        .context("failed to build share commit signature")?;
    let parent = repository
        .find_branch(git_branch, BranchType::Local)
        .ok()
        .and_then(|branch| branch.get().target())
        .and_then(|oid| repository.find_commit(oid).ok());
    if let Some(parent) = parent.as_ref()
        && parent.tree_id() == tree_oid
    {
        return Ok(parent.id().to_string());
    }
    let message = format!("chore(share): update {git_branch}");
    let oid = if let Some(parent) = parent.as_ref() {
        repository.commit(
            Some(&format!("refs/heads/{git_branch}")),
            &signature,
            &signature,
            &message,
            &tree,
            &[parent],
        )
    } else {
        repository.commit(
            Some(&format!("refs/heads/{git_branch}")),
            &signature,
            &signature,
            &message,
            &tree,
            &[],
        )
    }
    .context("failed to commit share artifacts")?;
    Ok(oid.to_string())
}

/// Pushes one local share branch to the configured remote.
fn push_branch(repository: &Repository, remote_url: &str, git_branch: &str) -> Result<()> {
    let mut remote = repository
        .find_remote(DEFAULT_REMOTE_NAME)
        .or_else(|_| repository.remote_anonymous(remote_url))
        .context("failed to resolve share remote")?;
    let refspec = format!("refs/heads/{git_branch}:refs/heads/{git_branch}");
    let callbacks = remote_callbacks(repository)?;
    let mut push_options = PushOptions::new();
    push_options.remote_callbacks(callbacks);
    remote
        .push(&[refspec.as_str()], Some(&mut push_options))
        .with_context(|| format!("failed to push share branch `{git_branch}`"))?;
    Ok(())
}

/// Builds remote callbacks that use standard Git credential sources.
fn remote_callbacks(repository: &Repository) -> Result<RemoteCallbacks<'_>> {
    let config = repository.config().context("failed to open Git config")?;
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(move |url, username_from_url, allowed| {
        if allowed.is_user_pass_plaintext()
            && let Ok(credential) = Cred::credential_helper(&config, url, username_from_url)
        {
            return Ok(credential);
        }
        if allowed.is_ssh_key()
            && let Some(username) = username_from_url.or(Some("git"))
            && let Ok(credential) = Cred::ssh_key_from_agent(username)
        {
            return Ok(credential);
        }
        if allowed.is_username()
            && let Some(username) = username_from_url.or(Some("git"))
        {
            return Cred::username(username);
        }
        Cred::default()
    });
    callbacks.push_update_reference(|reference, status| {
        if let Some(status) = status {
            return Err(git2::Error::from_str(&format!(
                "push rejected for {reference}: {status}"
            )));
        }
        Ok(())
    });
    Ok(callbacks)
}

/// Reads one existing encrypted object that can be reused by content hash.
fn read_existing_object(cache_path: &Path, object_path: &str) -> Result<Option<Vec<u8>>> {
    let path = manifest_artifact_path(cache_path, object_path)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => read_regular_file(&path, MAX_SHARE_OBJECT_BYTES).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
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
    root.join(SHARE_CACHE_DIR)
        .join(sha256_hex(format!("{remote_url}\n{git_branch}").as_bytes()))
}

/// Builds the stored provenance key for one imported remote branch.
fn share_origin_remote(remote_url: &str, git_branch: &str) -> String {
    let canonical_url =
        normalize_git_url(remote_url).unwrap_or_else(|_| sanitize_git_url_for_display(remote_url));
    let identity = sha256_hex(format!("{canonical_url}\n{git_branch}").as_bytes());
    format!("remote:{}:{git_branch}", &identity[..16])
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
    let relative = Path::new(relative_path);
    if relative.is_absolute() {
        bail!("share artifact path must be relative");
    }
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            bail!("share artifact path contains unsafe path components");
        }
    }
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
    let expected_prefix = format!("{ARTIFACT_ROOT}/objects/");
    if !object_path.starts_with(&expected_prefix) || !object_path.ends_with(".age") {
        bail!("share object path is outside the supported object namespace");
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
    Ok(cache_path.join(relative))
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
            normalize_git_url("https://github.com/Example/Darc.git/").unwrap(),
            "https://github.com/Example/Darc"
        );
        assert_eq!(
            normalize_git_url("https://user:token@github.com/Example/Darc.git/").unwrap(),
            "https://github.com/Example/Darc"
        );
        assert_eq!(
            normalize_git_url("ssh://deploy@github.com/Team/App.git").unwrap(),
            "https://github.com/Team/App"
        );
        assert!(normalize_git_url("file:///Users/alice/repo.git").is_err());
        assert!(normalize_git_url("/Users/alice/repo.git").is_err());
        assert_eq!(
            sanitize_git_url_for_display("https://user:token@github.com/Example/Darc.git"),
            "https://github.com/Example/Darc.git"
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

    #[test]
    fn recipient_set_changes_encrypt_to_a_new_object_path() {
        let workspace = unique_test_dir("share-recipient-fingerprint");
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
        let first = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            decryption_identity: &age_identity,
            signing_key: &signing_key,
            branch: "team",
            turns: turns.clone(),
            cache_path: &cache,
        })
        .unwrap();
        let settings = ShareSettings {
            remotes: Vec::new(),
            recipients: vec![ShareRecipient {
                recipient: Identity::generate().to_public().to_string(),
            }],
        };
        let second = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &settings,
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            decryption_identity: &age_identity,
            signing_key: &signing_key,
            branch: "team",
            turns,
            cache_path: &cache,
        })
        .unwrap();

        assert_ne!(
            first.manifest.turns[0].object_path,
            second.manifest.turns[0].object_path
        );
        assert!(
            !first
                .objects
                .contains_key(&second.manifest.turns[0].object_path)
        );
    }

    #[test]
    fn corrupted_cached_object_is_not_reused() {
        let workspace = unique_test_dir("share-corrupted-cache");
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
        let first = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            decryption_identity: &age_identity,
            signing_key: &signing_key,
            branch: "team",
            turns: turns.clone(),
            cache_path: &cache,
        })
        .unwrap();
        let object_path = first.manifest.turns[0].object_path.clone();
        let target = cache.join(&object_path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"not an age payload").unwrap();

        let second = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            decryption_identity: &age_identity,
            signing_key: &signing_key,
            branch: "team",
            turns,
            cache_path: &cache,
        })
        .unwrap();

        assert_ne!(second.objects[&object_path], b"not an age payload");
        let plaintext = decrypt_payload(&second.objects[&object_path], &age_identity).unwrap();
        assert_eq!(sha256_hex(&plaintext), first.manifest.turns[0].payload_hash);
    }

    #[test]
    fn resolve_remote_sanitizes_credentialed_display_url() {
        let workspace = unique_test_dir("share-sanitized-remote-report");
        let context = ShareProjectContext {
            root: workspace.clone(),
            index_db_path: workspace.join("index.sqlite"),
            project_id: "repo".to_owned(),
            project_name: "repo".to_owned(),
            local_path: workspace.join("repo"),
            git_upstream: Some("https://example.invalid/team/repo.git".to_owned()),
        };
        let settings = ShareSettings {
            remotes: vec![ShareRemote {
                name: "team".to_owned(),
                url: "https://user:token@example.invalid/team/share.git".to_owned(),
            }],
            recipients: Vec::new(),
        };

        let remote = resolve_remote(&context, &settings, Some("team")).unwrap();

        assert_eq!(
            remote.url,
            "https://user:token@example.invalid/team/share.git"
        );
        assert_eq!(remote.display_url, "https://example.invalid/team/share.git");
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
        };
        let sync = write_test_sync_object(
            &cache,
            &identity,
            &signing_key,
            &exporter,
            "git:https://example.invalid/team/repo",
            vec![SyncTurnEntry {
                provider: turn.provider,
                session_id: turn.session_id.clone(),
                turn_ordinal: turn.turn_ordinal,
            }],
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
            decryption_identity: &source_age_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
            cache_path: &cache,
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
            decryption_identity: &source_age_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
            cache_path: &cache,
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
            decryption_identity: &source_age_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
            cache_path: &cache,
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
            decryption_identity: &source_age_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
            cache_path: &cache,
        })
        .unwrap();
        write_export_artifact(&cache, &artifact).unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let attacker = Identity::generate();
        let attacker_signing_key = test_signing_key(&attacker);
        let mut forged_sync = EncryptedSyncPayload {
            schema: SYNC_PAYLOAD_SCHEMA.to_owned(),
            version: 1,
            project_key: "git:https://example.invalid/team/repo".to_owned(),
            exporter: source_identity,
            signature: None,
            turns: manifest
                .turns
                .iter()
                .map(|entry| SyncTurnEntry {
                    provider: entry.provider,
                    session_id: entry.session_id.clone(),
                    turn_ordinal: entry.turn_ordinal,
                })
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
            decryption_identity: &source_age_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
            cache_path: &cache,
        })
        .unwrap();
        write_export_artifact(&cache, &artifact).unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let original_object_path = manifest.turns[0].object_path.clone();
        let original_ciphertext = fs::read(cache.join(&original_object_path)).unwrap();
        let original_plaintext = decrypt_payload(&original_ciphertext, &target_identity).unwrap();
        let mut forged_payload: EncryptedTurnPayload =
            serde_json::from_slice(&original_plaintext).unwrap();
        forged_payload.turn.user_message = "forged task".to_owned();
        let forged_plaintext = serde_json::to_vec(&forged_payload).unwrap();
        let forged_object_path = format!(
            "{ARTIFACT_ROOT}/objects/forged-turn-{}.age",
            &sha256_hex(&forged_plaintext)[..16]
        );
        let forged_target = cache.join(&forged_object_path);
        fs::create_dir_all(forged_target.parent().unwrap()).unwrap();
        fs::write(
            &forged_target,
            encrypt_payload(&forged_plaintext, &[target_identity.to_public()]).unwrap(),
        )
        .unwrap();
        manifest.turns[0].payload_hash = sha256_hex(&forged_plaintext);
        manifest.turns[0].object_path = forged_object_path;
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
            report.warnings[0].contains("signature"),
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
            source_signing_key: _,
            artifact,
        } = build_single_turn_test_artifact("share-unsupported-payload-version");
        write_export_artifact(&cache, &artifact).unwrap();
        let manifest_path = cache.join(exporter_manifest_relative_path(&source_identity));
        let mut manifest = read_json_file::<ManifestArtifact>(&manifest_path).unwrap();
        let sync_ciphertext = fs::read(cache.join(&manifest.sync.object_path)).unwrap();
        let sync_plaintext = decrypt_payload(&sync_ciphertext, &target_identity).unwrap();
        let mut sync_payload: EncryptedSyncPayload =
            serde_json::from_slice(&sync_plaintext).unwrap();
        sync_payload.version = 2;
        let sync_plaintext = serde_json::to_vec(&sync_payload).unwrap();
        let sync_object_path = format!(
            "{ARTIFACT_ROOT}/objects/sync-v2-{}.age",
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
        let turn_ciphertext = fs::read(cache.join(&manifest.turns[0].object_path)).unwrap();
        let turn_plaintext = decrypt_payload(&turn_ciphertext, &target_identity).unwrap();
        let mut turn_payload: EncryptedTurnPayload =
            serde_json::from_slice(&turn_plaintext).unwrap();
        turn_payload.version = 2;
        let turn_plaintext = serde_json::to_vec(&turn_payload).unwrap();
        let turn_object_path = format!(
            "{ARTIFACT_ROOT}/objects/turn-v2-{}.age",
            &sha256_hex(&turn_plaintext)[..16]
        );
        let turn_target = cache.join(&turn_object_path);
        fs::create_dir_all(turn_target.parent().unwrap()).unwrap();
        fs::write(
            &turn_target,
            encrypt_payload(&turn_plaintext, &[target_identity.to_public()]).unwrap(),
        )
        .unwrap();
        manifest.turns[0].payload_hash = sha256_hex(&turn_plaintext);
        manifest.turns[0].object_path = turn_object_path;
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
            report.warnings[0].contains("share payload version"),
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
        let turn_ciphertext = fs::read(cache.join(&original_turn.object_path)).unwrap();
        let turn_plaintext = decrypt_payload(&turn_ciphertext, &target_identity).unwrap();
        let mut extra_payload: EncryptedTurnPayload =
            serde_json::from_slice(&turn_plaintext).unwrap();
        extra_payload.turn.turn_ordinal = 99;
        extra_payload.turn.turn_id = Some("turn-extra".to_owned());
        extra_payload.turn.started_at = "2026-05-15T23:59:59Z".to_owned();
        sign_turn_payload(&mut extra_payload, &source_signing_key).unwrap();
        let extra_plaintext = serde_json::to_vec(&extra_payload).unwrap();
        let extra_object_path = format!(
            "{ARTIFACT_ROOT}/objects/extra-turn-{}.age",
            &sha256_hex(&extra_plaintext)[..16]
        );
        let extra_target = cache.join(&extra_object_path);
        fs::create_dir_all(extra_target.parent().unwrap()).unwrap();
        fs::write(
            &extra_target,
            encrypt_payload(&extra_plaintext, &[target_identity.to_public()]).unwrap(),
        )
        .unwrap();
        manifest.turns.push(TurnManifestEntry {
            provider: original_turn.provider,
            session_id: original_turn.session_id,
            turn_ordinal: extra_payload.turn.turn_ordinal,
            started_at: extra_payload.turn.started_at,
            payload_hash: sha256_hex(&extra_plaintext),
            object_path: extra_object_path,
        });
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

        assert_eq!(report.imported_turn_count, 1);
        assert_eq!(report.skipped_turn_count, 1);
        assert_eq!(report.warning_count, 1);
        assert!(
            report.warnings[0].contains("not authenticated by sync payload"),
            "warning should reject unauthenticated manifest turn: {:?}",
            report.warnings
        );
        assert_eq!(turn_count, 1);
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
        };
        let sync = write_test_sync_object(
            &cache,
            &identity,
            &signing_key,
            &exporter,
            "git:https://example.invalid/team/repo",
            vec![SyncTurnEntry {
                provider: turn.provider,
                session_id: turn.session_id.clone(),
                turn_ordinal: turn.turn_ordinal,
            }],
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
            report.warnings[0].contains("unsafe path components"),
            "warning should explain path validation failure: {:?}",
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
        };
        let sync = write_test_sync_object(
            &cache,
            &identity,
            &signing_key,
            &exporter,
            "git:https://example.invalid/team/repo",
            vec![SyncTurnEntry {
                provider: turn.provider,
                session_id: turn.session_id.clone(),
                turn_ordinal: turn.turn_ordinal,
            }],
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
            decryption_identity: &age_identity,
            signing_key: &signing_key,
            branch: "team",
            turns,
            cache_path: &cache,
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
    fn push_and_pull_round_trip_rebinds_refreshes_and_prunes_sessions() {
        let workspace = unique_test_dir("share-round-trip");
        let remote_path = workspace.join("share.git");
        Repository::init_bare(&remote_path).unwrap();
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
    fn fetch_cleans_untracked_cache_artifacts_before_merge() {
        let workspace = unique_test_dir("share-fetch-cleans-untracked");
        let remote_path = workspace.join("share.git");
        Repository::init_bare(&remote_path).unwrap();
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
        assert!(injected_manifest.exists());
        assert!(injected_object.exists());

        fetch_share_branch(&target_context, &settings, "team", Some("share")).unwrap();

        assert!(!injected_manifest.exists());
        assert!(!injected_object.exists());
        let merge = merge_share_branch(&target_context, &settings, "team", Some("share")).unwrap();
        assert_eq!(merge.imported_turn_count, 1);
        assert_eq!(merge.warning_count, 0);
    }

    #[test]
    fn push_drops_unexpected_cache_files_from_branch_tip() {
        let workspace = unique_test_dir("share-artifact-only-tip");
        let remote_path = workspace.join("share.git");
        Repository::init_bare(&remote_path).unwrap();
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
    }

    #[test]
    fn push_preserves_same_email_exporters_at_branch_tip() {
        let workspace = unique_test_dir("share-same-email-author-tip");
        let remote_path = workspace.join("share.git");
        Repository::init_bare(&remote_path).unwrap();
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
    fn missing_remote_branch_drops_stale_cache_parent() {
        let workspace = unique_test_dir("share-missing-remote-branch");
        let remote_path = workspace.join("share.git");
        Repository::init_bare(&remote_path).unwrap();
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
        let remote = Repository::open_bare(&remote_path).unwrap();
        remote
            .find_reference("refs/heads/darc/team")
            .unwrap()
            .delete()
            .unwrap();
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
            decryption_identity: &source_age_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
            cache_path: &cache,
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
            decryption_identity: &source_age_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns: turns.clone(),
            cache_path: &cache,
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
            decryption_identity: &source_age_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns: turns.into_iter().take(1).collect(),
            cache_path: &cache,
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
            decryption_identity: &source_age_identity,
            signing_key: &source_signing_key,
            branch: "team",
            turns,
            cache_path: &cache,
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

    fn init_test_git_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        let repository = Repository::init(path).unwrap();
        let mut config = repository.config().unwrap();
        config.set_str("user.name", "Synthetic User").unwrap();
        config
            .set_str("user.email", "synthetic@example.invalid")
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

    fn write_test_sync_object(
        cache: &Path,
        identity: &Identity,
        signing_key: &SigningKey,
        exporter: &ShareIdentity,
        project_key: &str,
        turns: Vec<SyncTurnEntry>,
    ) -> SyncManifestEntry {
        let mut payload = EncryptedSyncPayload {
            schema: SYNC_PAYLOAD_SCHEMA.to_owned(),
            version: 1,
            project_key: project_key.to_owned(),
            exporter: exporter.clone(),
            signature: None,
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
        let repository = Repository::open_bare(remote_path).unwrap();
        let oid = repository
            .find_reference(&format!("refs/heads/{git_branch}"))
            .unwrap()
            .target()
            .unwrap();
        repository.find_commit(oid).unwrap().parent_count()
    }

    fn remote_tip_blob_paths(remote_path: &Path, git_branch: &str) -> Vec<String> {
        let repository = Repository::open_bare(remote_path).unwrap();
        let oid = repository
            .find_reference(&format!("refs/heads/{git_branch}"))
            .unwrap()
            .target()
            .unwrap();
        let commit = repository.find_commit(oid).unwrap();
        let tree = commit.tree().unwrap();
        let mut paths = Vec::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob)
                && let Some(name) = entry.name()
            {
                paths.push(format!("{root}{name}"));
            }
            git2::TreeWalkResult::Ok
        })
        .unwrap();
        paths.sort();
        paths
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
