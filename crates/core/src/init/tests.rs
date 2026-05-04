use std::{
    env, fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use darc_index::INDEX_DB_FILE_NAME;

use super::{
    config_io::{ExistingConfig, build_config, load_existing_config},
    project_config::{
        canonicalize_if_exists, merge_project_with_existing, project_config_from_path,
        project_id_from_path,
    },
    types::{DetectedRolloutSource, InitDraft},
    write_init,
};
use crate::{
    SourceKind,
    config::{ProjectConfig, SharedConfig, SourcesConfig, WatchConfig},
    constants::CONFIG_FILE_NAME,
};

static UNIQUE_TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Builds one unique temporary directory for init tests.
fn unique_test_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    let counter = UNIQUE_TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!(
        "test-{prefix}-{}-{nanos}-{counter}",
        std::process::id()
    ))
}

/// Builds an init draft fixture for display and status tests.
fn init_draft_fixture(global_config_exists: bool, project_exists: bool) -> Result<InitDraft> {
    let workspace_root = unique_test_dir("init-display");
    let project_root = workspace_root.join("repo");
    let sessions_root = workspace_root.join("projects/repo-abc123/sessions");
    fs::create_dir_all(&project_root)?;

    let project = ProjectConfig {
        id: "repo-abc123".into(),
        name: "repo".into(),
        local_path: project_root,
        git_upstream: Some("https://example.com/acme/repo.git".into()),
        sessions_root,
        known_paths: Vec::new(),
    };
    let config = SharedConfig::new(
        workspace_root,
        vec![project.clone()],
        SourcesConfig::default(),
    );

    Ok(InitDraft {
        global_config_exists,
        project_exists,
        sources: vec![
            DetectedRolloutSource {
                home: PathBuf::from("/Users/test/.claude"),
                kind: SourceKind::Claude,
                root: PathBuf::from("/Users/test/.claude/projects"),
                rollout_files: 12,
                subagent_rollout_files: 3,
            },
            DetectedRolloutSource {
                home: PathBuf::from("/Users/test/.codex"),
                kind: SourceKind::Codex,
                root: PathBuf::from("/Users/test/.codex/sessions"),
                rollout_files: 21,
                subagent_rollout_files: 0,
            },
        ],
        project,
        config,
    })
}

#[test]
fn init_draft_display_for_first_run_shows_global_and_project_sections() -> Result<()> {
    let draft = init_draft_fixture(false, false)?;
    let rendered = draft.to_string();

    assert!(rendered.contains("Global Darc: no config detected"));
    assert!(rendered.contains("Detected sources:"));
    assert!(rendered.contains("Root Path:"));
    assert!(rendered.contains("Config Path:"));
    assert!(rendered.contains("Index DB Path:"));
    assert!(rendered.contains("\nProject:\n"));
    assert!(rendered.contains("Name: repo"));
    assert!(rendered.contains("Upstream: https://example.com/acme/repo.git"));
    assert!(rendered.find("Detected sources:") < rendered.find("Project:"));

    Ok(())
}

#[test]
fn init_draft_display_for_existing_config_omits_detected_sources() -> Result<()> {
    let draft = init_draft_fixture(true, false)?;
    let rendered = draft.to_string();

    assert!(rendered.contains("Global Darc: existing config detected"));
    assert!(!rendered.contains("Detected sources:"));
    assert!(rendered.contains("\nProject:\n"));

    Ok(())
}

#[test]
fn project_ids_do_not_collide_for_same_repo_name() -> Result<()> {
    let projects_root = unique_test_dir("projects-root");
    let left_root = unique_test_dir("api-left").join("api");
    let right_root = unique_test_dir("api-right").join("api");
    fs::create_dir_all(&left_root)?;
    fs::create_dir_all(&right_root)?;

    let left = project_config_from_path(left_root, &projects_root, None)?;
    let right = project_config_from_path(right_root, &projects_root, None)?;

    assert_ne!(left.id, right.id);
    assert_ne!(left.sessions_root, right.sessions_root);
    assert_eq!(left.name, "api");
    assert_eq!(right.name, "api");

    Ok(())
}

#[test]
fn load_existing_projects_assigns_ids_without_moving_sessions_root() -> Result<()> {
    let workspace_root = unique_test_dir("legacy-config");
    let projects_root = workspace_root.join("projects");
    let project_root = workspace_root.join("repo");
    fs::create_dir_all(&projects_root)?;
    fs::create_dir_all(&project_root)?;

    let config_path = workspace_root.join(CONFIG_FILE_NAME);
    let legacy_sessions_root = projects_root.join("repo").join("sessions");
    let config_toml = format!(
        "[[projects]]\nname = \"repo\"\nlocal_path = \"{}\"\nsessions_root = \"{}\"\n",
        project_root.display(),
        legacy_sessions_root.display()
    );
    fs::write(&config_path, config_toml)?;

    let projects = load_existing_config(&config_path)?.projects;
    let project = &projects[0];

    assert!(!project.id.is_empty());
    assert_eq!(project.local_path, canonicalize_if_exists(project_root));
    assert_eq!(project.sessions_root, legacy_sessions_root);

    Ok(())
}

#[test]
fn load_existing_projects_drops_local_path_from_known_paths() -> Result<()> {
    let workspace_root = unique_test_dir("legacy-known-paths");
    let project_root = workspace_root.join("repo");
    let worktree_root = workspace_root.join("wt1");
    fs::create_dir_all(&project_root)?;
    fs::create_dir_all(&worktree_root)?;

    let config = SharedConfig::new(
        workspace_root.clone(),
        vec![ProjectConfig {
            id: "repo-abc123".into(),
            name: "repo".into(),
            local_path: project_root.clone(),
            git_upstream: None,
            sessions_root: workspace_root.join("projects/repo-abc123/sessions"),
            known_paths: vec![project_root.clone(), worktree_root.clone()],
        }],
        SourcesConfig::default(),
    );
    let config_path = workspace_root.join(CONFIG_FILE_NAME);
    fs::write(&config_path, toml::to_string_pretty(&config)?)?;

    let projects = load_existing_config(&config_path)?.projects;

    assert_eq!(projects.len(), 1);
    assert_eq!(
        projects[0].known_paths,
        vec![canonicalize_if_exists(worktree_root)]
    );

    Ok(())
}

#[test]
fn build_config_preserves_existing_update_check_opt_out() -> Result<()> {
    let workspace_root = unique_test_dir("preserve-update-check");
    let project_root = workspace_root.join("repo");
    let project = ProjectConfig {
        id: "repo-abc123".into(),
        name: "repo".into(),
        local_path: project_root,
        git_upstream: None,
        sessions_root: workspace_root.join("projects/repo-abc123/sessions"),
        known_paths: Vec::new(),
    };

    let config = build_config(
        ExistingConfig {
            projects: Vec::new(),
            check_for_update_on_startup: false,
        },
        project,
        &[],
        workspace_root,
    );

    assert!(!config.check_for_update_on_startup);

    Ok(())
}

#[test]
fn merge_project_with_existing_keeps_existing_sessions_root() -> Result<()> {
    let projects_root = unique_test_dir("merge-projects");
    let project_root = unique_test_dir("merge-repo");
    fs::create_dir_all(&projects_root)?;
    fs::create_dir_all(&project_root)?;

    let detected = project_config_from_path(project_root.clone(), &projects_root, None)?;
    let existing_sessions_root = projects_root.join("legacy-repo").join("sessions");
    let existing = ProjectConfig {
        sessions_root: existing_sessions_root.clone(),
        ..detected.clone()
    };

    let merged = merge_project_with_existing(&[existing], detected);

    assert_eq!(merged.sessions_root, existing_sessions_root);
    assert_eq!(merged.id, project_id_from_path(&project_root)?);

    Ok(())
}

#[test]
fn config_round_trips_with_known_paths() -> Result<()> {
    let dir = unique_test_dir("round-trip");
    fs::create_dir_all(&dir)?;

    let config = SharedConfig::new(
        dir.clone(),
        vec![ProjectConfig {
            id: "test-abc123".into(),
            name: "test".into(),
            local_path: dir.join("repo"),
            git_upstream: None,
            sessions_root: dir.join("sessions"),
            known_paths: vec![dir.join("wt1"), dir.join("wt2")],
        }],
        SourcesConfig::default(),
    );

    let toml_str = toml::to_string_pretty(&config)?;
    let loaded: SharedConfig = toml::from_str(&toml_str)?;

    assert_eq!(loaded.projects.len(), 1);
    assert_eq!(loaded.projects[0].known_paths.len(), 2);
    assert_eq!(loaded.projects[0].known_paths[0], dir.join("wt1"));

    Ok(())
}

#[test]
fn config_deserializes_without_known_paths() -> Result<()> {
    let dir = unique_test_dir("no-known-paths");
    fs::create_dir_all(&dir)?;

    let toml_str = format!(
        "version = 1\nroot = \"{dir}\"\n\n\
         [[projects]]\nid = \"x-123\"\nname = \"x\"\n\
         local_path = \"{dir}/repo\"\nsessions_root = \"{dir}/sessions\"\n",
        dir = dir.display()
    );
    let loaded: SharedConfig = toml::from_str(&toml_str)?;

    assert_eq!(loaded.projects.len(), 1);
    assert!(loaded.check_for_update_on_startup);
    assert!(loaded.projects[0].known_paths.is_empty());

    Ok(())
}

#[test]
fn config_round_trips_watch_defaults_when_present() -> Result<()> {
    let dir = unique_test_dir("watch-config");
    fs::create_dir_all(&dir)?;

    let mut config = SharedConfig::new(dir.clone(), Vec::new(), SourcesConfig::default());
    config.watch = WatchConfig {
        debounce: Some("30s".to_owned()),
        min_interval: Some("60s".to_owned()),
        reconcile_interval: Some("10m".to_owned()),
        providers: vec![SourceKind::Claude, SourceKind::Codex],
        poll: true,
    };

    let toml_str = toml::to_string_pretty(&config)?;
    let loaded: SharedConfig = toml::from_str(&toml_str)?;

    assert_eq!(loaded.watch.debounce.as_deref(), Some("30s"));
    assert_eq!(
        loaded.watch.providers,
        vec![SourceKind::Claude, SourceKind::Codex]
    );
    assert!(loaded.watch.poll);

    Ok(())
}

#[test]
fn write_init_creates_index_database_file() -> Result<()> {
    let draft = init_draft_fixture(false, false)?;
    let index_db_path = draft.root().join(INDEX_DB_FILE_NAME);

    write_init(&draft)?;

    assert!(draft.root().join(CONFIG_FILE_NAME).exists());
    assert!(index_db_path.exists());

    Ok(())
}
