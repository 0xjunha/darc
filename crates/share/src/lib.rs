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
use git2::{
    BranchType, Cred, FetchOptions, IndexAddOption, PushOptions, RemoteCallbacks, Repository,
    ResetType,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const ARTIFACT_ROOT: &str = "darc-share/v1";
const PROJECT_SCHEMA: &str = "darc.share.project.v1";
const MANIFEST_SCHEMA: &str = "darc.share.manifest.v1";
const TURN_PAYLOAD_SCHEMA: &str = "darc.share.turn.v1";
const SYNC_PAYLOAD_SCHEMA: &str = "darc.share.sync.v1";
const KEY_FILE_NAME: &str = "share.agekey";
const SHARE_CACHE_DIR: &str = "share-cache";
const DEFAULT_REMOTE_NAME: &str = "origin";
const MAX_SHARE_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SHARE_OBJECT_BYTES: u64 = 256 * 1024 * 1024;

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
    turn: ShareTurnExport,
}

/// Stores one encrypted export manifest used to authenticate pruning inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EncryptedSyncPayload {
    schema: String,
    version: u32,
    project_key: String,
    exporter: ShareIdentity,
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

/// Builds the local share identity from Git config and the Darc public key.
pub fn local_share_identity(context: &ShareProjectContext) -> Result<ShareIdentity> {
    let key = ensure_share_key(&context.root)?;
    let repository = Repository::discover(&context.local_path).with_context(|| {
        format!(
            "failed to discover Git repository from {}",
            context.local_path.display()
        )
    })?;
    let config = repository.config().context("failed to read Git config")?;
    let display_name = config.get_string("user.name").ok();
    let email = config.get_string("user.email").ok();
    let user_id = derive_user_id(display_name.as_deref(), email.as_deref(), &key.public_key);
    Ok(ShareIdentity {
        user_id,
        display_name,
        email,
        public_key: key.public_key,
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
    let connection = open_index_database_writer(&context.index_db_path)?;
    let turns = query_share_export_turns(&connection, &context.project_id)?;
    let cache_path = cache_repo_path(&context.root, &remote.url, &git_branch);
    let repository = prepare_cache_repository(&cache_path, &remote.url, &identity)?;
    fetch_branch_if_exists(&repository, &remote.url, &git_branch)?;
    checkout_share_branch(&repository, &git_branch)?;
    let existing_objects = read_existing_objects(&cache_path)?;
    clear_cache_workdir(&cache_path)?;
    let artifact = build_export_artifact(ExportBuildRequest {
        context,
        settings,
        project_key: &project_key,
        identity: &identity,
        decryption_identity: &decryption_identity,
        branch,
        turns,
        existing_objects,
    })?;
    write_export_artifact(&cache_path, &artifact)?;
    let commit_id = commit_cache_repository(&repository, &identity, &git_branch)?;
    push_branch(&repository, &remote.url, &git_branch)?;
    Ok(SharePushReport {
        branch: branch.to_owned(),
        git_branch,
        remote_name: remote.name,
        remote_url: remote.url,
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
    Ok(ShareFetchReport {
        branch: branch.to_owned(),
        git_branch,
        remote_name: remote.name,
        remote_url: remote.url,
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
    import_from_cache(
        context,
        branch,
        &git_branch,
        &remote.name,
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
}

/// Stores the inputs needed to build one share export artifact.
struct ExportBuildRequest<'a> {
    context: &'a ShareProjectContext,
    settings: &'a ShareSettings,
    project_key: &'a str,
    identity: &'a ShareIdentity,
    decryption_identity: &'a Identity,
    branch: &'a str,
    turns: Vec<ShareTurnExport>,
    existing_objects: BTreeMap<String, Vec<u8>>,
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
    let mut manifest_turns = Vec::with_capacity(request.turns.len());
    let mut session_ids = BTreeSet::new();
    for turn in request.turns {
        session_ids.insert((turn.session.provider, turn.session.session_id.clone()));
        let payload = EncryptedTurnPayload {
            schema: TURN_PAYLOAD_SCHEMA.to_owned(),
            version: 1,
            project_key: request.project_key.to_owned(),
            exporter: request.identity.clone(),
            turn,
        };
        let plaintext =
            serde_json::to_vec(&payload).context("failed to serialize share payload")?;
        let payload_hash = sha256_hex(&plaintext);
        let object_path =
            format!("{ARTIFACT_ROOT}/objects/{recipient_fingerprint}-{payload_hash}.age");
        let encrypted = reusable_existing_object(
            request.existing_objects.get(&object_path),
            request.decryption_identity,
            &plaintext,
            &recipients,
        )?;
        objects.insert(object_path.clone(), encrypted);
        manifest_turns.push(TurnManifestEntry {
            provider: payload.turn.session.provider,
            session_id: payload.turn.session.session_id,
            turn_ordinal: payload.turn.turn_ordinal,
            started_at: payload.turn.started_at,
            payload_hash,
            object_path,
        });
    }
    let sync_payload = EncryptedSyncPayload {
        schema: SYNC_PAYLOAD_SCHEMA.to_owned(),
        version: 1,
        project_key: request.project_key.to_owned(),
        exporter: request.identity.clone(),
        turns: manifest_turns
            .iter()
            .map(|entry| SyncTurnEntry {
                provider: entry.provider,
                session_id: entry.session_id.clone(),
                turn_ordinal: entry.turn_ordinal,
            })
            .collect(),
    };
    let sync_plaintext =
        serde_json::to_vec(&sync_payload).context("failed to serialize share sync payload")?;
    let sync_payload_hash = sha256_hex(&sync_plaintext);
    let sync_object_path =
        format!("{ARTIFACT_ROOT}/objects/sync-{recipient_fingerprint}-{sync_payload_hash}.age");
    let sync_encrypted = reusable_existing_object(
        request.existing_objects.get(&sync_object_path),
        request.decryption_identity,
        &sync_plaintext,
        &recipients,
    )?;
    objects.insert(sync_object_path.clone(), sync_encrypted);
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

/// Writes all share artifacts into a cache repository workdir.
fn write_export_artifact(path: &Path, artifact: &BuiltExportArtifact) -> Result<()> {
    write_json_file(
        &path.join(ARTIFACT_ROOT).join("project.json"),
        &artifact.project,
    )?;
    write_json_file(
        &path.join(ARTIFACT_ROOT).join("manifest.json"),
        &artifact.manifest,
    )?;
    for (relative, content) in &artifact.objects {
        let target = path.join(relative);
        let parent = target
            .parent()
            .context("share object path is missing a parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        fs::write(&target, content)
            .with_context(|| format!("failed to write {}", target.display()))?;
    }
    Ok(())
}

/// Imports all valid encrypted payloads from one cache workdir.
fn import_from_cache(
    context: &ShareProjectContext,
    branch: &str,
    git_branch: &str,
    remote_name: &str,
    expected_project_key: &str,
    cache_path: &Path,
) -> Result<ShareMergeReport> {
    let manifest_path = cache_path.join(ARTIFACT_ROOT).join("manifest.json");
    let manifest = read_json_file::<ManifestArtifact>(&manifest_path)?;
    let mut warnings = Vec::new();
    if manifest.schema != MANIFEST_SCHEMA {
        bail!(
            "unsupported Darc share manifest schema `{}`",
            manifest.schema
        );
    }
    if manifest.project_key != expected_project_key {
        bail!(
            "share branch project key `{}` does not match active project key `{}`",
            manifest.project_key,
            expected_project_key
        );
    }
    let identity_key = ensure_share_key(&context.root)?;
    let identity = read_share_identity_key(&identity_key.key_path)?;
    let sync_payload = read_sync_payload(cache_path, &manifest, expected_project_key, &identity)?;
    let mut connection = open_index_database_writer(&context.index_db_path)?;
    let origin_remote = share_origin_remote(remote_name, git_branch);
    let keep_turns = sync_payload
        .turns
        .iter()
        .map(|entry| (entry.provider, entry.session_id.clone(), entry.turn_ordinal))
        .collect::<BTreeSet<_>>();
    let authenticated_turns = sync_payload.turns.iter().cloned().collect::<BTreeSet<_>>();
    let mut imported_turn_count = 0_u64;
    let mut skipped_turn_count = 0_u64;
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
        let import_context = ImportEntryContext {
            expected_project_key,
            project_id: &context.project_id,
            origin_remote: &origin_remote,
            expected_exporter: &sync_payload.exporter,
            identity: &identity,
            cache_path,
        };
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
    if payload.project_key != expected_project_key {
        bail!("share sync payload project key does not match active project");
    }
    if payload.exporter != manifest.exporter {
        bail!("share sync payload exporter does not match manifest exporter");
    }
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
    if payload.project_key != context.expected_project_key {
        bail!("share payload project key does not match active project");
    }
    if payload.exporter != *context.expected_exporter {
        bail!("share payload exporter does not match sync payload exporter");
    }
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
            url: remote.url.clone(),
        });
    }
    if let Some(url) = context.git_upstream.clone() {
        return Ok(ResolvedRemote {
            name: DEFAULT_REMOTE_NAME.to_owned(),
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
    Ok(format!("git:{}", normalize_git_url(&url)))
}

/// Normalizes one Git URL enough for Darc project matching.
fn normalize_git_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/').trim_end_matches(".git");
    if let Some(normalized) = normalize_scp_like_git_url(trimmed) {
        return normalized;
    }
    if let Some(normalized) = normalize_scheme_git_url(trimmed, "ssh://", "https") {
        return normalized;
    }
    if let Some(normalized) = normalize_scheme_git_url(trimmed, "https://", "https") {
        return normalized;
    }
    if let Some(normalized) = normalize_scheme_git_url(trimmed, "http://", "http") {
        return normalized;
    }
    trimmed.to_owned()
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

/// Reads and parses one age identity file.
fn read_share_identity_key(path: &Path) -> Result<Identity> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Identity::from_str(content.trim()).map_err(|error| anyhow::anyhow!("{error}"))
}

/// Writes one age identity file with private-key permissions on Unix.
fn write_share_identity_key(path: &Path, content: &str) -> Result<()> {
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

/// Restricts one age identity file to the current user on Unix.
fn harden_share_key_permissions(path: &Path) -> Result<()> {
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
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    let repository = if path.join(".git").exists() {
        Repository::open(path).with_context(|| format!("failed to open {}", path.display()))?
    } else {
        Repository::init(path).with_context(|| format!("failed to init {}", path.display()))?
    };
    configure_cache_repository(&repository, remote_url, identity)?;
    Ok(repository)
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
) -> Result<()> {
    match fetch_branch(repository, remote_url, git_branch) {
        Ok(()) => Ok(()),
        Err(error)
            if error
                .root_cause()
                .to_string()
                .contains("couldn't find remote ref") =>
        {
            clear_share_branch_refs(repository, git_branch)?;
            Ok(())
        }
        Err(error) if error.root_cause().to_string().contains("not found") => {
            clear_share_branch_refs(repository, git_branch)?;
            Ok(())
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
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
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

/// Reads encrypted object files that can be reused by content hash.
fn read_existing_objects(cache_path: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let root = cache_path.join(ARTIFACT_ROOT).join("objects");
    let mut objects = BTreeMap::new();
    if !root.exists() {
        return Ok(objects);
    }
    for entry in
        fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read {}", root.display()))?;
        if !entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_file()
        {
            continue;
        }
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let relative = format!("{ARTIFACT_ROOT}/objects/{file_name}");
        let content = read_regular_file(&path, MAX_SHARE_OBJECT_BYTES)?;
        objects.insert(relative, content);
    }
    Ok(objects)
}

/// Clears every non-Git entry in one cache workdir.
fn clear_cache_workdir(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry.with_context(|| format!("failed to read {}", path.display()))?;
        if entry.file_name() == ".git" {
            continue;
        }
        let entry_path = entry.path();
        if entry_path.is_dir() {
            fs::remove_dir_all(&entry_path)
        } else {
            fs::remove_file(&entry_path)
        }
        .with_context(|| format!("failed to remove {}", entry_path.display()))?;
    }
    Ok(())
}

/// Returns one cache repository path for a remote URL and branch.
fn cache_repo_path(root: &Path, remote_url: &str, git_branch: &str) -> PathBuf {
    root.join(SHARE_CACHE_DIR)
        .join(sha256_hex(format!("{remote_url}\n{git_branch}").as_bytes()))
}

/// Builds the stored provenance key for one imported remote branch.
fn share_origin_remote(remote_name: &str, git_branch: &str) -> String {
    format!("{remote_name}:{git_branch}")
}

/// Resolves and validates one manifest object path below the cache workdir.
fn manifest_object_path(cache_path: &Path, entry: &TurnManifestEntry) -> Result<PathBuf> {
    manifest_artifact_path(cache_path, &entry.object_path)
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

/// Writes one pretty JSON file.
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
    if branch.contains("..") || branch.contains("@{") || branch.ends_with(".lock") {
        bail!("share branch name is not a safe Git branch component");
    }
    if !branch
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
    {
        bail!("share branch name may only contain ASCII letters, digits, `/`, `-`, `_`, or `.`");
    }
    Ok(())
}

/// Derives one stable share user id.
fn derive_user_id(name: Option<&str>, email: Option<&str>, public_key: &str) -> String {
    let basis = email
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("email:{}", value.trim().to_ascii_lowercase()))
        .unwrap_or_else(|| {
            format!(
                "name:{}\nkey:{}",
                name.unwrap_or("unknown").trim(),
                public_key.trim()
            )
        });
    format!("usr-{}", &sha256_hex(basis.as_bytes())[..16])
}

/// Returns one lowercase hex SHA-256 digest.
fn sha256_hex(input: &[u8]) -> String {
    let digest = Sha256::digest(input);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
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
    }

    #[test]
    fn normalizes_common_git_urls() {
        assert_eq!(
            normalize_git_url("git@github.com:Example/Darc.git"),
            "https://github.com/Example/Darc"
        );
        assert_eq!(
            normalize_git_url("https://github.com/Example/Darc.git/"),
            "https://github.com/Example/Darc"
        );
        assert_eq!(
            normalize_git_url("https://user:token@github.com/Example/Darc.git/"),
            "https://github.com/Example/Darc"
        );
        assert_eq!(
            normalize_git_url("ssh://deploy@github.com/Team/App.git"),
            "https://github.com/Team/App"
        );
    }

    #[test]
    fn derives_email_based_user_id_case_insensitively() {
        let left = derive_user_id(Some("A"), Some("USER@example.com"), "age1abc");
        let right = derive_user_id(Some("B"), Some("user@example.com"), "age1def");
        assert_eq!(left, right);
    }

    #[test]
    fn encrypts_and_decrypts_payload() {
        let identity = Identity::generate();
        let recipient = identity.to_public();
        let encrypted = encrypt_payload(b"payload", &[recipient]).unwrap();
        let decrypted = decrypt_payload(&encrypted, &identity).unwrap();
        assert_eq!(decrypted, b"payload");
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
        let identity = ShareIdentity {
            user_id: "usr-synthetic".to_owned(),
            display_name: Some("Synthetic User".to_owned()),
            email: Some("synthetic@example.invalid".to_owned()),
            public_key: age_identity.to_public().to_string(),
        };
        let first = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            decryption_identity: &age_identity,
            branch: "team",
            turns: turns.clone(),
            existing_objects: BTreeMap::new(),
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
            branch: "team",
            turns,
            existing_objects: first.objects.clone(),
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
        let identity = test_share_identity(&age_identity.to_public().to_string());
        let first = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            decryption_identity: &age_identity,
            branch: "team",
            turns: turns.clone(),
            existing_objects: BTreeMap::new(),
        })
        .unwrap();
        let object_path = first.manifest.turns[0].object_path.clone();
        let mut corrupted = first.objects.clone();
        corrupted.insert(object_path.clone(), b"not an age payload".to_vec());

        let second = build_export_artifact(ExportBuildRequest {
            context: &context,
            settings: &ShareSettings::default(),
            project_key: "git:https://example.invalid/team/repo",
            identity: &identity,
            decryption_identity: &age_identity,
            branch: "team",
            turns,
            existing_objects: corrupted,
        })
        .unwrap();

        assert_ne!(second.objects[&object_path], b"not an age payload");
        let plaintext = decrypt_payload(&second.objects[&object_path], &age_identity).unwrap();
        assert_eq!(sha256_hex(&plaintext), first.manifest.turns[0].payload_hash);
    }

    #[test]
    fn merge_skips_malformed_payload_with_warning() {
        let root = unique_test_dir("share-malformed-payload");
        let cache = root.join("cache");
        let key = ensure_share_key(&root).unwrap();
        let identity = read_share_identity_key(&key.key_path).unwrap();
        let exporter = test_share_identity(&key.public_key);
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
    fn merge_skips_unsafe_manifest_object_paths_with_warning() {
        let root = unique_test_dir("share-unsafe-object-path");
        let cache = root.join("cache");
        let key = ensure_share_key(&root).unwrap();
        let identity = read_share_identity_key(&key.key_path).unwrap();
        let exporter = test_share_identity(&key.public_key);
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
    fn merge_rejects_symlinked_artifacts() {
        use std::os::unix::fs::symlink;

        let root = unique_test_dir("share-symlink-artifacts");
        let cache = root.join("cache");
        let key = ensure_share_key(&root).unwrap();
        let identity = read_share_identity_key(&key.key_path).unwrap();
        let exporter = test_share_identity(&key.public_key);
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

        let error = import_from_cache(
            &context,
            "team",
            "darc/team",
            "origin",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("symlink"),
            "error should reject symlinked manifest: {error:#}"
        );
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
        let source_context = ShareProjectContext {
            root: source_root.clone(),
            index_db_path: source_root.join("index.sqlite"),
            project_id: "source-repo".to_owned(),
            project_name: "source-repo".to_owned(),
            local_path: source_repo,
            git_upstream: Some(remote_url.clone()),
        };
        let target_context = ShareProjectContext {
            root: target_root.clone(),
            index_db_path: target_root.join("index.sqlite"),
            project_id: "target-repo".to_owned(),
            project_name: "target-repo".to_owned(),
            local_path: target_repo,
            git_upstream: Some(remote_url.clone()),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();
        let settings = ShareSettings {
            remotes: Vec::new(),
            recipients: vec![ShareRecipient {
                recipient: target_key.public_key,
            }],
        };

        let push = push_share_branch(&source_context, &settings, "team", None).unwrap();
        assert_eq!(push.exported_session_count, 1);
        assert_eq!(push.exported_turn_count, 1);

        let pull =
            pull_share_branch(&target_context, &ShareSettings::default(), "team", None).unwrap();
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
        let second_push = push_share_branch(&source_context, &settings, "team", None).unwrap();
        assert_eq!(second_push.exported_session_count, 2);
        let second_pull =
            pull_share_branch(&target_context, &ShareSettings::default(), "team", None).unwrap();
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

        exclude_all_sessions(&source_context).unwrap();
        let empty_push = push_share_branch(&source_context, &settings, "team", None).unwrap();
        assert_eq!(empty_push.exported_turn_count, 0);
        let empty_pull =
            pull_share_branch(&target_context, &ShareSettings::default(), "team", None).unwrap();
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
            git_upstream: Some(remote_url),
        };
        seed_share_export_session(
            &source_context.index_db_path,
            "source-repo",
            "00000000-0000-4000-8000-000000000303",
        );
        update_share_policy(&source_context, SharePolicy::All).unwrap();

        push_share_branch(&source_context, &ShareSettings::default(), "team", None).unwrap();
        let remote = Repository::open_bare(&remote_path).unwrap();
        remote
            .find_reference("refs/heads/darc/team")
            .unwrap()
            .delete()
            .unwrap();
        push_share_branch(&source_context, &ShareSettings::default(), "team", None).unwrap();

        assert_eq!(remote_tip_parent_count(&remote_path, "darc/team"), 0);
    }

    #[test]
    fn merge_requires_sync_exporter_before_pruning() {
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
        let source_identity = test_share_identity(&source_age_identity.to_public().to_string());
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
            branch: "team",
            turns,
            existing_objects: BTreeMap::new(),
        })
        .unwrap();
        write_export_artifact(&cache, &artifact).unwrap();
        import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap();
        let mut manifest =
            read_json_file::<ManifestArtifact>(&cache.join(ARTIFACT_ROOT).join("manifest.json"))
                .unwrap();
        manifest.exporter.user_id = "usr-attacker".to_owned();
        manifest.turns.clear();
        write_json_file(&cache.join(ARTIFACT_ROOT).join("manifest.json"), &manifest).unwrap();

        let error = import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
            "git:https://example.invalid/team/repo",
            &cache,
        )
        .unwrap_err();
        let target = open_index_database_writer(&target_context.index_db_path).unwrap();
        let session_count: i64 = target
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE origin_kind = 'shared'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            error.to_string().contains("exporter"),
            "error should reject mismatched exporter: {error:#}"
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
        let source_identity = test_share_identity(&source_age_identity.to_public().to_string());
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
            branch: "team",
            turns: turns.clone(),
            existing_objects: BTreeMap::new(),
        })
        .unwrap();
        write_export_artifact(&cache, &full).unwrap();
        import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
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
            branch: "team",
            turns: turns.into_iter().take(1).collect(),
            existing_objects: full.objects,
        })
        .unwrap();
        write_export_artifact(&cache, &shortened).unwrap();

        import_from_cache(
            &target_context,
            "team",
            "darc/team",
            "origin",
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

    fn init_test_git_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        let repository = Repository::init(path).unwrap();
        let mut config = repository.config().unwrap();
        config.set_str("user.name", "Synthetic User").unwrap();
        config
            .set_str("user.email", "synthetic@example.invalid")
            .unwrap();
    }

    fn test_share_identity(public_key: &str) -> ShareIdentity {
        ShareIdentity {
            user_id: "usr-synthetic".to_owned(),
            display_name: Some("Synthetic User".to_owned()),
            email: Some("synthetic@example.invalid".to_owned()),
            public_key: public_key.to_owned(),
        }
    }

    fn write_test_sync_object(
        cache: &Path,
        identity: &Identity,
        exporter: &ShareIdentity,
        project_key: &str,
        turns: Vec<SyncTurnEntry>,
    ) -> SyncManifestEntry {
        let payload = EncryptedSyncPayload {
            schema: SYNC_PAYLOAD_SCHEMA.to_owned(),
            version: 1,
            project_key: project_key.to_owned(),
            exporter: exporter.clone(),
            turns,
        };
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
