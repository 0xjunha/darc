use super::*;

/// Verifies that a path resolves inside a Git repository.
pub(crate) fn ensure_git_repository(path: &Path) -> Result<()> {
    run_git(
        path,
        ["rev-parse", "--git-dir"],
        &format!("failed to discover Git repository from {}", path.display()),
    )?;
    Ok(())
}

/// Reads one optional Git config value through the user's Git client.
pub(crate) fn git_config_value(path: &Path, key: &str) -> Result<Option<String>> {
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
pub(crate) fn origin_configured_remote_url(path: &Path) -> Result<String> {
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
pub(crate) fn origin_effective_remote_url(path: &Path) -> Result<String> {
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
pub(crate) fn resolved_remote_url(path: &Path, url: &str) -> Result<String> {
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
pub(crate) fn cache_remote_url_from_resolved(resolved: &str) -> Result<String> {
    let cache_url = sanitize_git_url_for_cache_remote(resolved);
    validate_share_remote_url(&cache_url)?;
    Ok(cache_url)
}

/// Resolves one local Git path URL against the active project path.
pub(crate) fn resolve_local_git_path_url(project_path: &Path, url: &str) -> String {
    let candidate = Path::new(url);
    if url.contains("://") || normalize_scp_like_git_url(url).is_some() || candidate.is_absolute() {
        return url.to_owned();
    }
    project_path.join(candidate).to_string_lossy().into_owned()
}

/// Removes credential-bearing URL parts before persisting a cache Git remote.
pub(crate) fn sanitize_git_url_for_cache_remote(url: &str) -> String {
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
pub(crate) fn prepare_cache_repository(
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
pub(crate) fn create_safe_cache_repository_dir(path: &Path) -> Result<()> {
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
pub(crate) fn create_safe_ancestor_dir_all(path: &Path) -> Result<()> {
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
pub(crate) fn ensure_safe_existing_cache_dir(path: &Path) -> Result<bool> {
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
pub(crate) fn ensure_safe_cache_git_dir(path: &Path) -> Result<()> {
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
pub(crate) fn configure_cache_repository(
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
pub(crate) fn configure_cache_ssh_command(
    cache_path: &Path,
    source_repo_path: &Path,
) -> Result<()> {
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
pub(crate) fn configure_git_lfs(path: &Path) -> Result<bool> {
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
pub(crate) fn git_lfs_available(path: &Path) -> Result<bool> {
    let output = run_git_raw(path, ["lfs", "version"]).context("failed to inspect Git LFS")?;
    Ok(output.status.success())
}

/// Returns whether Darc should publish share objects through Git LFS.
pub(crate) fn git_lfs_publish_enabled(path: &Path) -> Result<bool> {
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
pub(crate) fn git_lfs_publish_enabled_from_env(
    enable: Option<OsString>,
    disable: Option<OsString>,
    available: bool,
) -> bool {
    disable.is_none() && enable.is_some() && available
}

/// Returns whether Darc can hydrate existing Git LFS share objects.
pub(crate) fn git_lfs_hydration_enabled(path: &Path) -> Result<bool> {
    if std::env::var_os("DARC_SHARE_DISABLE_LFS").is_some() {
        return Ok(false);
    }
    git_lfs_available(path)
}

/// Downloads referenced Git LFS share objects for one fetched cache checkout when supported.
pub(crate) fn hydrate_lfs_objects(path: &Path, object_paths: &BTreeSet<String>) -> Result<()> {
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
pub(crate) fn reject_lfs_pointer_objects(
    cache_path: &Path,
    object_paths: &BTreeSet<String>,
) -> Result<()> {
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
pub(crate) fn assert_no_checked_out_lfs_config(cache_path: &Path) -> Result<()> {
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
pub(crate) fn fetch_branch_if_exists(path: &Path, git_branch: &str) -> Result<bool> {
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
pub(crate) fn clear_share_branch_refs(path: &Path, git_branch: &str) -> Result<()> {
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
pub(crate) fn clear_cache_worktree(path: &Path) -> Result<()> {
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
pub(crate) fn clean_cached_checkout(path: &Path) -> Result<()> {
    if !ensure_safe_existing_cache_dir(path)? {
        return Ok(());
    }
    ensure_safe_cache_git_dir(path)?;
    reset_cached_checkout(path)?;
    clean_untracked_cache_worktree(path)
}

/// Resets one cache checkout to its current HEAD before importing artifacts.
pub(crate) fn reset_cached_checkout(path: &Path) -> Result<()> {
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
pub(crate) fn clean_untracked_cache_worktree(cache_path: &Path) -> Result<()> {
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
pub(crate) fn clean_non_artifact_share_cache_files(path: &Path) -> Result<()> {
    clean_share_cache_files(path, allowed_share_cache_file)
}

/// Builds the exact cache-relative file set that may be published.
pub(crate) fn allowed_share_cache_paths(
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
pub(crate) fn insert_allowed_share_cache_path(paths: &mut BTreeSet<String>, relative: &str) {
    if allowed_share_cache_file(Path::new(relative)) {
        paths.insert(relative.to_owned());
    }
}

/// Removes files outside the authenticated share artifact publish set.
pub(crate) fn clean_unexpected_share_cache_files(
    path: &Path,
    allowed_paths: &BTreeSet<String>,
) -> Result<()> {
    clean_share_cache_files(path, |relative| {
        cache_relative_path_key(relative)
            .as_ref()
            .is_some_and(|relative| allowed_paths.contains(relative))
    })
}

/// Removes cache files rejected by one cache-relative allow predicate.
pub(crate) fn clean_share_cache_files(
    path: &Path,
    keep_file: impl Fn(&Path) -> bool + Copy,
) -> Result<()> {
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
pub(crate) fn clean_share_cache_entry(
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
pub(crate) fn allowed_share_cache_file(relative: &Path) -> bool {
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
pub(crate) fn cache_relative_path_components(relative: &Path) -> Option<Vec<&str>> {
    relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect()
}

/// Returns one slash-separated key for a cache-relative path.
pub(crate) fn cache_relative_path_key(relative: &Path) -> Option<String> {
    cache_relative_path_components(relative).map(|components| components.join("/"))
}

/// Fetches one remote share branch into the cache repository.
pub(crate) fn fetch_branch(path: &Path, git_branch: &str) -> Result<()> {
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
pub(crate) fn checkout_share_branch(path: &Path, git_branch: &str) -> Result<()> {
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
pub(crate) fn commit_cache_repository(path: &Path, git_branch: &str) -> Result<String> {
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
pub(crate) fn push_branch(path: &Path, git_branch: &str) -> Result<()> {
    push_branch_impl::<fn(SharePushProgress)>(path, git_branch, None)
}

/// Pushes one local share branch while streaming Git upload progress.
pub(crate) fn push_branch_with_progress<F>(
    path: &Path,
    git_branch: &str,
    progress: &mut F,
) -> Result<()>
where
    F: FnMut(SharePushProgress),
{
    push_branch_impl(path, git_branch, Some(progress))
}

/// Pushes one local share branch, optionally streaming Git upload progress.
pub(crate) fn push_branch_impl<F>(
    path: &Path,
    git_branch: &str,
    mut progress: Option<&mut F>,
) -> Result<()>
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
pub(crate) fn push_branch_commands(
    git_branch: &str,
    lfs_available: bool,
) -> Vec<PushBranchCommand> {
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
pub(crate) fn git_ref_exists(path: &Path, reference: &str) -> Result<bool> {
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
pub(crate) fn rev_parse_head(path: &Path) -> Result<String> {
    let output = run_cache_git(
        path,
        ["rev-parse", "HEAD"],
        "failed to read share commit id",
    )?;
    Ok(output.stdout.trim().to_owned())
}

/// Returns whether one Git failure means the remote share branch is absent.
pub(crate) fn is_missing_remote_ref_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("couldn't find remote ref")
        || message.contains("could not find remote ref")
        || message.contains("couldn't find remote branch")
}

/// Runs one system Git command and requires a successful exit status.
pub(crate) fn run_git<I, S>(path: &Path, args: I, context: &str) -> Result<GitCommandOutput>
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
pub(crate) fn run_cache_git<I, S>(
    cache_path: &Path,
    args: I,
    context: &str,
) -> Result<GitCommandOutput>
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
pub(crate) fn run_cache_git_raw<I, S>(cache_path: &Path, args: I) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_cache_git_raw_with_hook_override(cache_path, args, true)
}

/// Runs one cache Git command while controlling Git hook override behavior.
pub(crate) fn run_cache_git_with_hook_override<I, S>(
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
pub(crate) fn run_cache_git_with_lfs_filter_override<I, S>(
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
pub(crate) fn run_cache_git_raw_with_hook_override<I, S>(
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
pub(crate) fn run_cache_git_raw_with_options<I, S>(
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
pub(crate) fn run_cache_git_streaming_progress<I, S>(
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
pub(crate) fn run_cache_git_raw_streaming_progress<I, S>(
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
pub(crate) fn scoped_cache_git_args<I, S>(cache_path: &Path, args: I) -> Vec<OsString>
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
pub(crate) fn run_git_raw<I, S>(path: &Path, args: I) -> Result<GitCommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_git_raw_with_hook_override(path, args, true)
}

/// Runs one system Git command with optional hook override.
pub(crate) fn run_git_raw_with_hook_override<I, S>(
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
pub(crate) fn run_git_raw_with_options<I, S>(
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
pub(crate) fn run_git_raw_streaming_progress_with_options<I, S>(
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
pub(crate) fn collect_streaming_command_output(
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
pub(crate) fn configured_git_command(
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
pub(crate) fn read_git_progress_stderr<R: Read>(
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
pub(crate) fn emit_git_progress_fragment(
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
pub(crate) fn git_ssh_command(path: &Path) -> OsString {
    git_ssh_command_with_env(path, std::env::var_os("GIT_SSH_COMMAND"))
}

/// Returns an SSH command using environment first, then Git config.
pub(crate) fn git_ssh_command_with_env(path: &Path, command: Option<OsString>) -> OsString {
    if command.is_some() {
        return noninteractive_ssh_command(command);
    }
    noninteractive_ssh_command(git_core_ssh_command(path))
}

/// Reads the effective Git core.sshCommand for one repository path.
pub(crate) fn git_core_ssh_command(path: &Path) -> Option<OsString> {
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
pub(crate) fn noninteractive_ssh_command(command: Option<OsString>) -> OsString {
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
pub(crate) fn git_failure_message(context: &str, output: &GitCommandOutput) -> String {
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
pub(crate) fn sanitize_git_diagnostic(text: &str) -> String {
    text.split_whitespace()
        .map(sanitize_git_diagnostic_token)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sanitizes one possible URL-bearing diagnostic token.
pub(crate) fn sanitize_git_diagnostic_token(token: &str) -> String {
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
pub(crate) fn git_args_display(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| sanitize_git_diagnostic_token(arg.to_string_lossy().as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns whether a path is an existing non-symlink directory.
pub(crate) fn is_regular_directory(path: &Path) -> Result<bool> {
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
