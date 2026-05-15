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
    clear_project_share_states, import_shared_turn, open_index_database_writer,
    prune_shared_sessions, query_share_export_turns, query_share_status, set_project_share_policy,
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
const KEY_FILE_NAME: &str = "share.agekey";
const SHARE_CACHE_DIR: &str = "share-cache";
const DEFAULT_REMOTE_NAME: &str = "origin";

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
    turns: Vec<TurnManifestEntry>,
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
    update_share_policy(context, SharePolicy::All)
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
    let connection = open_index_database_writer(&context.index_db_path)?;
    let turns = query_share_export_turns(&connection, &context.project_id)?;
    let cache_path = cache_repo_path(&context.root, &remote.url, &git_branch);
    let repository = prepare_cache_repository(&cache_path, &remote.url, &identity)?;
    fetch_branch_if_exists(&repository, &remote.url, &git_branch)?;
    checkout_share_branch(&repository, &git_branch)?;
    let existing_objects = read_existing_objects(&cache_path)?;
    clear_cache_workdir(&cache_path)?;
    let artifact = build_export_artifact(
        context,
        settings,
        &project_key,
        &identity,
        branch,
        turns,
        existing_objects,
    )?;
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

/// Builds all share artifacts for the current export.
fn build_export_artifact(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    project_key: &str,
    identity: &ShareIdentity,
    branch: &str,
    turns: Vec<ShareTurnExport>,
    existing_objects: BTreeMap<String, Vec<u8>>,
) -> Result<BuiltExportArtifact> {
    let timestamp = current_utc_timestamp();
    let recipient_strings = encryption_recipient_strings(identity, settings);
    let recipient_fingerprint = encryption_recipient_fingerprint(&recipient_strings);
    let recipients = parse_encryption_recipients(&recipient_strings)?;
    let mut objects = BTreeMap::new();
    let mut manifest_turns = Vec::with_capacity(turns.len());
    let mut session_ids = BTreeSet::new();
    for turn in turns {
        session_ids.insert((turn.session.provider, turn.session.session_id.clone()));
        let payload = EncryptedTurnPayload {
            schema: TURN_PAYLOAD_SCHEMA.to_owned(),
            version: 1,
            project_key: project_key.to_owned(),
            exporter: identity.clone(),
            turn,
        };
        let plaintext =
            serde_json::to_vec(&payload).context("failed to serialize share payload")?;
        let payload_hash = sha256_hex(&plaintext);
        let object_path =
            format!("{ARTIFACT_ROOT}/objects/{recipient_fingerprint}-{payload_hash}.age");
        let encrypted = existing_objects
            .get(&object_path)
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| encrypt_payload(&plaintext, &recipients))?;
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
    let exported_turn_count =
        u64::try_from(manifest_turns.len()).context("turn count exceeds u64 range")?;
    Ok(BuiltExportArtifact {
        project: ProjectArtifact {
            schema: PROJECT_SCHEMA.to_owned(),
            version: 1,
            project_key: project_key.to_owned(),
            project_name: context.project_name.clone(),
            updated_at: timestamp.clone(),
        },
        manifest: ManifestArtifact {
            schema: MANIFEST_SCHEMA.to_owned(),
            version: 1,
            project_key: project_key.to_owned(),
            branch: branch.to_owned(),
            exported_at: timestamp,
            exporter: identity.clone(),
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
    let mut connection = open_index_database_writer(&context.index_db_path)?;
    let origin_remote = share_origin_remote(remote_name, git_branch);
    let keep_sessions = manifest
        .turns
        .iter()
        .map(|entry| (entry.provider, entry.session_id.clone()))
        .collect::<BTreeSet<_>>();
    prune_shared_sessions(
        &connection,
        &context.project_id,
        &origin_remote,
        &manifest.exporter.user_id,
        &keep_sessions,
    )?;
    let mut imported_turn_count = 0_u64;
    let mut skipped_turn_count = 0_u64;
    for entry in &manifest.turns {
        match import_manifest_entry(
            &mut connection,
            expected_project_key,
            &context.project_id,
            &origin_remote,
            &identity,
            entry,
            cache_path,
        ) {
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

/// Imports one manifest entry from an encrypted object file.
fn import_manifest_entry(
    connection: &mut Connection,
    expected_project_key: &str,
    project_id: &str,
    origin_remote: &str,
    identity: &Identity,
    entry: &TurnManifestEntry,
    cache_path: &Path,
) -> Result<bool> {
    let object_path = manifest_object_path(cache_path, entry)?;
    let ciphertext = fs::read(&object_path)
        .with_context(|| format!("failed to read {}", object_path.display()))?;
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
    if payload.project_key != expected_project_key {
        bail!("share payload project key does not match active project");
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
            project_id,
            user: &user,
            remote_name: origin_remote,
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
    if let Some(rest) = trimmed.strip_prefix("git@")
        && let Some((host, path)) = rest.split_once(':')
    {
        return format!(
            "https://{}/{}",
            host.to_ascii_lowercase(),
            path.trim_start_matches('/').to_ascii_lowercase()
        );
    }
    if let Some(rest) = trimmed.strip_prefix("ssh://git@")
        && let Some((host, path)) = rest.split_once('/')
    {
        return format!(
            "https://{}/{}",
            host.to_ascii_lowercase(),
            path.trim_start_matches('/').to_ascii_lowercase()
        );
    }
    if let Some(rest) = trimmed.strip_prefix("https://")
        && let Some((host, path)) = rest.split_once('/')
    {
        return format!(
            "https://{}/{}",
            host.to_ascii_lowercase(),
            path.trim_start_matches('/').to_ascii_lowercase()
        );
    }
    trimmed.to_owned()
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
            Ok(())
        }
        Err(error) if error.root_cause().to_string().contains("not found") => Ok(()),
        Err(error) => Err(error),
    }
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
        let content =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
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
    let expected_prefix = format!("{ARTIFACT_ROOT}/objects/");
    if !entry.object_path.starts_with(&expected_prefix) || !entry.object_path.ends_with(".age") {
        bail!("share object path is outside the supported object namespace");
    }
    let relative = Path::new(&entry.object_path);
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
    let content = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&content).with_context(|| format!("failed to parse {}", path.display()))
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
            "https://github.com/example/darc"
        );
        assert_eq!(
            normalize_git_url("https://github.com/Example/Darc.git/"),
            "https://github.com/example/darc"
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
        let identity = ShareIdentity {
            user_id: "usr-synthetic".to_owned(),
            display_name: Some("Synthetic User".to_owned()),
            email: Some("synthetic@example.invalid".to_owned()),
            public_key: Identity::generate().to_public().to_string(),
        };
        let first = build_export_artifact(
            &context,
            &ShareSettings::default(),
            "git:https://example.invalid/team/repo",
            &identity,
            "team",
            turns.clone(),
            BTreeMap::new(),
        )
        .unwrap();
        let settings = ShareSettings {
            remotes: Vec::new(),
            recipients: vec![ShareRecipient {
                recipient: Identity::generate().to_public().to_string(),
            }],
        };
        let second = build_export_artifact(
            &context,
            &settings,
            "git:https://example.invalid/team/repo",
            &identity,
            "team",
            turns,
            first.objects.clone(),
        )
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
    fn merge_skips_malformed_payload_with_warning() {
        let root = unique_test_dir("share-malformed-payload");
        let cache = root.join("cache");
        let object_path = cache.join(ARTIFACT_ROOT).join("objects").join("bad.age");
        fs::create_dir_all(object_path.parent().unwrap()).unwrap();
        fs::write(&object_path, b"not an age payload").unwrap();
        write_json_file(
            &cache.join(ARTIFACT_ROOT).join("manifest.json"),
            &ManifestArtifact {
                schema: MANIFEST_SCHEMA.to_owned(),
                version: 1,
                project_key: "git:https://example.invalid/team/repo".to_owned(),
                branch: "team".to_owned(),
                exported_at: "2026-05-15T00:00:00Z".to_owned(),
                exporter: ShareIdentity {
                    user_id: "usr-synthetic".to_owned(),
                    display_name: Some("Synthetic User".to_owned()),
                    email: Some("synthetic@example.invalid".to_owned()),
                    public_key: "age1synthetic".to_owned(),
                },
                turns: vec![TurnManifestEntry {
                    provider: SourceKind::Codex,
                    session_id: "00000000-0000-4000-8000-000000000001".to_owned(),
                    turn_ordinal: 1,
                    started_at: "2026-05-15T00:00:00Z".to_owned(),
                    payload_hash: "bad-hash".to_owned(),
                    object_path: format!("{ARTIFACT_ROOT}/objects/bad.age"),
                }],
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
        write_json_file(
            &cache.join(ARTIFACT_ROOT).join("manifest.json"),
            &ManifestArtifact {
                schema: MANIFEST_SCHEMA.to_owned(),
                version: 1,
                project_key: "git:https://example.invalid/team/repo".to_owned(),
                branch: "team".to_owned(),
                exported_at: "2026-05-15T00:00:00Z".to_owned(),
                exporter: ShareIdentity {
                    user_id: "usr-synthetic".to_owned(),
                    display_name: Some("Synthetic User".to_owned()),
                    email: Some("synthetic@example.invalid".to_owned()),
                    public_key: "age1synthetic".to_owned(),
                },
                turns: vec![TurnManifestEntry {
                    provider: SourceKind::Codex,
                    session_id: "00000000-0000-4000-8000-000000000001".to_owned(),
                    turn_ordinal: 1,
                    started_at: "2026-05-15T00:00:00Z".to_owned(),
                    payload_hash: "bad-hash".to_owned(),
                    object_path: format!("{ARTIFACT_ROOT}/objects/../bad.age"),
                }],
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
