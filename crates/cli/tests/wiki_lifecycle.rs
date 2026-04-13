use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use darc_test_utils::{unique_test_dir, write_file};
use serde::Serialize;
use serde_json::Value;

/// Stores one minimal darc config fixture written for wiki lifecycle tests.
#[derive(Debug, Serialize)]
struct ConfigFixture {
    version: u32,
    root: String,
    projects: Vec<ProjectFixture>,
}

/// Stores one configured project fixture written for wiki lifecycle tests.
#[derive(Debug, Serialize)]
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

/// Creates one minimal darc root fixture with one configured project.
fn create_wiki_fixture_root(prefix: &str) -> Result<PathBuf> {
    let root = unique_test_dir(prefix);
    let project_root = root.join("repo");
    let sessions_root = root.join("projects/repo-abc123/sessions");
    fs::create_dir_all(&project_root)?;
    fs::create_dir_all(&sessions_root)?;
    let config = ConfigFixture {
        version: 1,
        root: root.to_string_lossy().into_owned(),
        projects: vec![ProjectFixture {
            id: "repo-abc123".to_owned(),
            name: "repo".to_owned(),
            local_path: project_root.to_string_lossy().into_owned(),
            sessions_root: sessions_root.to_string_lossy().into_owned(),
            known_paths: Vec::new(),
        }],
    };
    write_file(
        &root.join("config.toml"),
        &toml::to_string(&config).context("failed to serialize config fixture TOML")?,
    )?;
    Ok(root)
}

/// Runs the compiled `darc` binary and returns its captured output.
fn run_darc<I, S>(args: I) -> Result<std::process::Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new(darc_binary())
        .args(args)
        .output()
        .context("failed to run compiled darc binary")
}

/// Parses one UTF-8 JSON value from captured process output.
fn parse_json(bytes: &[u8], stream: &str) -> Result<Value> {
    serde_json::from_slice(bytes).with_context(|| format!("failed to parse {stream} JSON"))
}

/// Removes one temporary test root after the test finishes.
fn remove_root(root: &Path) -> Result<()> {
    fs::remove_dir_all(root)
        .with_context(|| format!("failed to remove temporary test root {}", root.display()))
}

/// Polls the wiki runs query until one run id is visible or the timeout expires.
fn wait_for_run_visibility(root: &Path, run_id: &str) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let output = run_darc([
            "query",
            "wiki",
            "runs",
            "--root",
            root.to_string_lossy().as_ref(),
            "--project-id",
            "repo-abc123",
            "--json",
        ])?;
        if output.status.success() {
            let value = parse_json(&output.stdout, "stdout")?;
            let found = value["data"]["runs"]
                .as_array()
                .map(|runs| runs.iter().any(|run| run["run_id"] == run_id))
                .unwrap_or(false);
            if found {
                return Ok(value);
            }
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for run `{run_id}` to become visible");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Polls the wiki runs query until one run reaches the requested status.
fn wait_for_run_status(root: &Path, run_id: &str, expected_status: &str) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let value = wait_for_run_visibility(root, run_id)?;
        let reached = value["data"]["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["run_id"] == run_id && run["status"] == expected_status);
        if reached {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for run `{run_id}` to reach status `{expected_status}`");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn wiki_digest_start_query_and_cancel_round_trip() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-lifecycle")?;

    let start_output = run_darc([
        "wiki",
        "digest",
        "start",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--session-ref",
        "codex:session-1",
        "--agent",
        "codex",
        "--runtime",
        "external-cli",
        "--model",
        "gpt-5.4",
        "--auth-profile",
        "openai/default",
        "--json",
    ])?;

    assert!(start_output.status.success());
    let start_value = parse_json(&start_output.stdout, "stdout")?;
    assert_eq!(start_value["schema"], "darc.wiki.digest.start.v1");
    assert_eq!(start_value["data"]["project_id"], "repo-abc123");
    assert_eq!(start_value["data"]["status"], "running");
    let run_id = start_value["data"]["run_id"]
        .as_str()
        .context("missing run id")?
        .to_owned();

    let run_dir = root
        .join("context-wiki/projects/repo-abc123/runs")
        .join(&run_id);
    assert!(run_dir.join("request.json").exists());
    assert!(run_dir.join("context.json").exists());
    assert!(run_dir.join("run.toml").exists());
    assert!(run_dir.join("events.jsonl").exists());
    assert!(run_dir.join("agent.stdout.log").exists());
    assert!(run_dir.join("agent.stderr.log").exists());
    assert!(!run_dir.join("cancel.flag").exists());

    let runs_value = wait_for_run_visibility(&root, &run_id)?;
    let run = runs_value["data"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["run_id"] == run_id)
        .context("run should be present in query output")?;
    assert_eq!(run["status"], "running");

    let cancel_output = run_darc([
        "wiki",
        "digest",
        "cancel",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--run-id",
        &run_id,
        "--json",
    ])?;

    assert!(cancel_output.status.success());
    let cancel_value = parse_json(&cancel_output.stdout, "stdout")?;
    assert_eq!(cancel_value["schema"], "darc.wiki.digest.cancel.v1");
    assert_eq!(cancel_value["data"]["run_id"], run_id);
    assert_eq!(cancel_value["data"]["status"], "cancel_requested");
    assert!(run_dir.join("cancel.flag").exists());

    let runs_value = wait_for_run_status(&root, &run_id, "canceled")?;
    let canceled = runs_value["data"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["run_id"] == run_id)
        .context("canceled run should still be visible")?;
    assert_eq!(canceled["status"], "canceled");

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_digest_start_repairs_stale_running_runs() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-stale-repair")?;
    let stale_run_dir = root.join("context-wiki/projects/repo-abc123/runs/cwrun_00stale");
    write_file(&root.join("context-wiki/VERSION"), "1\n")?;
    write_file(
        &stale_run_dir.join("run.toml"),
        concat!(
            "schema_version = 1\n",
            "run_id = \"cwrun_00stale\"\n",
            "project_id = \"repo-abc123\"\n",
            "status = \"running\"\n",
            "phase = \"waiting_for_agent\"\n",
            "created_at = \"2026-04-13T10:00:00Z\"\n",
            "started_at = \"2026-04-13T10:00:01Z\"\n",
            "updated_at = \"2026-04-13T10:00:02Z\"\n",
            "heartbeat_at = \"2026-04-13T10:00:02Z\"\n",
            "attempt = 1\n",
            "cancel_requested = false\n",
            "pid = 999999\n",
            "selected_sessions = []\n",
            "target_categories = []\n",
            "target_domains = []\n",
            "created_entry_ids = []\n",
            "updated_entry_ids = []\n"
        ),
    )?;

    let start_output = run_darc([
        "wiki",
        "digest",
        "start",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--session-ref",
        "codex:session-1",
        "--agent",
        "codex",
        "--runtime",
        "external-cli",
        "--model",
        "gpt-5.4",
        "--json",
    ])?;

    assert!(start_output.status.success());
    let start_value = parse_json(&start_output.stdout, "stdout")?;
    let new_run_id = start_value["data"]["run_id"]
        .as_str()
        .context("missing new run id")?
        .to_owned();

    let runs_output = run_darc([
        "query",
        "wiki",
        "runs",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;
    assert!(runs_output.status.success());
    let runs_value = parse_json(&runs_output.stdout, "stdout")?;
    let runs = runs_value["data"]["runs"].as_array().unwrap();
    let stale = runs
        .iter()
        .find(|run| run["run_id"] == "cwrun_00stale")
        .context("stale run should remain visible")?;
    assert_eq!(stale["status"], "interrupted");
    let fresh = runs
        .iter()
        .find(|run| run["run_id"] == new_run_id)
        .context("new run should be visible")?;
    assert_eq!(fresh["status"], "running");

    let cancel_output = run_darc([
        "wiki",
        "digest",
        "cancel",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--run-id",
        &new_run_id,
        "--json",
    ])?;
    assert!(cancel_output.status.success());

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_digest_start_times_out_to_failed_without_runtime() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-runtime-timeout")?;

    let start_output = run_darc([
        "wiki",
        "digest",
        "start",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--session-ref",
        "codex:session-1",
        "--agent",
        "codex",
        "--runtime",
        "external-cli",
        "--model",
        "gpt-5.4",
        "--json",
    ])?;
    assert!(start_output.status.success());
    let start_value = parse_json(&start_output.stdout, "stdout")?;
    let run_id = start_value["data"]["run_id"]
        .as_str()
        .context("missing run id")?
        .to_owned();

    let runs_value = wait_for_run_status(&root, &run_id, "failed")?;
    let failed = runs_value["data"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["run_id"] == run_id)
        .context("failed run should be visible")?;
    assert_eq!(failed["status"], "failed");

    let run_toml = fs::read_to_string(
        root.join("context-wiki/projects/repo-abc123/runs")
            .join(&run_id)
            .join("run.toml"),
    )?;
    assert!(run_toml.contains("error_code = \"runtime_not_implemented\""));

    remove_root(&root)?;
    Ok(())
}
