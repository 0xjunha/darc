use super::*;

/// Builds all share artifacts for the current export.
#[cfg(test)]
pub(crate) fn build_export_artifact(
    request: ExportBuildRequest<'_>,
) -> Result<BuiltExportArtifact> {
    build_export_artifact_with_reuse(request, ExportReuseContext::default())
}

/// Builds all share artifacts while exposing export progress events to tests.
#[cfg(test)]
pub(crate) fn build_export_artifact_with_progress(
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

/// Builds share export artifacts directly into one cache worktree.
pub(crate) fn build_export_artifact_to_cache_with_reuse(
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
pub(crate) fn build_export_artifact_with_reuse(
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

/// Builds share artifacts into the provided encrypted object target.
pub(crate) fn build_export_artifact_with_target(
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
pub(crate) fn should_emit_export_turn_progress(exported_turns: usize, total_turns: usize) -> bool {
    exported_turns == 1 || exported_turns == total_turns || exported_turns % 100 == 0
}

/// Returns the stable session identity for one exported turn.
pub(crate) fn share_turn_session_key(turn: &ShareTurnExport) -> (darc_paths::SourceKind, String) {
    (turn.session.provider, turn.session.session_id.clone())
}

/// Emits one session export progress update after checked count conversion.
pub(crate) fn emit_export_session_progress(
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
pub(crate) fn unchanged_previous_export_artifact(
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
pub(crate) fn authenticated_manifest_chunks(
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
pub(crate) fn manifest_uses_recipient_fingerprint(
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
pub(crate) fn sync_session_entries_from_states(
    selected_sessions: &[ShareSessionExportState],
) -> Option<BTreeSet<SyncSessionEntry>> {
    selected_sessions
        .iter()
        .map(sync_session_entry_from_state)
        .collect()
}

/// Checks that encrypted chunk files referenced by a reusable manifest still exist.
pub(crate) fn reusable_chunk_objects_are_available(
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
pub(crate) fn write_export_chunk(
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
pub(crate) fn manifest_matches_without_timestamp(
    left: &ManifestArtifact,
    right: &ManifestArtifact,
) -> bool {
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
pub(crate) fn encrypted_export_object(
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
pub(crate) fn read_trusted_export_object(
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
pub(crate) fn write_trusted_export_object(
    cache_path: &Path,
    object_path: &str,
    content: &[u8],
) -> Result<()> {
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
pub(crate) fn ensure_safe_trusted_object_cache_dir(path: &Path) -> Result<()> {
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
pub(crate) fn trusted_export_object_path(cache_path: &Path, object_path: &str) -> PathBuf {
    cache_path.join(format!("{}.age", sha256_hex(object_path.as_bytes())))
}

/// Inserts one encrypted export object while enforcing in-memory export caps.
pub(crate) fn insert_export_object(
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
pub(crate) fn write_export_artifact(path: &Path, artifact: &BuiltExportArtifact) -> Result<()> {
    write_export_metadata(path, artifact)?;
    for (relative, content) in &artifact.objects {
        write_artifact_file(path, relative, content)?;
    }
    Ok(())
}

/// Writes visible share metadata into a cache repository workdir.
pub(crate) fn write_export_metadata(path: &Path, artifact: &BuiltExportArtifact) -> Result<()> {
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
