use super::*;

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
pub(crate) fn ensure_share_signing_key(root: &Path) -> Result<SigningKey> {
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
pub(crate) fn ensure_private_key_directory(root: &Path) -> Result<PathBuf> {
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
pub(crate) fn push_share_branch_impl<F>(
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
pub(crate) fn fetch_share_branch_impl(
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
pub(crate) fn merge_share_branch_impl(
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
pub(crate) fn pull_share_branch_impl<F>(
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
