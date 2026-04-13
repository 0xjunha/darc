#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::{
    fs,
    io::Write,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use darc_index::{INDEX_DB_FILE_NAME, open_index_database};
use darc_test_utils::{unique_test_dir, write_file};
use rusqlite::params;
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
    seed_wiki_index(&root)?;
    Ok(root)
}

/// Runs the compiled `darc` binary and returns its captured output.
fn run_darc<I, S>(args: I) -> Result<std::process::Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    run_darc_with_env(args, std::iter::empty::<(&str, &str)>())
}

/// Runs the compiled `darc` binary with additional environment overrides.
fn run_darc_with_env<I, S, K, V>(
    args: I,
    envs: impl IntoIterator<Item = (K, V)>,
) -> Result<std::process::Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
    K: AsRef<std::ffi::OsStr>,
    V: AsRef<std::ffi::OsStr>,
{
    Command::new(darc_binary())
        .args(args)
        .envs(envs)
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

/// Seeds one minimal indexed session/turn fixture so the worker can build digest context.
fn seed_wiki_index(root: &Path) -> Result<()> {
    let connection = open_index_database(&root.join(INDEX_DB_FILE_NAME))?;
    connection.execute(
        "INSERT INTO sessions (project_id, provider, session_id, parent_session_id, session_kind, archive_path, cwd) VALUES (?1, 'codex', 'session-1', NULL, 'primary', 'codex/session-1.jsonl', ?2)",
        params!["repo-abc123", root.join("repo").to_string_lossy().into_owned()],
    )?;
    connection.execute(
        concat!(
            "INSERT INTO turns (",
            "project_id, provider, session_id, turn_ordinal, started_at, completed_at, status, ",
            "user_message, final_answer_at, final_answer_text, steps_json, step_count, has_final_answer, primary_model",
            ") VALUES (?1, 'codex', 'session-1', 0, '2026-04-13T10:00:00Z', '2026-04-13T10:00:10Z', 'completed', ?2, '2026-04-13T10:00:10Z', ?3, '[]', 0, 1, 'gpt-5.4')"
        ),
        params![
            "repo-abc123",
            "Review the query surface",
            "Keep the query protocol additive"
        ],
    )?;
    connection.execute(
        "INSERT INTO sessions (project_id, provider, session_id, parent_session_id, session_kind, archive_path, cwd) VALUES (?1, 'claude', 'session-2', NULL, 'primary', 'claude/session-2.jsonl', ?2)",
        params!["repo-abc123", root.join("repo").to_string_lossy().into_owned()],
    )?;
    connection.execute(
        concat!(
            "INSERT INTO turns (",
            "project_id, provider, session_id, turn_ordinal, started_at, completed_at, status, ",
            "user_message, final_answer_at, final_answer_text, steps_json, step_count, has_final_answer, primary_model",
            ") VALUES (?1, 'claude', 'session-2', 0, '2026-04-13T11:00:00Z', '2026-04-13T11:00:10Z', 'completed', ?2, '2026-04-13T11:00:10Z', ?3, '[]', 0, 1, 'claude-sonnet-4-6')"
        ),
        params![
            "repo-abc123",
            "Summarize the runtime requirements",
            "Use the external CLI path first"
        ],
    )?;
    Ok(())
}

/// Writes one executable fake CLI script into the temporary test root.
fn write_fake_cli(root: &Path, name: &str, body: &str) -> Result<PathBuf> {
    let path = root.join(format!("{name}.sh"));
    let mut file = fs::File::create(&path)?;
    writeln!(file, "#!/bin/sh")?;
    write!(file, "{body}")?;
    #[cfg(unix)]
    {
        let mut permissions = file.metadata()?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions)?;
    }
    Ok(path)
}

#[test]
fn wiki_digest_start_query_and_cancel_round_trip() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-lifecycle")?;
    let codex = write_fake_cli(
        &root,
        "fake-codex-slow",
        concat!(
            "output=\"\"\n",
            "while [ \"$#\" -gt 0 ]; do\n",
            "  case \"$1\" in\n",
            "    -o|--output-last-message)\n",
            "      output=\"$2\"\n",
            "      shift 2\n",
            "      ;;\n",
            "    *)\n",
            "      shift\n",
            "      ;;\n",
            "  esac\n",
            "done\n",
            "sleep 2\n",
            "cat > \"$output\" <<'JSON'\n",
            "{\n",
            "  \"schema\": \"darc.wiki.digest.proposal.v1\",\n",
            "  \"project_id\": \"repo-abc123\",\n",
            "  \"run_id\": \"cwrun-placeholder\",\n",
            "  \"entries\": [],\n",
            "  \"run_summary\": {\n",
            "    \"title\": \"Canceled run\",\n",
            "    \"summary\": \"Canceled before validation completed.\",\n",
            "    \"themes\": [\"cancellation\"],\n",
            "    \"extracted_decision_count\": 0\n",
            "  }\n",
            "}\n",
            "JSON\n"
        ),
    )?;

    let start_output = run_darc_with_env(
        [
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
        ],
        [("DARC_WIKI_CODEX_BIN", codex.as_os_str())],
    )?;

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

    let cancel_output = run_darc_with_env(
        [
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
        ],
        [("DARC_WIKI_CODEX_BIN", codex.as_os_str())],
    )?;

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
    let codex = write_fake_cli(
        &root,
        "fake-codex-delay",
        concat!(
            "output=\"\"\n",
            "while [ \"$#\" -gt 0 ]; do\n",
            "  case \"$1\" in\n",
            "    -o|--output-last-message)\n",
            "      output=\"$2\"\n",
            "      shift 2\n",
            "      ;;\n",
            "    *)\n",
            "      shift\n",
            "      ;;\n",
            "  esac\n",
            "done\n",
            "sleep 2\n",
            "cat > \"$output\" <<'JSON'\n",
            "{\n",
            "  \"schema\": \"darc.wiki.digest.proposal.v1\",\n",
            "  \"project_id\": \"repo-abc123\",\n",
            "  \"run_id\": \"cwrun-placeholder\",\n",
            "  \"entries\": [],\n",
            "  \"run_summary\": {\n",
            "    \"title\": \"Delayed run\",\n",
            "    \"summary\": \"Delayed runtime.\",\n",
            "    \"themes\": [\"delay\"],\n",
            "    \"extracted_decision_count\": 0\n",
            "  }\n",
            "}\n",
            "JSON\n"
        ),
    )?;
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

    let start_output = run_darc_with_env(
        [
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
        ],
        [("DARC_WIKI_CODEX_BIN", codex.as_os_str())],
    )?;

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

    let cancel_output = run_darc_with_env(
        [
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
        ],
        [("DARC_WIKI_CODEX_BIN", codex.as_os_str())],
    )?;
    assert!(cancel_output.status.success());

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_digest_succeeds_after_valid_codex_proposal() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-runtime-success")?;
    let codex = write_fake_cli(
        &root,
        "fake-codex-success",
        concat!(
            "output=\"\"\n",
            "prompt=\"\"\n",
            "while [ \"$#\" -gt 0 ]; do\n",
            "  case \"$1\" in\n",
            "    -o|--output-last-message)\n",
            "      output=\"$2\"\n",
            "      shift 2\n",
            "      ;;\n",
            "    --output-schema|--model|-m|--cd|-C|--sandbox)\n",
            "      shift 2\n",
            "      ;;\n",
            "    --skip-git-repo-check|--ephemeral|exec)\n",
            "      shift\n",
            "      ;;\n",
            "    *)\n",
            "      prompt=\"$1\"\n",
            "      shift\n",
            "      ;;\n",
            "  esac\n",
            "done\n",
            "run_id=$(printf '%s' \"$prompt\" | sed -n 's/^.*\"run_id\": \"\\(cwrun_[^\"]*\\)\".*$/\\1/p' | head -n 1)\n",
            "printf 'codex runtime stdout\\n'\n",
            "printf 'codex runtime stderr\\n' >&2\n",
            "cat > \"$output\" <<JSON\n",
            "{\n",
            "  \"schema\": \"darc.wiki.digest.proposal.v1\",\n",
            "  \"project_id\": \"repo-abc123\",\n",
            "  \"run_id\": \"$run_id\",\n",
            "  \"entries\": [\n",
            "    {\n",
            "      \"operation\": \"create\",\n",
            "      \"entry_type\": \"decision_trace\",\n",
            "      \"title\": \"Keep the query protocol additive\",\n",
            "      \"category\": \"product\",\n",
            "      \"domains\": [\"query-protocol\"],\n",
            "      \"decision_date\": \"2026-04-13\",\n",
            "      \"context\": \"The selected session discussed stable read-side contracts.\",\n",
            "      \"options\": [\n",
            "        {\"status\": \"chosen\", \"description\": \"Keep new query fields additive.\"},\n",
            "        {\"status\": \"rejected\", \"description\": \"Ship breaking protocol changes.\"}\n",
            "      ],\n",
            "      \"final_decision\": \"Keep the query protocol additive.\",\n",
            "      \"rationale\": \"Desktop already depends on the current v1 query shape.\",\n",
            "      \"consequences\": \"Future changes need additive migration paths.\",\n",
            "      \"evidence\": [\"codex:session-1#0\"]\n",
            "    }\n",
            "  ],\n",
            "  \"run_summary\": {\n",
            "    \"title\": \"Validated additive query decision\",\n",
            "    \"summary\": \"The session contained one durable product decision.\",\n",
            "    \"themes\": [\"query stability\"],\n",
            "    \"extracted_decision_count\": 1\n",
            "  }\n",
            "}\n",
            "JSON\n"
        ),
    )?;

    let start_output = run_darc_with_env(
        [
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
            "--target-domain",
            "query-protocol",
            "--json",
        ],
        [("DARC_WIKI_CODEX_BIN", codex.as_os_str())],
    )?;
    assert!(start_output.status.success());
    let start_value = parse_json(&start_output.stdout, "stdout")?;
    let run_id = start_value["data"]["run_id"]
        .as_str()
        .context("missing run id")?
        .to_owned();

    let runs_value = wait_for_run_status(&root, &run_id, "succeeded")?;
    let succeeded = runs_value["data"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["run_id"] == run_id)
        .context("succeeded run should be visible")?;
    assert_eq!(succeeded["status"], "succeeded");

    let run_dir = root
        .join("context-wiki/projects/repo-abc123/runs")
        .join(&run_id);
    let run_toml = fs::read_to_string(run_dir.join("run.toml"))?;
    assert!(run_toml.contains("status = \"succeeded\""));
    let proposal = fs::read_to_string(run_dir.join("proposal.json"))?;
    assert!(proposal.contains("\"entry_type\": \"decision_trace\""));
    let result = fs::read_to_string(run_dir.join("result.json"))?;
    assert!(result.contains("\"valid\": true"));
    assert!(fs::read_to_string(run_dir.join("agent.stdout.log"))?.contains("codex runtime stdout"));

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_digest_fails_on_invalid_claude_proposal() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-runtime-invalid")?;
    let claude = write_fake_cli(
        &root,
        "fake-claude-invalid",
        concat!(
            "prompt=\"\"\n",
            "while [ \"$#\" -gt 0 ]; do\n",
            "  case \"$1\" in\n",
            "    --model|--output-format|--json-schema|--permission-mode|--tools)\n",
            "      shift 2\n",
            "      ;;\n",
            "    --print|--no-session-persistence)\n",
            "      shift\n",
            "      ;;\n",
            "    *)\n",
            "      prompt=\"$1\"\n",
            "      shift\n",
            "      ;;\n",
            "  esac\n",
            "done\n",
            "run_id=$(printf '%s' \"$prompt\" | sed -n 's/^.*\"run_id\": \"\\(cwrun_[^\"]*\\)\".*$/\\1/p' | head -n 1)\n",
            "cat <<JSON\n",
            "{\n",
            "  \"schema\": \"darc.wiki.digest.proposal.v1\",\n",
            "  \"project_id\": \"repo-abc123\",\n",
            "  \"run_id\": \"$run_id\",\n",
            "  \"entries\": [\n",
            "    {\n",
            "      \"operation\": \"create\",\n",
            "      \"entry_type\": \"decision_trace\",\n",
            "      \"title\": \"Bad domain\",\n",
            "      \"category\": \"product\",\n",
            "      \"domains\": [\"not-allowed\"],\n",
            "      \"decision_date\": \"2026-04-13\",\n",
            "      \"context\": \"Context\",\n",
            "      \"options\": [{\"status\": \"chosen\", \"description\": \"Chosen\"}],\n",
            "      \"final_decision\": \"Decision\",\n",
            "      \"rationale\": \"Rationale\",\n",
            "      \"consequences\": \"Consequences\",\n",
            "      \"evidence\": [\"claude:session-2#0\"]\n",
            "    }\n",
            "  ],\n",
            "  \"run_summary\": {\n",
            "    \"title\": \"Invalid summary\",\n",
            "    \"summary\": \"Should fail domain validation.\",\n",
            "    \"themes\": [\"validation\"],\n",
            "    \"extracted_decision_count\": 1\n",
            "  }\n",
            "}\n",
            "JSON\n"
        ),
    )?;

    let start_output = run_darc_with_env(
        [
            "wiki",
            "digest",
            "start",
            "--root",
            root.to_string_lossy().as_ref(),
            "--project-id",
            "repo-abc123",
            "--session-ref",
            "claude:session-2",
            "--agent",
            "claude",
            "--runtime",
            "external-cli",
            "--model",
            "claude-sonnet-4-6",
            "--json",
        ],
        [("DARC_WIKI_CLAUDE_BIN", claude.as_os_str())],
    )?;
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

    let run_dir = root
        .join("context-wiki/projects/repo-abc123/runs")
        .join(&run_id);
    let run_toml = fs::read_to_string(run_dir.join("run.toml"))?;
    assert!(run_toml.contains("error_code = \"proposal_validation_failed\""));
    let result = fs::read_to_string(run_dir.join("result.json"))?;
    assert!(result.contains("\"valid\": false"));
    assert!(result.contains("entries[0].domains[0]"));

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_digest_fails_when_runtime_cannot_be_invoked() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-runtime-failure")?;

    let start_output = run_darc_with_env(
        [
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
        ],
        [(
            "DARC_WIKI_CODEX_BIN",
            root.join("missing-codex").as_os_str(),
        )],
    )?;
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

    let run_dir = root
        .join("context-wiki/projects/repo-abc123/runs")
        .join(&run_id);
    let run_toml = fs::read_to_string(run_dir.join("run.toml"))?;
    assert!(run_toml.contains("error_code = \"runtime_invocation_failed\""));
    let result = fs::read_to_string(run_dir.join("result.json"))?;
    assert!(result.contains("\"error_code\": \"runtime_invocation_failed\""));

    remove_root(&root)?;
    Ok(())
}
