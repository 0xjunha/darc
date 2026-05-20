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
        validate_share_remote_url("https://example.invalid/team/share.git?token=secret").is_err()
    );
    assert!(validate_share_remote_url("git://user:token@example.invalid/team/share.git").is_err());
    assert!(validate_share_remote_url("git://user@example.invalid/team/share.git").is_err());
    assert!(validate_share_remote_url("ssh://user:pass@example.invalid/team/share.git").is_err());
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
    let recipient_fingerprint = encryption_recipient_fingerprint(&encryption_recipient_strings(
        &identity,
        &ShareSettings::default(),
    ));
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
    let recipient_fingerprint = encryption_recipient_fingerprint(&encryption_recipient_strings(
        &identity,
        &ShareSettings::default(),
    ));
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
    let recipient_fingerprint = encryption_recipient_fingerprint(&encryption_recipient_strings(
        &identity,
        &ShareSettings::default(),
    ));
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
    let recipient_fingerprint = encryption_recipient_fingerprint(&encryption_recipient_strings(
        &identity,
        &ShareSettings::default(),
    ));
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
    let recipient_fingerprint = encryption_recipient_fingerprint(&encryption_recipient_strings(
        &identity,
        &ShareSettings::default(),
    ));
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
    prepare_cache_repository(&cache, &remote.cache_url, &context.local_path, &identity).unwrap();
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
    let mut sync_payload: EncryptedSyncPayload = serde_json::from_slice(&sync_plaintext).unwrap();
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

    let error =
        remove_artifact_object(&cache, &format!("{ARTIFACT_ROOT}/objects/orphan.age")).unwrap_err();

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
    let second_push = push_share_branch(&source_context, &settings, "team", Some("share")).unwrap();
    assert_eq!(second_push.exported_session_count, 2);
    let second_pull = pull_share_branch(&target_context, &settings, "team", Some("share")).unwrap();
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
    let empty_push = push_share_branch(&source_context, &settings, "team", Some("share")).unwrap();
    assert_eq!(empty_push.exported_turn_count, 0);
    let empty_pull = pull_share_branch(&target_context, &settings, "team", Some("share")).unwrap();
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
    assert!(
        events
            .iter()
            .any(|event| { matches!(event, SharePushProgress::BuildingExport { total_turns: 1 }) })
    );
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
fn synthetic_share_turn(project_id: &str, session_id: &str, turn_ordinal: i64) -> ShareTurnExport {
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
    run_cache_git_with_lfs_filter_override(
        &workspace,
        ["add", ".gitattributes", "object.bin"],
        "failed to stage synthetic LFS file",
        true,
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
