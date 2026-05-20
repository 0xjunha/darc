use super::*;

/// Returns one cache repository path for a remote URL and branch.
pub(crate) fn cache_repo_path(root: &Path, remote_url: &str, git_branch: &str) -> PathBuf {
    root.join(SHARE_CACHE_DIR).join(sha256_hex(
        format!("{}\n{git_branch}", canonical_share_remote_url(remote_url)).as_bytes(),
    ))
}

/// Returns the trusted local object cache path for one cache repository.
pub(crate) fn trusted_object_cache_path(cache_path: &Path) -> PathBuf {
    cache_path.join(".git").join(TRUSTED_OBJECT_CACHE_DIR)
}

/// Builds the stored provenance key for one imported remote branch.
pub(crate) fn share_origin_remote(remote_url: &str, git_branch: &str) -> String {
    let canonical_url = canonical_share_remote_url(remote_url);
    let identity = sha256_hex(format!("{canonical_url}\n{git_branch}").as_bytes());
    format!("remote:{}:{git_branch}", &identity[..16])
}

/// Returns a non-secret canonical URL for share cache and provenance keys.
pub(crate) fn canonical_share_remote_url(remote_url: &str) -> String {
    normalize_git_url(remote_url).unwrap_or_else(|_| sanitize_git_url_for_display(remote_url))
}

/// Returns the per-exporter visible manifest path.
pub(crate) fn exporter_manifest_relative_path(identity: &ShareIdentity) -> String {
    format!(
        "{ARTIFACT_ROOT}/{EXPORTERS_DIR}/{}/{}",
        exporter_manifest_id(identity),
        LEGACY_MANIFEST_FILE
    )
}

/// Returns one stable non-secret exporter path component.
pub(crate) fn exporter_manifest_id(identity: &ShareIdentity) -> String {
    sha256_hex(identity.user_id.as_bytes())[..16].to_owned()
}

/// Returns whether one manifest path is canonical for its authenticated exporter.
pub(crate) fn manifest_path_matches_exporter(relative_path: &str, exporter_id: &str) -> bool {
    relative_path == format!("{ARTIFACT_ROOT}/{LEGACY_MANIFEST_FILE}")
        || relative_path
            == format!("{ARTIFACT_ROOT}/{EXPORTERS_DIR}/{exporter_id}/{LEGACY_MANIFEST_FILE}")
}

/// Resolves and validates one manifest object path below the cache workdir.
pub(crate) fn manifest_object_path(
    cache_path: &Path,
    entry: &TurnManifestEntry,
) -> Result<PathBuf> {
    manifest_artifact_path(cache_path, &entry.object_path)
}

/// Removes one encrypted object path from the cache workdir if it exists.
pub(crate) fn remove_artifact_object(cache_path: &Path, object_path: &str) -> Result<()> {
    let path = manifest_artifact_path(cache_path, object_path)?;
    remove_file_if_exists(&path)
}

/// Removes one relative manifest file from the cache workdir if it exists.
pub(crate) fn remove_relative_file(cache_path: &Path, relative_path: &str) -> Result<()> {
    let relative = validate_relative_artifact_path(relative_path)?;
    ensure_safe_artifact_ancestors(cache_path, relative)?;
    remove_file_if_exists(&cache_path.join(relative))
}

/// Removes one file, ignoring already-missing paths.
pub(crate) fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

/// Resolves and validates one encrypted object path below the cache workdir.
pub(crate) fn manifest_artifact_path(cache_path: &Path, object_path: &str) -> Result<PathBuf> {
    let relative = validate_manifest_object_relative_path(object_path)?;
    ensure_safe_artifact_ancestors(cache_path, relative)?;
    Ok(cache_path.join(relative))
}

/// Validates one visible encrypted object path without touching the worktree.
pub(crate) fn validate_manifest_object_relative_path(object_path: &str) -> Result<&Path> {
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
pub(crate) fn write_json_artifact_file<T: Serialize>(
    cache_path: &Path,
    relative_path: &str,
    value: &T,
) -> Result<()> {
    let content = serde_json::to_vec_pretty(value).context("failed to serialize JSON")?;
    write_artifact_file(cache_path, relative_path, &content)
}

/// Writes one artifact file below the cache workdir without following symlinks.
pub(crate) fn write_artifact_file(
    cache_path: &Path,
    relative_path: &str,
    content: &[u8],
) -> Result<()> {
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
pub(crate) fn create_safe_dir_all(cache_path: &Path, directory: &Path) -> Result<()> {
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
pub(crate) fn validate_relative_artifact_path(relative_path: &str) -> Result<&Path> {
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
pub(crate) fn ensure_safe_artifact_ancestors(
    cache_path: &Path,
    relative_path: &Path,
) -> Result<()> {
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
pub(crate) fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("JSON path is missing a parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let content = serde_json::to_vec_pretty(value).context("failed to serialize JSON")?;
    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))
}

/// Reads one JSON file.
pub(crate) fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let content = read_regular_file(path, MAX_SHARE_MANIFEST_BYTES)?;
    serde_json::from_slice(&content).with_context(|| format!("failed to parse {}", path.display()))
}

/// Reads one regular artifact file after rejecting symlinks and oversized content.
pub(crate) fn read_regular_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
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
pub(crate) fn read_file_prefix(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
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
pub(crate) fn ensure_not_lfs_pointer(content: &[u8], path: &Path) -> Result<()> {
    if content.starts_with(GIT_LFS_POINTER_PREFIX) {
        bail!(
            "share object {} is a Git LFS pointer; run `git lfs pull` for the share cache",
            path.display()
        );
    }
    Ok(())
}
