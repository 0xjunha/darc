use std::{fs, path::Path, process::Command};

use anyhow::{Context, Result};
use darc_index::open_index_database;
use darc_paths::SourceKind;
use darc_test_utils::{
    IndexedSessionFixture, IndexedTurnFixture, insert_indexed_session, insert_indexed_turn,
    unique_test_dir, write_file,
};

const PRIMARY_SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";

/// Stores one minimal darc config fixture written for status CLI tests.
#[derive(serde::Serialize)]
struct ConfigFixture {
    version: u32,
    root: String,
    projects: Vec<ProjectFixture>,
    sources: SourcesFixture,
}

/// Stores one configured project fixture written for status CLI tests.
#[derive(serde::Serialize)]
struct ProjectFixture {
    id: String,
    name: String,
    local_path: String,
    sessions_root: String,
    known_paths: Vec<String>,
}

/// Stores source fixture tables written for status CLI tests.
#[derive(Default, serde::Serialize)]
struct SourcesFixture {
    #[serde(skip_serializing_if = "Option::is_none")]
    codex: Option<CodexSourceFixture>,
}

/// Stores one Codex source fixture written for status CLI tests.
#[derive(serde::Serialize)]
struct CodexSourceFixture {
    enabled: bool,
    home: String,
    sessions_root: String,
}

/// Returns the compiled `darc` binary path exposed by Cargo integration tests.
fn darc_binary() -> &'static str {
    env!("CARGO_BIN_EXE_darc")
}

/// Returns whether captured output contains an ANSI control sequence.
fn contains_ansi(bytes: &[u8]) -> bool {
    bytes.windows(2).any(|window| window == b"\x1b[")
}

/// Builds one configured project fixture for status CLI tests.
fn project_fixture(root: &Path, name: &str, project_id: &str) -> ProjectFixture {
    ProjectFixture {
        id: project_id.to_owned(),
        name: name.to_owned(),
        local_path: root.join(name).to_string_lossy().into_owned(),
        sessions_root: root
            .join(format!("projects/{project_id}/sessions"))
            .to_string_lossy()
            .into_owned(),
        known_paths: Vec::new(),
    }
}

/// Writes one status config fixture to disk.
fn write_config_fixture(
    root: &Path,
    projects: Vec<ProjectFixture>,
    sources: SourcesFixture,
) -> Result<()> {
    let config = ConfigFixture {
        version: 1,
        root: root.to_string_lossy().into_owned(),
        projects,
        sources,
    };
    write_file(
        &root.join("config.toml"),
        &toml::to_string(&config).context("failed to serialize config fixture TOML")?,
    )
}

/// Runs one status command in a given directory.
fn run_status(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new(darc_binary())
        .args(args)
        .current_dir(cwd)
        .output()
        .context("failed to run darc status")
}

/// Inserts one indexed session and turn for a configured project.
fn insert_index_fixture(root: &Path, project_id: &str, project_root: &Path) -> Result<()> {
    let connection = open_index_database(&root.join("index.sqlite"))?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new(
            project_id,
            SourceKind::Codex,
            PRIMARY_SESSION_ID,
            project_root.to_string_lossy().as_ref(),
        ),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture::new(
            project_id,
            SourceKind::Codex,
            PRIMARY_SESSION_ID,
            0,
            "2026-04-06T10:00:00Z",
            "completed",
            "[]",
        ),
    )?;
    Ok(())
}

/// Writes one minimal live Codex rollout fixture for sync-check status tests.
fn write_codex_rollout(sessions_root: &Path, session_id: &str, cwd: &Path) -> Result<()> {
    write_file(
        &sessions_root.join(format!(
            "2026/04/01/rollout-2026-04-01T10-00-00-{session_id}.jsonl"
        )),
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"cwd\":\"{cwd}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Inspect repo status\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Done\"}}]}}}}\n"
            ),
            cwd = cwd.display(),
            session_id = session_id,
        ),
    )
}

#[test]
fn status_reports_active_project_counts() -> Result<()> {
    let root = unique_test_dir("cli-status-project");
    let project = project_fixture(&root, "repo", "repo-abc123");
    fs::create_dir_all(Path::new(&project.local_path))?;
    fs::create_dir_all(Path::new(&project.sessions_root))?;
    write_config_fixture(
        &root,
        vec![project_fixture(&root, "repo", "repo-abc123")],
        SourcesFixture::default(),
    )?;
    insert_index_fixture(&root, "repo-abc123", Path::new(&project.local_path))?;

    let output = run_status(
        Path::new(&project.local_path),
        &["status", "--root", root.to_str().unwrap()],
    )?;

    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!contains_ansi(&output.stdout));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("Active Project"));
    assert!(stdout.contains("  Name: repo"));
    assert!(stdout.contains("  ID: repo-abc123"));
    assert!(stdout.contains("Index DB: ok"));
    assert!(stdout.contains("  Sessions: 1"));
    assert!(stdout.contains("  Turns: 1"));
    assert!(stdout.contains("Status"));
    assert!(stdout.contains("  Overall: ok"));

    Ok(())
}

#[test]
fn status_workspace_reports_all_projects() -> Result<()> {
    let root = unique_test_dir("cli-status-workspace");
    let repo_a = project_fixture(&root, "repo-a", "repo-a-123");
    let repo_b = project_fixture(&root, "repo-b", "repo-b-456");
    fs::create_dir_all(Path::new(&repo_a.local_path))?;
    fs::create_dir_all(Path::new(&repo_a.sessions_root))?;
    fs::create_dir_all(Path::new(&repo_b.local_path))?;
    fs::create_dir_all(Path::new(&repo_b.sessions_root))?;
    write_config_fixture(
        &root,
        vec![
            project_fixture(&root, "repo-a", "repo-a-123"),
            project_fixture(&root, "repo-b", "repo-b-456"),
        ],
        SourcesFixture::default(),
    )?;
    insert_index_fixture(&root, "repo-a-123", Path::new(&repo_a.local_path))?;

    let output = run_status(
        &root,
        &["status", "--root", root.to_str().unwrap(), "--workspace"],
    )?;

    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!contains_ansi(&output.stdout));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("Workspace Summary"));
    assert!(stdout.contains("  Projects: 2"));
    assert!(stdout.contains("  Indexed sessions: 1"));
    assert!(stdout.contains("  repo-a"));
    assert!(stdout.contains("    ID: repo-a-123"));
    assert!(stdout.contains("  repo-b"));
    assert!(stdout.contains("    ID: repo-b-456"));

    Ok(())
}

#[test]
fn status_check_reports_pending_sync_without_writes() -> Result<()> {
    let root = unique_test_dir("cli-status-check");
    let project = project_fixture(&root, "repo", "repo-abc123");
    let codex_home = root.join(".codex");
    let codex_sessions = codex_home.join("sessions");
    fs::create_dir_all(Path::new(&project.local_path))?;
    fs::create_dir_all(Path::new(&project.sessions_root))?;
    write_codex_rollout(
        &codex_sessions,
        PRIMARY_SESSION_ID,
        Path::new(&project.local_path),
    )?;
    write_config_fixture(
        &root,
        vec![project_fixture(&root, "repo", "repo-abc123")],
        SourcesFixture {
            codex: Some(CodexSourceFixture {
                enabled: true,
                home: codex_home.to_string_lossy().into_owned(),
                sessions_root: codex_sessions.to_string_lossy().into_owned(),
            }),
        },
    )?;

    let manifest_path = Path::new(&project.sessions_root).join(".manifest.json");
    let output = run_status(
        Path::new(&project.local_path),
        &["status", "--root", root.to_str().unwrap(), "--check"],
    )?;

    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!contains_ansi(&output.stdout));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("Sync Check"));
    assert!(stdout.contains("  Providers: Codex"));
    assert!(stdout.contains("  Sessions: 1 pending, 0 unchanged"));
    assert!(stdout.contains("  Manifest: would update"));
    assert!(!manifest_path.exists());

    Ok(())
}
