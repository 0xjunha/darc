use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use darc_index::{INDEX_DB_FILE_NAME, open_index_database};
use rusqlite::params;

use super::{
    RefreshOptions, RefreshProgress, preview_remove_project, refresh_all_projects,
    refresh_all_projects_best_effort, refresh_all_projects_best_effort_with_progress,
    registry::load_normalized_shared_config,
    remove_project,
    workflow::{
        link_project_from, preview_rename_project_from, refresh_project_from, rename_project_from,
    },
};
use crate::{
    active_project::load_active_project,
    config::{CodexSourceConfig, ProjectConfig, SharedConfig, SourcesConfig},
    constants::CONFIG_FILE_NAME,
};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Builds one unique temporary directory for project-management tests.
fn unique_test_dir(label: &str) -> PathBuf {
    let suffix = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "darc-project-{label}-{}-{suffix}",
        std::process::id()
    ))
}

/// Writes one UTF-8 file after creating its parent directory.
fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

/// Stores one minimal shared config used by project-management tests.
fn write_config(root: &Path, config: &SharedConfig) -> Result<()> {
    fs::create_dir_all(root)?;
    fs::write(root.join(CONFIG_FILE_NAME), toml::to_string_pretty(config)?)?;
    Ok(())
}

/// Writes one minimal live Codex rollout fixture for refresh tests.
fn write_codex_rollout(
    sessions_root: &Path,
    rollout_name: &str,
    session_id: &str,
    cwd: &Path,
    user_message: &str,
    assistant_reply: &str,
) -> Result<()> {
    write_file(
        &sessions_root.join(format!("2026/04/01/{rollout_name}")),
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"cwd\":\"{cwd}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"{user_message}\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{assistant_reply}\"}}]}}}}\n"
            ),
            cwd = cwd.display(),
            session_id = session_id,
            user_message = user_message,
            assistant_reply = assistant_reply,
        ),
    )
}

/// Writes one workspace fixture with a broken project followed by a healthy project.
fn write_partial_refresh_workspace(root: &Path) -> Result<()> {
    let broken_root = root.join("broken-repo");
    let healthy_root = root.join("healthy-repo");
    let codex_home = root.join(".codex");
    let codex_sessions_root = codex_home.join("sessions");
    fs::create_dir_all(&healthy_root)?;
    write_codex_rollout(
        &codex_sessions_root,
        "rollout-2026-04-01T10-10-00-22222222-2222-4222-8222-222222222241.jsonl",
        "22222222-2222-4222-8222-222222222241",
        &healthy_root,
        "Inspect healthy-repo",
        "Indexed healthy-repo",
    )?;

    write_config(
        root,
        &SharedConfig::new(
            root.to_path_buf(),
            vec![
                ProjectConfig {
                    id: "broken-repo-123".into(),
                    name: "broken-repo".into(),
                    local_path: broken_root,
                    git_upstream: None,
                    sessions_root: root.join("projects/broken-repo-123/sessions"),
                    known_paths: Vec::new(),
                },
                ProjectConfig {
                    id: "healthy-repo-456".into(),
                    name: "healthy-repo".into(),
                    local_path: healthy_root,
                    git_upstream: None,
                    sessions_root: root.join("projects/healthy-repo-456/sessions"),
                    known_paths: Vec::new(),
                },
            ],
            SourcesConfig {
                claude: None,
                codex: Some(CodexSourceConfig {
                    enabled: true,
                    home: codex_home,
                    sessions_root: codex_sessions_root,
                }),
            },
        ),
    )
}

/// Creates one stable test timestamp seed to keep temp paths distinct.
fn timestamp_seed() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

#[test]
fn link_project_merges_source_paths_without_duplicates() -> Result<()> {
    let root = unique_test_dir(&format!("link-{}", timestamp_seed()));
    let target_root = root.join("darc");
    let target_worktree = root.join("darc-wt");
    let source_root = root.join("memstack");
    let source_worktree = root.join("memstack-wt");
    fs::create_dir_all(&target_root)?;
    fs::create_dir_all(&target_worktree)?;
    fs::create_dir_all(&source_root)?;
    fs::create_dir_all(&source_worktree)?;

    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![
                ProjectConfig {
                    id: "darc-123".into(),
                    name: "darc".into(),
                    local_path: target_root.clone(),
                    git_upstream: None,
                    sessions_root: root.join("projects/darc-123/sessions"),
                    known_paths: vec![target_worktree.clone()],
                },
                ProjectConfig {
                    id: "memstack-456".into(),
                    name: "memstack".into(),
                    local_path: source_root.clone(),
                    git_upstream: None,
                    sessions_root: root.join("projects/memstack-456/sessions"),
                    known_paths: vec![source_worktree.clone(), target_worktree.clone()],
                },
            ],
            SourcesConfig::default(),
        ),
    )?;

    let report = link_project_from(&target_root, root.clone(), "memstack")?;

    assert_eq!(report.target_project_name, "darc");
    assert_eq!(report.source_project_name, "memstack");
    assert_eq!(report.new_known_paths.len(), 2);
    assert_eq!(report.total_known_paths, 3);
    assert!(report.config_written);

    let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
    let target_project = config
        .projects
        .iter()
        .find(|project| project.name == "darc")
        .context("missing target project")?;
    let source_project = config
        .projects
        .iter()
        .find(|project| project.name == "memstack")
        .context("missing source project")?;
    assert_eq!(
        target_project.known_paths,
        vec![
            fs::canonicalize(&target_worktree)?,
            fs::canonicalize(&source_root)?,
            fs::canonicalize(&source_worktree)?,
        ]
    );
    assert_eq!(
        source_project.known_paths,
        vec![fs::canonicalize(&source_worktree)?]
    );
    let active_project = load_active_project(&target_worktree, &root)?;
    assert_eq!(active_project.project.name, "darc");

    Ok(())
}

#[test]
fn link_project_cleans_source_overlap_when_target_already_knows_paths() -> Result<()> {
    let root = unique_test_dir(&format!("link-cleanup-{}", timestamp_seed()));
    let target_root = root.join("darc");
    let source_root = root.join("memstack");
    let source_worktree = root.join("memstack-wt");
    fs::create_dir_all(&target_root)?;
    fs::create_dir_all(&source_root)?;
    fs::create_dir_all(&source_worktree)?;

    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![
                ProjectConfig {
                    id: "darc-123".into(),
                    name: "darc".into(),
                    local_path: target_root.clone(),
                    git_upstream: None,
                    sessions_root: root.join("projects/darc-123/sessions"),
                    known_paths: vec![source_root.clone(), source_worktree.clone()],
                },
                ProjectConfig {
                    id: "memstack-456".into(),
                    name: "memstack".into(),
                    local_path: source_root.clone(),
                    git_upstream: None,
                    sessions_root: root.join("projects/memstack-456/sessions"),
                    known_paths: vec![source_worktree.clone()],
                },
            ],
            SourcesConfig::default(),
        ),
    )?;

    let report = link_project_from(&target_root, root.clone(), "memstack")?;

    assert!(report.config_written);
    assert!(report.new_known_paths.is_empty());
    assert_eq!(report.total_known_paths, 2);
    let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
    let source_project = config
        .projects
        .iter()
        .find(|project| project.name == "memstack")
        .context("missing source project")?;
    assert!(source_project.known_paths.is_empty());
    let active_project = load_active_project(&source_worktree, &root)?;
    assert_eq!(active_project.project.name, "darc");

    Ok(())
}

#[test]
fn link_project_supports_old_only_config_after_directory_rename() -> Result<()> {
    let root = unique_test_dir(&format!("link-old-only-{}", timestamp_seed()));
    let target_root = root.join("darc");
    let source_root = root.join("memstack");
    let source_worktree = root.join("memstack-wt");
    fs::create_dir_all(&target_root)?;
    fs::create_dir_all(&source_root)?;
    fs::create_dir_all(&source_worktree)?;

    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![ProjectConfig {
                id: "memstack-456".into(),
                name: "memstack".into(),
                local_path: source_root.clone(),
                git_upstream: None,
                sessions_root: root.join("projects/memstack-456/sessions"),
                known_paths: vec![source_worktree.clone()],
            }],
            SourcesConfig::default(),
        ),
    )?;

    let report = link_project_from(&target_root, root.clone(), "memstack")?;

    assert_eq!(report.target_project_name, "darc");
    assert_eq!(report.source_project_name, "memstack");
    assert_eq!(report.total_known_paths, 2);
    assert!(report.config_written);

    let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
    assert_eq!(config.projects.len(), 2);
    let target_project = config
        .projects
        .iter()
        .find(|project| project.name == "darc")
        .context("missing target project")?;
    assert_eq!(
        target_project.known_paths,
        vec![
            fs::canonicalize(&source_root)?,
            fs::canonicalize(&source_worktree)?
        ]
    );

    let active_project = load_active_project(&target_root, &root)?;
    assert_eq!(active_project.project.name, "darc");

    Ok(())
}

#[test]
fn remove_project_deletes_archive_and_index_rows() -> Result<()> {
    let root = unique_test_dir(&format!("remove-{}", timestamp_seed()));
    let target_root = root.join("darc");
    let source_root = root.join("memstack");
    let source_sessions = root.join("projects/memstack-456/sessions");
    let target_sessions = root.join("projects/darc-123/sessions");
    fs::create_dir_all(&target_root)?;
    fs::create_dir_all(&source_root)?;
    write_file(
        &source_sessions.join("codex/rollout.jsonl"),
        "{\"type\":\"session_meta\"}\n",
    )?;

    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![
                ProjectConfig {
                    id: "darc-123".into(),
                    name: "darc".into(),
                    local_path: target_root,
                    git_upstream: None,
                    sessions_root: target_sessions,
                    known_paths: Vec::new(),
                },
                ProjectConfig {
                    id: "memstack-456".into(),
                    name: "memstack".into(),
                    local_path: source_root,
                    git_upstream: None,
                    sessions_root: source_sessions.clone(),
                    known_paths: Vec::new(),
                },
            ],
            SourcesConfig::default(),
        ),
    )?;

    let index_db_path = root.join(INDEX_DB_FILE_NAME);
    let connection = open_index_database(&index_db_path)?;
    connection.execute(
        "INSERT INTO sessions (project_id, provider, session_id, parent_session_id, session_kind, archive_path, cwd) VALUES (?1, 'codex', 'source-session', NULL, 'primary', 'codex/rollout.jsonl', '/tmp/memstack')",
        params!["memstack-456"],
    )?;
    connection.execute(
        "INSERT INTO turns (project_id, provider, session_id, turn_ordinal, started_at, status, user_message, steps_json) VALUES (?1, 'codex', 'source-session', 0, '2026-04-01T10:00:00Z', 'completed', 'Inspect', '[]')",
        params!["memstack-456"],
    )?;
    connection.execute(
        "INSERT INTO sessions (project_id, provider, session_id, parent_session_id, session_kind, archive_path, cwd) VALUES (?1, 'codex', 'target-session', NULL, 'primary', 'codex/rollout.jsonl', '/tmp/darc')",
        params!["darc-123"],
    )?;

    let report = remove_project(Some(root.clone()), "memstack")?;

    assert_eq!(report.project_name, "memstack");
    assert_eq!(report.indexed_sessions_removed, 1);
    assert_eq!(report.indexed_turns_removed, 1);
    assert!(report.archive_deleted);
    assert!(report.config_written);
    assert!(!source_sessions.exists());

    let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
    assert_eq!(config.projects.len(), 1);
    assert_eq!(config.projects[0].name, "darc");

    let connection = open_index_database(&index_db_path)?;
    let remaining_source_sessions: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sessions WHERE project_id = 'memstack-456'",
        [],
        |row| row.get(0),
    )?;
    let remaining_target_sessions: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sessions WHERE project_id = 'darc-123'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(remaining_source_sessions, 0);
    assert_eq!(remaining_target_sessions, 1);

    Ok(())
}

#[test]
fn preview_remove_project_reports_changes_without_writes() -> Result<()> {
    let root = unique_test_dir(&format!("remove-preview-{}", timestamp_seed()));
    let project_root = root.join("repo");
    let sessions_root = root.join("projects/repo-123/sessions");
    fs::create_dir_all(&project_root)?;
    write_file(
        &sessions_root.join("codex/rollout.jsonl"),
        "{\"type\":\"session_meta\"}\n",
    )?;

    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![ProjectConfig {
                id: "repo-123".into(),
                name: "repo".into(),
                local_path: project_root,
                git_upstream: None,
                sessions_root: sessions_root.clone(),
                known_paths: Vec::new(),
            }],
            SourcesConfig::default(),
        ),
    )?;

    let index_db_path = root.join(INDEX_DB_FILE_NAME);
    let connection = open_index_database(&index_db_path)?;
    connection.execute(
        "INSERT INTO sessions (project_id, provider, session_id, parent_session_id, session_kind, archive_path, cwd) VALUES (?1, 'codex', 'repo-session', NULL, 'primary', 'codex/rollout.jsonl', '/tmp/repo')",
        params!["repo-123"],
    )?;
    connection.execute(
        "INSERT INTO turns (project_id, provider, session_id, turn_ordinal, started_at, status, user_message, steps_json) VALUES (?1, 'codex', 'repo-session', 0, '2026-04-01T10:00:00Z', 'completed', 'Inspect', '[]')",
        params!["repo-123"],
    )?;

    let report = preview_remove_project(Some(root.clone()), "repo")?;

    assert_eq!(report.project_name, "repo");
    assert_eq!(report.project_id, "repo-123");
    assert!(report.archive_would_delete);
    assert_eq!(report.indexed_sessions_would_remove, 1);
    assert_eq!(report.indexed_turns_would_remove, 1);
    assert!(report.config_would_change);
    assert!(sessions_root.exists());

    let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
    assert_eq!(config.projects.len(), 1);
    let remaining_sessions: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sessions WHERE project_id = 'repo-123'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(remaining_sessions, 1);

    Ok(())
}

#[test]
fn remove_project_rejects_ambiguous_names() -> Result<()> {
    let root = unique_test_dir(&format!("remove-ambiguous-{}", timestamp_seed()));
    let left_root = root.join("repo-a");
    let right_root = root.join("repo-b");
    fs::create_dir_all(&left_root)?;
    fs::create_dir_all(&right_root)?;

    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![
                ProjectConfig {
                    id: "same-111".into(),
                    name: "same".into(),
                    local_path: left_root,
                    git_upstream: None,
                    sessions_root: root.join("projects/same-111/sessions"),
                    known_paths: Vec::new(),
                },
                ProjectConfig {
                    id: "same-222".into(),
                    name: "same".into(),
                    local_path: right_root,
                    git_upstream: None,
                    sessions_root: root.join("projects/same-222/sessions"),
                    known_paths: Vec::new(),
                },
            ],
            SourcesConfig::default(),
        ),
    )?;

    let error = remove_project(Some(root), "same").expect_err("expected ambiguity error");
    assert!(error.to_string().contains("project `same` is ambiguous"));

    Ok(())
}

#[test]
fn remove_project_handles_missing_archive_and_index() -> Result<()> {
    let root = unique_test_dir(&format!("remove-empty-{}", timestamp_seed()));
    let project_root = root.join("repo");
    fs::create_dir_all(&project_root)?;

    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![ProjectConfig {
                id: "repo-123".into(),
                name: "repo".into(),
                local_path: project_root,
                git_upstream: None,
                sessions_root: root.join("projects/repo-123/sessions"),
                known_paths: Vec::new(),
            }],
            SourcesConfig::default(),
        ),
    )?;

    let report = remove_project(Some(root.clone()), "repo")?;

    assert_eq!(report.project_name, "repo");
    assert!(!report.archive_deleted);
    assert_eq!(report.indexed_sessions_removed, 0);
    assert_eq!(report.indexed_turns_removed, 0);
    let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
    assert!(config.projects.is_empty());

    Ok(())
}

#[test]
fn refresh_project_syncs_and_indexes_active_project() -> Result<()> {
    let root = unique_test_dir(&format!("refresh-{}", timestamp_seed()));
    let project_root = root.join("repo");
    let codex_home = root.join(".codex");
    let codex_sessions_root = codex_home.join("sessions");
    let project_sessions_root = root.join("projects/repo-123/sessions");
    let rollout_name = "rollout-2026-04-01T10-00-00-22222222-2222-4222-8222-22222222223f.jsonl";
    fs::create_dir_all(&project_root)?;
    write_codex_rollout(
        &codex_sessions_root,
        rollout_name,
        "22222222-2222-4222-8222-22222222223f",
        &project_root,
        "Refresh me",
        "Done",
    )?;

    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![ProjectConfig {
                id: "repo-123".into(),
                name: "repo".into(),
                local_path: project_root.clone(),
                git_upstream: None,
                sessions_root: project_sessions_root.clone(),
                known_paths: Vec::new(),
            }],
            SourcesConfig {
                claude: None,
                codex: Some(CodexSourceConfig {
                    enabled: true,
                    home: codex_home,
                    sessions_root: codex_sessions_root,
                }),
            },
        ),
    )?;

    let report = refresh_project_from(&project_root, root.clone(), &RefreshOptions::default())?;

    assert_eq!(report.sync.project_name, "repo");
    assert_eq!(report.sync.sessions_copied, 1);
    assert_eq!(report.index.project_name, "repo");
    assert_eq!(report.index.sessions_currently_indexed, 1);
    assert!(
        project_sessions_root
            .join(format!("codex/{rollout_name}"))
            .exists()
    );

    let connection = open_index_database(&root.join(INDEX_DB_FILE_NAME))?;
    let indexed_sessions: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sessions WHERE project_id = 'repo-123'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(indexed_sessions, 1);

    Ok(())
}

#[test]
fn refresh_all_projects_refreshes_each_registered_project() -> Result<()> {
    let root = unique_test_dir(&format!("refresh-all-{}", timestamp_seed()));
    let left_root = root.join("repo-a");
    let right_root = root.join("repo-b");
    let codex_home = root.join(".codex");
    let codex_sessions_root = codex_home.join("sessions");
    fs::create_dir_all(&left_root)?;
    fs::create_dir_all(&right_root)?;
    write_codex_rollout(
        &codex_sessions_root,
        "rollout-2026-04-01T10-00-00-22222222-2222-4222-8222-22222222223f.jsonl",
        "22222222-2222-4222-8222-22222222223f",
        &left_root,
        "Inspect repo-a",
        "Indexed repo-a",
    )?;
    write_codex_rollout(
        &codex_sessions_root,
        "rollout-2026-04-01T10-05-00-22222222-2222-4222-8222-222222222240.jsonl",
        "22222222-2222-4222-8222-222222222240",
        &right_root,
        "Inspect repo-b",
        "Indexed repo-b",
    )?;

    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![
                ProjectConfig {
                    id: "repo-a-123".into(),
                    name: "repo-a".into(),
                    local_path: left_root.clone(),
                    git_upstream: None,
                    sessions_root: root.join("projects/repo-a-123/sessions"),
                    known_paths: Vec::new(),
                },
                ProjectConfig {
                    id: "repo-b-456".into(),
                    name: "repo-b".into(),
                    local_path: right_root.clone(),
                    git_upstream: None,
                    sessions_root: root.join("projects/repo-b-456/sessions"),
                    known_paths: Vec::new(),
                },
            ],
            SourcesConfig {
                claude: None,
                codex: Some(CodexSourceConfig {
                    enabled: true,
                    home: codex_home,
                    sessions_root: codex_sessions_root,
                }),
            },
        ),
    )?;

    let report = refresh_all_projects(Some(root.clone()), RefreshOptions::default())?;

    assert_eq!(report.projects.len(), 2);
    assert!(
        report
            .projects
            .iter()
            .all(|project| project.sync.sessions_copied == 1)
    );
    assert!(
        report
            .projects
            .iter()
            .all(|project| project.index.sessions_currently_indexed == 1)
    );
    assert_eq!(
        report
            .projects
            .iter()
            .map(|project| project.sync.project_name.as_str())
            .collect::<Vec<_>>(),
        vec!["repo-a", "repo-b"]
    );

    let connection = open_index_database(&root.join(INDEX_DB_FILE_NAME))?;
    let repo_a_sessions: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sessions WHERE project_id = 'repo-a-123'",
        [],
        |row| row.get(0),
    )?;
    let repo_b_sessions: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sessions WHERE project_id = 'repo-b-456'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(repo_a_sessions, 1);
    assert_eq!(repo_b_sessions, 1);

    Ok(())
}

#[test]
fn refresh_all_projects_fails_fast_when_one_project_breaks() -> Result<()> {
    let root = unique_test_dir(&format!("refresh-all-fail-fast-{}", timestamp_seed()));
    write_partial_refresh_workspace(&root)?;

    let error = refresh_all_projects(Some(root.clone()), RefreshOptions::default())
        .expect_err("strict refresh-all should stop on the first failure");

    assert!(format!("{error:#}").contains("failed to refresh project `broken-repo`"));
    assert!(!root.join("projects/healthy-repo-456/sessions").exists());

    Ok(())
}

#[test]
fn refresh_all_projects_best_effort_continues_after_project_failure() -> Result<()> {
    let root = unique_test_dir(&format!("refresh-all-partial-{}", timestamp_seed()));
    write_partial_refresh_workspace(&root)?;

    let report = refresh_all_projects_best_effort(Some(root.clone()), RefreshOptions::default())?;

    assert_eq!(
        report
            .projects
            .iter()
            .map(|project| match project {
                super::RefreshProjectAttempt::Refreshed(project) =>
                    project.sync.project_name.as_str(),
                super::RefreshProjectAttempt::Failed(failure) => failure.project_name.as_str(),
            })
            .collect::<Vec<_>>(),
        vec!["broken-repo", "healthy-repo"]
    );
    let failure = report.projects[0]
        .failure()
        .context("expected broken project to fail")?;
    assert_eq!(failure.project_name, "broken-repo");
    assert!(format!("{:#}", failure.error).contains("failed to refresh project `broken-repo`"));
    let healthy_report = report.projects[1]
        .refreshed_report()
        .context("expected healthy project to refresh")?;
    assert_eq!(healthy_report.sync.project_name, "healthy-repo");
    assert_eq!(healthy_report.index.sessions_currently_indexed, 1);
    assert_eq!(report.refreshed_count(), 1);
    assert_eq!(report.failed_count(), 1);
    assert!(report.has_failures());

    let connection = open_index_database(&root.join(INDEX_DB_FILE_NAME))?;
    let indexed_sessions: i64 =
        connection.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
    assert_eq!(indexed_sessions, 1);

    Ok(())
}

#[test]
fn refresh_all_projects_best_effort_reports_progress_events() -> Result<()> {
    let root = unique_test_dir(&format!("refresh-all-progress-{}", timestamp_seed()));
    write_partial_refresh_workspace(&root)?;

    let mut events = Vec::new();
    let report = refresh_all_projects_best_effort_with_progress(
        Some(root),
        RefreshOptions::default(),
        |event| {
            events.push(match event {
                RefreshProgress::WorkspaceStarted { total_projects } => {
                    format!("workspace:{total_projects}")
                }
                RefreshProgress::ProjectStarted {
                    project_name,
                    project_index,
                    total_projects,
                    ..
                } => format!("project-start:{project_index}/{total_projects}:{project_name}"),
                RefreshProgress::SyncStarted { project_name } => {
                    format!("sync-start:{project_name}")
                }
                RefreshProgress::SyncFinished { project_name } => {
                    format!("sync-finish:{project_name}")
                }
                RefreshProgress::IndexStarted { project_name } => {
                    format!("index-start:{project_name}")
                }
                RefreshProgress::IndexFinished { project_name } => {
                    format!("index-finish:{project_name}")
                }
                RefreshProgress::ProjectFinished { project_name } => {
                    format!("project-finish:{project_name}")
                }
                RefreshProgress::ProjectFailed { project_name } => {
                    format!("project-failed:{project_name}")
                }
            });
        },
    )?;

    assert_eq!(report.refreshed_count(), 1);
    assert_eq!(report.failed_count(), 1);
    assert_eq!(
        events,
        vec![
            "workspace:2",
            "project-start:1/2:broken-repo",
            "project-failed:broken-repo",
            "project-start:2/2:healthy-repo",
            "sync-start:healthy-repo",
            "sync-finish:healthy-repo",
            "index-start:healthy-repo",
            "index-finish:healthy-repo",
            "project-finish:healthy-repo",
        ]
    );

    Ok(())
}

#[test]
fn rename_project_links_syncs_indexes_and_removes_source() -> Result<()> {
    let root = unique_test_dir(&format!("rename-{}", timestamp_seed()));
    let target_root = root.join("darc");
    let source_root = root.join("memstack");
    let codex_home = root.join(".codex");
    let codex_sessions_root = codex_home.join("sessions");
    let source_sessions_root = root.join("projects/memstack-456/sessions");
    let target_sessions_root = root.join("projects/darc-123/sessions");
    let rollout_name = "rollout-2026-04-01T10-00-00-22222222-2222-4222-8222-22222222223f.jsonl";
    fs::create_dir_all(&target_root)?;
    write_file(
        &codex_sessions_root.join(format!("2026/04/01/{rollout_name}")),
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"22222222-2222-4222-8222-22222222223f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"First task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"First reply\"}}]}}}}\n"
            ),
            source_root.display()
        ),
    )?;
    write_file(
        &source_sessions_root.join("codex/stale.jsonl"),
        "{\"type\":\"session_meta\"}\n",
    )?;

    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![
                ProjectConfig {
                    id: "darc-123".into(),
                    name: "darc".into(),
                    local_path: target_root.clone(),
                    git_upstream: None,
                    sessions_root: target_sessions_root.clone(),
                    known_paths: Vec::new(),
                },
                ProjectConfig {
                    id: "memstack-456".into(),
                    name: "memstack".into(),
                    local_path: source_root.clone(),
                    git_upstream: None,
                    sessions_root: source_sessions_root.clone(),
                    known_paths: Vec::new(),
                },
            ],
            SourcesConfig {
                claude: None,
                codex: Some(CodexSourceConfig {
                    enabled: true,
                    home: codex_home.clone(),
                    sessions_root: codex_sessions_root.clone(),
                }),
            },
        ),
    )?;

    let index_db_path = root.join(INDEX_DB_FILE_NAME);
    let connection = open_index_database(&index_db_path)?;
    connection.execute(
        "INSERT INTO sessions (project_id, provider, session_id, parent_session_id, session_kind, archive_path, cwd) VALUES (?1, 'codex', 'stale-session', NULL, 'primary', 'codex/stale.jsonl', '/tmp/memstack')",
        params!["memstack-456"],
    )?;

    let report = rename_project_from(&target_root, root.clone(), "memstack")?;

    assert_eq!(report.link.source_project_name, "memstack");
    assert_eq!(report.sync.sessions_copied, 1);
    assert_eq!(report.index.project_name, "darc");
    assert_eq!(report.index.sessions_currently_indexed, 1);
    assert_eq!(report.remove.project_name, "memstack");
    assert!(
        target_sessions_root
            .join(format!("codex/{rollout_name}"))
            .exists()
    );
    assert!(!source_sessions_root.exists());

    let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
    assert_eq!(config.projects.len(), 1);
    assert_eq!(config.projects[0].name, "darc");
    assert_eq!(config.projects[0].known_paths, vec![source_root]);

    let connection = open_index_database(&index_db_path)?;
    let target_sessions: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sessions WHERE project_id = 'darc-123'",
        [],
        |row| row.get(0),
    )?;
    let source_sessions: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sessions WHERE project_id = 'memstack-456'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(target_sessions, 1);
    assert_eq!(source_sessions, 0);

    Ok(())
}

#[test]
fn preview_rename_project_reports_workflow_without_writes() -> Result<()> {
    let root = unique_test_dir(&format!("rename-preview-{}", timestamp_seed()));
    let target_root = root.join("darc");
    let source_root = root.join("memstack");
    let source_sessions_root = root.join("projects/memstack-456/sessions");
    let target_sessions_root = root.join("projects/darc-123/sessions");
    fs::create_dir_all(&target_root)?;
    fs::create_dir_all(&source_root)?;
    write_file(
        &source_sessions_root.join("codex/previous.jsonl"),
        "{\"type\":\"session_meta\"}\n",
    )?;

    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![
                ProjectConfig {
                    id: "darc-123".into(),
                    name: "darc".into(),
                    local_path: target_root.clone(),
                    git_upstream: None,
                    sessions_root: target_sessions_root,
                    known_paths: Vec::new(),
                },
                ProjectConfig {
                    id: "memstack-456".into(),
                    name: "memstack".into(),
                    local_path: source_root.clone(),
                    git_upstream: None,
                    sessions_root: source_sessions_root.clone(),
                    known_paths: Vec::new(),
                },
            ],
            SourcesConfig::default(),
        ),
    )?;

    let connection = open_index_database(&root.join(INDEX_DB_FILE_NAME))?;
    connection.execute(
        "INSERT INTO sessions (project_id, provider, session_id, parent_session_id, session_kind, archive_path, cwd) VALUES (?1, 'codex', 'stale-session', NULL, 'primary', 'codex/previous.jsonl', '/tmp/memstack')",
        params!["memstack-456"],
    )?;

    let report = preview_rename_project_from(&target_root, root.clone(), "memstack")?;

    assert_eq!(report.target_project_name, "darc");
    assert_eq!(report.source_project_name, "memstack");
    assert_eq!(report.source_sessions_root, source_sessions_root);
    assert_eq!(report.new_known_paths, vec![fs::canonicalize(source_root)?]);
    assert_eq!(report.total_known_paths, 1);
    assert!(report.config_would_change);
    assert!(report.source_archive_would_delete);
    assert_eq!(report.indexed_sessions_would_remove, 1);
    assert_eq!(report.indexed_turns_would_remove, 0);
    assert!(source_sessions_root.exists());

    let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
    assert_eq!(config.projects.len(), 2);
    assert!(
        config
            .projects
            .iter()
            .any(|project| project.name == "memstack")
    );

    Ok(())
}

#[test]
fn rename_project_supports_old_only_config_after_directory_rename() -> Result<()> {
    let root = unique_test_dir(&format!("rename-old-only-{}", timestamp_seed()));
    let target_root = root.join("darc");
    let source_root = root.join("memstack");
    let codex_home = root.join(".codex");
    let codex_sessions_root = codex_home.join("sessions");
    let source_sessions_root = root.join("projects/memstack-456/sessions");
    let rollout_name = "rollout-2026-04-01T10-00-00-22222222-2222-4222-8222-22222222223f.jsonl";
    let shared_worktree = root.join("shared-worktree");
    fs::create_dir_all(&target_root)?;
    fs::create_dir_all(&shared_worktree)?;
    write_file(
        &codex_sessions_root.join(format!("2026/04/01/{rollout_name}")),
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"22222222-2222-4222-8222-22222222223f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Rename me\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Linked\"}}]}}}}\n"
            ),
            source_root.display()
        ),
    )?;
    write_file(
        &source_sessions_root.join("codex/stale.jsonl"),
        "{\"type\":\"session_meta\"}\n",
    )?;

    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![ProjectConfig {
                id: "memstack-456".into(),
                name: "memstack".into(),
                local_path: source_root.clone(),
                git_upstream: None,
                sessions_root: source_sessions_root.clone(),
                known_paths: vec![shared_worktree.clone()],
            }],
            SourcesConfig {
                claude: None,
                codex: Some(CodexSourceConfig {
                    enabled: true,
                    home: codex_home.clone(),
                    sessions_root: codex_sessions_root.clone(),
                }),
            },
        ),
    )?;

    let connection = open_index_database(&root.join(INDEX_DB_FILE_NAME))?;
    connection.execute(
        "INSERT INTO sessions (project_id, provider, session_id, parent_session_id, session_kind, archive_path, cwd) VALUES (?1, 'codex', 'stale-session', NULL, 'primary', 'codex/stale.jsonl', '/tmp/memstack')",
        params!["memstack-456"],
    )?;

    let report = rename_project_from(&target_root, root.clone(), "memstack")?;
    let target_sessions_root = root
        .join("projects")
        .join(&report.link.target_project_id)
        .join("sessions");

    assert_eq!(report.link.target_project_name, "darc");
    assert_eq!(report.link.source_project_name, "memstack");
    assert_eq!(report.index.project_name, "darc");
    assert_eq!(report.remove.project_name, "memstack");
    assert!(
        target_sessions_root
            .join(format!("codex/{rollout_name}"))
            .exists()
    );
    assert!(!source_sessions_root.exists());

    let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
    assert_eq!(config.projects.len(), 1);
    assert_eq!(config.projects[0].name, "darc");
    assert_eq!(config.projects[0].id, report.link.target_project_id);
    assert!(config.projects[0].known_paths.contains(&source_root));

    Ok(())
}

#[test]
fn rename_project_keeps_source_when_sync_fails() -> Result<()> {
    let root = unique_test_dir(&format!("rename-sync-fail-{}", timestamp_seed()));
    let target_root = root.join("darc");
    let source_root = root.join("memstack");
    let source_sessions_root = root.join("projects/memstack-456/sessions");
    let broken_sessions_root = root.join("broken");
    let codex_home = root.join(".codex");
    let codex_sessions_root = codex_home.join("sessions");
    let rollout_name = "rollout-2026-04-01T10-00-00-22222222-2222-4222-8222-22222222223f.jsonl";
    fs::create_dir_all(&target_root)?;
    fs::create_dir_all(&source_root)?;
    write_file(
        &codex_sessions_root.join(format!("2026/04/01/{rollout_name}")),
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"22222222-2222-4222-8222-22222222223f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Copy me\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Done\"}}]}}}}\n"
            ),
            source_root.display()
        ),
    )?;
    write_file(
        &source_sessions_root.join("codex/previous.jsonl"),
        "{\"type\":\"session_meta\"}\n",
    )?;
    write_file(&broken_sessions_root, "not-a-directory")?;

    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![
                ProjectConfig {
                    id: "darc-123".into(),
                    name: "darc".into(),
                    local_path: target_root.clone(),
                    git_upstream: None,
                    sessions_root: broken_sessions_root.clone(),
                    known_paths: Vec::new(),
                },
                ProjectConfig {
                    id: "memstack-456".into(),
                    name: "memstack".into(),
                    local_path: source_root.clone(),
                    git_upstream: None,
                    sessions_root: source_sessions_root.clone(),
                    known_paths: Vec::new(),
                },
            ],
            SourcesConfig {
                claude: None,
                codex: Some(CodexSourceConfig {
                    enabled: true,
                    home: codex_home,
                    sessions_root: codex_sessions_root,
                }),
            },
        ),
    )?;

    let error = rename_project_from(&target_root, root.clone(), "memstack")
        .expect_err("expected sync failure");
    assert!(error.to_string().contains("failed to create"));

    let config = load_normalized_shared_config(&root.join(CONFIG_FILE_NAME))?;
    assert_eq!(config.projects.len(), 2);
    assert!(
        config
            .projects
            .iter()
            .any(|project| project.name == "memstack")
    );
    assert!(source_sessions_root.exists());

    Ok(())
}
