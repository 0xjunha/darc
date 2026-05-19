use super::*;

/// Reads the visible project artifact from one cache workdir when it exists.
pub(crate) fn read_cached_project_artifact(cache_path: &Path) -> Result<Option<ProjectArtifact>> {
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
pub(crate) fn read_cached_manifests(cache_path: &Path) -> Result<CachedManifestRead> {
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
pub(crate) fn read_cached_manifest(
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
pub(crate) fn checked_cached_manifest_size(path: &Path) -> Result<u64> {
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
pub(crate) fn remove_replaced_exporter_artifacts(
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
pub(crate) fn manifest_object_paths(manifest: &ManifestArtifact) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    paths.insert(manifest.sync.object_path.clone());
    paths.extend(manifest.turns.iter().map(|turn| turn.object_path.clone()));
    paths
}

/// Returns validated encrypted object paths referenced by visible manifests.
pub(crate) fn manifest_lfs_object_paths(
    cached_manifests: &[CachedManifest],
) -> Result<BTreeSet<String>> {
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
pub(crate) fn authenticated_retained_manifests(
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
pub(crate) fn verify_cached_manifest_payloads(
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
pub(crate) fn authenticated_manifest_turns(
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
pub(crate) fn manifest_entry_is_chunked(entry: &TurnManifestEntry) -> bool {
    entry.chunk_id.is_some() || entry.chunk_record_index.is_some()
}

/// Returns whether a signed manifest turn set uses chunked payloads.
pub(crate) fn manifest_turns_are_chunked(entries: &[TurnManifestEntry]) -> bool {
    entries.iter().any(manifest_entry_is_chunked)
}

/// Validates that all manifest entries use one payload mode.
pub(crate) fn validate_manifest_chunk_mode(entries: &[TurnManifestEntry]) -> Result<()> {
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
pub(crate) fn verify_cached_turn_payload(
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
pub(crate) fn import_from_cache(
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
pub(crate) fn import_from_cache_with_progress(
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
pub(crate) fn sync_turn_prune_key(entry: &SyncTurnEntry) -> (darc_paths::SourceKind, String, i64) {
    (entry.provider, entry.session_id.clone(), entry.turn_ordinal)
}

/// Imports decoded turns and updates per-exporter keep state.
#[allow(clippy::too_many_arguments)]
pub(crate) fn import_decoded_turns(
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
pub(crate) fn import_chunked_manifest_turns(
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
pub(crate) fn decode_manifest_entries(
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
pub(crate) fn read_sync_payload(
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
pub(crate) fn read_manifest_entry_turn(
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
pub(crate) fn read_manifest_chunks(
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
pub(crate) fn read_manifest_chunk_for_entries(
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
pub(crate) fn chunk_object_path_for_entries(
    chunk_id: &str,
    entries: &[&TurnManifestEntry],
) -> Result<String> {
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
pub(crate) fn read_manifest_chunk(
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
pub(crate) fn verify_chunked_manifest_entry(
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
