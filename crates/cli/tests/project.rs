use std::{fs, path::Path, process::Command};

use anyhow::{Context, Result};
use darc_test_utils::{unique_test_dir, write_file};

/// Stores one minimal darc config fixture written for project CLI tests.
#[derive(serde::Serialize)]
struct ConfigFixture {
    version: u32,
    root: String,
    projects: Vec<ProjectFixture>,
}

/// Stores one configured project fixture written for project CLI tests.
#[derive(serde::Serialize)]
struct ProjectFixture {
    id: String,
    name: String,
    local_path: String,
    sessions_root: String,
    known_paths: Vec<String>,
}

/// Returns the compiled `darc` binary path exposed by Cargo integration tests.
fn darc_binary() -> &'static str {
    env!("CARGO_BIN_EXE_darc")
}

/// Stores one config fixture for the provided project list.
fn write_config_fixture(root: &Path, projects: Vec<ProjectFixture>) -> Result<()> {
    let config = ConfigFixture {
        version: 1,
        root: root.to_string_lossy().into_owned(),
        projects,
    };
    write_file(
        &root.join("config.toml"),
        &toml::to_string(&config).context("failed to serialize config fixture TOML")?,
    )
}

#[test]
fn project_remove_dry_run_reports_preview_without_deleting_archive() -> Result<()> {
    let root = unique_test_dir("cli-project-remove-preview");
    let project_root = root.join("repo");
    let sessions_root = root.join("projects/repo-123/sessions");
    fs::create_dir_all(&project_root)?;
    write_file(
        &sessions_root.join("codex/rollout.jsonl"),
        "{\"type\":\"session_meta\"}\n",
    )?;
    write_config_fixture(
        &root,
        vec![ProjectFixture {
            id: "repo-123".into(),
            name: "repo".into(),
            local_path: project_root.to_string_lossy().into_owned(),
            sessions_root: sessions_root.to_string_lossy().into_owned(),
            known_paths: Vec::new(),
        }],
    )?;

    let output = Command::new(darc_binary())
        .args([
            "project",
            "remove",
            "--root",
            root.to_str().context("test root is not UTF-8")?,
            "--dry-run",
            "repo",
        ])
        .current_dir(&project_root)
        .output()
        .context("failed to run darc project remove --dry-run")?;

    assert!(
        output.status.success(),
        "project remove --dry-run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("Remove Preview"));
    assert!(stdout.contains("Would Delete"));
    assert!(stdout.contains("Overall: dry run only"));
    assert!(sessions_root.exists());
    assert!(fs::read_to_string(root.join("config.toml"))?.contains("repo-123"));

    Ok(())
}

#[test]
fn project_link_dry_run_reports_preview_without_writing_config() -> Result<()> {
    let root = unique_test_dir("cli-project-link-preview");
    let target_root = root.join("new-repo");
    let source_root = root.join("old-repo");
    let source_sessions = root.join("projects/old-123/sessions");
    let target_sessions = root.join("projects/new-456/sessions");
    fs::create_dir_all(&target_root)?;
    fs::create_dir_all(&source_root)?;
    fs::create_dir_all(&source_sessions)?;
    fs::create_dir_all(&target_sessions)?;
    write_config_fixture(
        &root,
        vec![
            ProjectFixture {
                id: "old-123".into(),
                name: "old-repo".into(),
                local_path: source_root.to_string_lossy().into_owned(),
                sessions_root: source_sessions.to_string_lossy().into_owned(),
                known_paths: vec![root.join("old-worktree").to_string_lossy().into_owned()],
            },
            ProjectFixture {
                id: "new-456".into(),
                name: "new-repo".into(),
                local_path: target_root.to_string_lossy().into_owned(),
                sessions_root: target_sessions.to_string_lossy().into_owned(),
                known_paths: Vec::new(),
            },
        ],
    )?;
    let before = fs::read_to_string(root.join("config.toml"))?;

    let output = Command::new(darc_binary())
        .args([
            "project",
            "link",
            "--root",
            root.to_str().context("test root is not UTF-8")?,
            "--dry-run",
            "old-repo",
        ])
        .current_dir(&target_root)
        .output()
        .context("failed to run darc project link --dry-run")?;

    assert!(
        output.status.success(),
        "project link --dry-run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("Link Preview"));
    assert!(stdout.contains("Would Update"));
    assert!(stdout.contains("Config: yes"));
    assert!(stdout.contains("Overall: dry run only"));
    assert_eq!(fs::read_to_string(root.join("config.toml"))?, before);

    Ok(())
}
