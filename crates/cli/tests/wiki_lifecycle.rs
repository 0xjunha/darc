use std::collections::BTreeSet;
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
use darc_core::{EntryFrontmatter, EntryId, EntryStatus, EntryType, RunId, ensure_project_wiki};
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

/// Writes one project registry domains fixture for wiki lifecycle tests.
fn write_registry_domains(root: &Path, domains: &[&str]) -> Result<()> {
    let wiki_root = root.join("context-wiki");
    let registry_dir = root.join("context-wiki/projects/repo-abc123/registry");
    fs::create_dir_all(wiki_root.join("projects"))?;
    fs::create_dir_all(&registry_dir)?;
    write_file(&wiki_root.join("VERSION"), "1\n")?;
    let domains = domains
        .iter()
        .map(|domain| format!("\"{domain}\""))
        .collect::<Vec<_>>()
        .join(", ");
    write_file(
        &registry_dir.join("domains.toml"),
        &format!("schema_version = 1\ndomains = [{domains}]\n"),
    )
}

/// Writes one canonical wiki entry fixture for entry lifecycle command tests.
fn write_entry_fixture(
    root: &Path,
    entry_id: &str,
    display_id: &str,
    status: EntryStatus,
) -> Result<()> {
    let layout = ensure_project_wiki(Some(root.to_path_buf()), "repo-abc123")?;
    let entry_id = EntryId::new(entry_id)?;
    let frontmatter = EntryFrontmatter {
        schema_version: 1,
        entry_id: entry_id.clone(),
        entry_type: EntryType::DecisionTrace,
        display_id: Some(display_id.to_owned()),
        project_id: "repo-abc123".to_owned(),
        title: "Keep the query protocol additive".to_owned(),
        category: "product".to_owned(),
        domains: vec!["query-protocol".to_owned()],
        status,
        created_at: "2026-04-13T10:00:00Z".to_owned(),
        updated_at: "2026-04-13T10:00:00Z".to_owned(),
        decision_date: Some("2026-04-13".to_owned()),
        evidence: vec!["codex:session-1#0".to_owned()],
        content_fingerprint: None,
        created_by_run_id: RunId::new("cwrun_01entryfixture")?,
        updated_by_run_id: RunId::new("cwrun_01entryfixture")?,
        supersedes: Vec::new(),
    };
    let mut content =
        toml::to_string_pretty(&frontmatter).context("failed to serialize entry fixture")?;
    if !content.ends_with('\n') {
        content.push('\n');
    }
    write_file(
        &layout.entry_path("product", &entry_id),
        &format!(
            concat!(
                "+++\n",
                "{content}",
                "+++\n\n",
                "## Context\n\n",
                "Desktop already depends on the current query protocol.\n\n",
                "## Options Considered\n\n",
                "1. Chosen: Keep the protocol additive.\n",
                "2. Rejected: Ship breaking changes.\n\n",
                "## Final Decision\n\n",
                "Keep the protocol additive.\n\n",
                "## Rationale\n\n",
                "Downstream tools already consume the stable shape.\n\n",
                "## Consequences\n\n",
                "Future changes must stay additive.\n\n",
                "## Evidence\n\n",
                "- `codex:session-1#0`\n"
            ),
            content = content
        ),
    )
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
    wait_for_run_status_with_timeout(root, run_id, expected_status, Duration::from_secs(6))
}

/// Polls the wiki runs query until one run reaches the requested status within the timeout.
fn wait_for_run_status_with_timeout(
    root: &Path,
    run_id: &str,
    expected_status: &str,
    timeout: Duration,
) -> Result<Value> {
    let deadline = Instant::now() + timeout;
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
            let reached = value["data"]["runs"]
                .as_array()
                .map(|runs| {
                    runs.iter()
                        .any(|run| run["run_id"] == run_id && run["status"] == expected_status)
                })
                .unwrap_or(false);
            if reached {
                return Ok(value);
            }
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for run `{run_id}` to reach status `{expected_status}`");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Polls one filesystem path until it exists or the timeout expires.
fn wait_for_path(path: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if path.exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for path `{}`", path.display());
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
    let marker = root.join("fake-codex-slow.started");
    let codex = write_fake_cli(
        &root,
        "fake-codex-slow",
        &format!(
            concat!(
                "output=\"\"\n",
                "unexpected=\"\"\n",
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
                "      unexpected=\"$unexpected $1\"\n",
                "      shift\n",
                "      ;;\n",
                "  esac\n",
                "done\n",
                "prompt=$(cat)\n",
                "[ -z \"$unexpected\" ] || {{ echo \"unexpected args:$unexpected\" >&2; exit 64; }}\n",
                "touch \"{}\"\n",
                "while :; do :; done\n",
                "run_id=$(basename \"$(dirname \"$output\")\")\n",
                "cat > \"$output\" <<JSON\n",
                "{{\n",
                "  \"schema\": \"darc.wiki.digest.proposal.v1\",\n",
                "  \"project_id\": \"repo-abc123\",\n",
                "  \"run_id\": \"$run_id\",\n",
                "  \"entries\": [],\n",
                "  \"run_summary\": {{\n",
                "    \"title\": \"Canceled run\",\n",
                "    \"summary\": \"Canceled before validation completed.\",\n",
                "    \"themes\": [\"cancellation\"],\n",
                "    \"extracted_decision_count\": 0\n",
                "  }}\n",
                "}}\n",
                "JSON\n"
            ),
            marker.display()
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
    assert!(!run_dir.join("context.json").exists());
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
    wait_for_path(&marker)?;

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
    let result_path = run_dir.join("result.json");
    wait_for_path(&result_path)?;
    let result = fs::read_to_string(result_path)?;
    assert!(result.contains("\"status\": \"canceled\""));

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_entry_discard_and_restore_round_trip() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-entry-roundtrip")?;
    write_entry_fixture(&root, "cw_01entryroundtrip", "DT-1", EntryStatus::Active)?;

    let discard_output = run_darc([
        "wiki",
        "entry",
        "discard",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--entry-id",
        "cw_01entryroundtrip",
        "--json",
    ])?;

    assert!(discard_output.status.success());
    let discard_value = parse_json(&discard_output.stdout, "stdout")?;
    assert_eq!(discard_value["schema"], "darc.wiki.entry.discard.v1");
    assert_eq!(discard_value["data"]["entry_id"], "cw_01entryroundtrip");
    assert_eq!(discard_value["data"]["previous_status"], "active");
    assert_eq!(discard_value["data"]["status"], "discarded");
    assert_eq!(discard_value["data"]["changed"], true);

    let query_discarded = run_darc([
        "query",
        "wiki",
        "entry",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--entry-id",
        "cw_01entryroundtrip",
        "--json",
    ])?;
    assert!(query_discarded.status.success());
    let discarded_entry = parse_json(&query_discarded.stdout, "stdout")?;
    assert_eq!(discarded_entry["data"]["entry"]["status"], "discarded");
    let entry_path =
        root.join("context-wiki/projects/repo-abc123/entries/product/cw_01entryroundtrip.md");
    assert!(fs::read_to_string(&entry_path)?.contains("status = \"discarded\""));

    let restore_output = run_darc([
        "wiki",
        "entry",
        "restore",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--entry-id",
        "cw_01entryroundtrip",
        "--json",
    ])?;

    assert!(restore_output.status.success());
    let restore_value = parse_json(&restore_output.stdout, "stdout")?;
    assert_eq!(restore_value["schema"], "darc.wiki.entry.restore.v1");
    assert_eq!(restore_value["data"]["previous_status"], "discarded");
    assert_eq!(restore_value["data"]["status"], "active");
    assert_eq!(restore_value["data"]["changed"], true);

    let query_restored = run_darc([
        "query",
        "wiki",
        "entry",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--entry-id",
        "cw_01entryroundtrip",
        "--json",
    ])?;
    assert!(query_restored.status.success());
    let restored_entry = parse_json(&query_restored.stdout, "stdout")?;
    assert_eq!(restored_entry["data"]["entry"]["status"], "active");
    assert!(fs::read_to_string(&entry_path)?.contains("status = \"active\""));

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_entry_restore_rejects_duplicate_active_identity() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-entry-restore-conflict")?;
    write_entry_fixture(&root, "cw_01entryactive", "DT-1", EntryStatus::Active)?;
    write_entry_fixture(&root, "cw_01entrydiscarded", "DT-2", EntryStatus::Discarded)?;

    let output = run_darc([
        "wiki",
        "entry",
        "restore",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--entry-id",
        "cw_01entrydiscarded",
        "--json",
    ])?;

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("active entry `cw_01entryactive` already has the same canonical identity")
    );

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
            "unexpected=\"\"\n",
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
            "      unexpected=\"$unexpected $1\"\n",
            "      shift\n",
            "      ;;\n",
            "  esac\n",
            "done\n",
            "prompt=$(cat)\n",
            "[ -z \"$unexpected\" ] || { echo \"unexpected args:$unexpected\" >&2; exit 64; }\n",
            "sleep 2\n",
            "run_id=$(basename \"$(dirname \"$output\")\")\n",
            "cat > \"$output\" <<JSON\n",
            "{\n",
            "  \"schema\": \"darc.wiki.digest.proposal.v1\",\n",
            "  \"project_id\": \"repo-abc123\",\n",
            "  \"run_id\": \"$run_id\",\n",
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
    let _ = wait_for_run_status(&root, &new_run_id, "canceled")?;

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_digest_start_rejects_unregistered_target_domain() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-target-domain-validation")?;

    let output = run_darc([
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
    ])?;

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("target domain `query-protocol` is not defined in the project registry")
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_digest_start_rejects_codex_provider_auth_opt_in() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-codex-provider-auth")?;
    let output = Command::new(darc_binary())
        .args([
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
            "--use-provider-auth",
            "--json",
        ])
        .output()
        .context("failed to run compiled darc binary with codex provider-auth opt-in")?;

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("does not expose a documented per-run API-key/provider-auth selector")
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_digest_succeeds_after_valid_codex_proposal() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-runtime-success")?;
    write_registry_domains(&root, &["query-protocol"])?;
    let codex = write_fake_cli(
        &root,
        "fake-codex-success",
        concat!(
            "output=\"\"\n",
            "unexpected=\"\"\n",
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
            "      unexpected=\"$unexpected $1\"\n",
            "      shift\n",
            "      ;;\n",
            "  esac\n",
            "done\n",
            "prompt=$(cat)\n",
            "[ -z \"$unexpected\" ] || { echo \"unexpected args:$unexpected\" >&2; exit 64; }\n",
            "run_id=$(basename \"$(dirname \"$output\")\")\n",
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
    assert!(result.contains("\"status\": \"succeeded\""));
    assert!(result.contains("\"valid\": true"));
    assert!(fs::read_to_string(run_dir.join("agent.stdout.log"))?.contains("codex runtime stdout"));
    assert!(run_toml.contains("digest_id = \"dg_"));

    let entries_output = run_darc([
        "query",
        "wiki",
        "entries",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;
    assert!(entries_output.status.success());
    let entries_value = parse_json(&entries_output.stdout, "stdout")?;
    let entries = entries_value["data"]["entries"]
        .as_array()
        .context("entries query should return an array")?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["display_id"], "DT-1");
    let entry_id = entries[0]["entry_id"]
        .as_str()
        .context("entry id should be present")?
        .to_owned();

    let entry_output = run_darc([
        "query",
        "wiki",
        "entry",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--entry-id",
        &entry_id,
        "--json",
    ])?;
    assert!(entry_output.status.success());
    let entry_value = parse_json(&entry_output.stdout, "stdout")?;
    assert!(
        entry_value["data"]["entry"]["body_markdown"]
            .as_str()
            .unwrap()
            .contains("## Final Decision")
    );

    let digests_output = run_darc([
        "query",
        "wiki",
        "digests",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;
    assert!(digests_output.status.success());
    let digests_value = parse_json(&digests_output.stdout, "stdout")?;
    let digests = digests_value["data"]["digests"]
        .as_array()
        .context("digests query should return an array")?;
    assert_eq!(digests.len(), 1);
    let digest_id = digests[0]["digest_id"]
        .as_str()
        .context("digest id should be present")?
        .to_owned();

    let digest_output = run_darc([
        "query",
        "wiki",
        "digest",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--digest-id",
        &digest_id,
        "--json",
    ])?;
    assert!(digest_output.status.success());
    let digest_value = parse_json(&digest_output.stdout, "stdout")?;
    assert!(
        digest_value["data"]["digest"]["body_markdown"]
            .as_str()
            .unwrap()
            .contains("## Summary")
    );

    let run_output = run_darc([
        "query",
        "wiki",
        "run",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--run-id",
        &run_id,
        "--json",
    ])?;
    assert!(run_output.status.success());
    let run_value = parse_json(&run_output.stdout, "stdout")?;
    let run = &run_value["data"]["run"];
    assert_eq!(run["created_entry_ids"][0], entry_id);
    assert_eq!(run["updated_entry_ids"], Value::Array(vec![]));
    assert_eq!(run["digest_id"], digest_id);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_digest_reuses_existing_canonical_entry_on_repeated_runs() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-runtime-dedupe")?;
    write_registry_domains(&root, &["query-protocol"])?;
    let codex = write_fake_cli(
        &root,
        "fake-codex-dedupe",
        concat!(
            "output=\"\"\n",
            "unexpected=\"\"\n",
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
            "      unexpected=\"$unexpected $1\"\n",
            "      shift\n",
            "      ;;\n",
            "  esac\n",
            "done\n",
            "prompt=$(cat)\n",
            "[ -z \"$unexpected\" ] || { echo \"unexpected args:$unexpected\" >&2; exit 64; }\n",
            "run_id=$(basename \"$(dirname \"$output\")\")\n",
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

    let mut run_ids = Vec::new();
    for _ in 0..2 {
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
        let _ = wait_for_run_status(&root, &run_id, "succeeded")?;
        run_ids.push(run_id);
    }

    let entries_output = run_darc([
        "query",
        "wiki",
        "entries",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;
    assert!(entries_output.status.success());
    let entries_value = parse_json(&entries_output.stdout, "stdout")?;
    let entries = entries_value["data"]["entries"]
        .as_array()
        .context("entries query should return an array")?;
    assert_eq!(entries.len(), 1);
    let entry_id = entries[0]["entry_id"]
        .as_str()
        .context("entry id should be present")?
        .to_owned();

    let first_run_output = run_darc([
        "query",
        "wiki",
        "run",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--run-id",
        &run_ids[0],
        "--json",
    ])?;
    assert!(first_run_output.status.success());
    let first_run_value = parse_json(&first_run_output.stdout, "stdout")?;
    assert_eq!(
        first_run_value["data"]["run"]["created_entry_ids"][0],
        entry_id
    );
    assert_eq!(
        first_run_value["data"]["run"]["updated_entry_ids"],
        Value::Array(vec![])
    );

    let second_run_output = run_darc([
        "query",
        "wiki",
        "run",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--run-id",
        &run_ids[1],
        "--json",
    ])?;
    assert!(second_run_output.status.success());
    let second_run_value = parse_json(&second_run_output.stdout, "stdout")?;
    assert_eq!(
        second_run_value["data"]["run"]["created_entry_ids"],
        Value::Array(vec![])
    );
    assert_eq!(
        second_run_value["data"]["run"]["updated_entry_ids"][0],
        entry_id
    );

    let digests_output = run_darc([
        "query",
        "wiki",
        "digests",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;
    assert!(digests_output.status.success());
    let digests_value = parse_json(&digests_output.stdout, "stdout")?;
    assert_eq!(
        digests_value["data"]["digests"]
            .as_array()
            .context("digests query should return an array")?
            .len(),
        2
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_digest_serializes_canonical_merge_for_overlapping_runs() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-runtime-concurrent")?;
    write_registry_domains(&root, &["query-protocol"])?;
    let barrier_dir = root.join("fake-codex-overlap-barrier");
    let codex = write_fake_cli(
        &root,
        "fake-codex-overlap",
        &format!(
            concat!(
                "output=\"\"\n",
                "unexpected=\"\"\n",
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
                "      unexpected=\"$unexpected $1\"\n",
                "      shift\n",
                "      ;;\n",
                "  esac\n",
                "done\n",
                "prompt=$(cat)\n",
                "[ -z \"$unexpected\" ] || {{ echo \"unexpected args:$unexpected\" >&2; exit 64; }}\n",
                "run_id=$(basename \"$(dirname \"$output\")\")\n",
                "barrier_dir=\"{barrier_dir}\"\n",
                "mkdir -p \"$barrier_dir\"\n",
                "touch \"$barrier_dir/$run_id.ready\"\n",
                "attempts=0\n",
                "while true; do\n",
                "  ready_count=$(find \"$barrier_dir\" -name '*.ready' | wc -l | tr -d '[:space:]')\n",
                "  [ \"$ready_count\" -ge 2 ] && break\n",
                "  attempts=$((attempts + 1))\n",
                "  [ \"$attempts\" -lt 6 ] || {{ echo \"timed out waiting for overlapping runtime\" >&2; exit 70; }}\n",
                "  sleep 1\n",
                "done\n",
                "sleep 1\n",
                "printf 'codex overlap stdout for %s\\n' \"$run_id\"\n",
                "printf 'codex overlap stderr for %s\\n' \"$run_id\" >&2\n",
                "cat > \"$output\" <<JSON\n",
                "{{\n",
                "  \"schema\": \"darc.wiki.digest.proposal.v1\",\n",
                "  \"project_id\": \"repo-abc123\",\n",
                "  \"run_id\": \"$run_id\",\n",
                "  \"entries\": [\n",
                "    {{\n",
                "      \"operation\": \"create\",\n",
                "      \"entry_type\": \"decision_trace\",\n",
                "      \"title\": \"Keep the query protocol additive\",\n",
                "      \"category\": \"product\",\n",
                "      \"domains\": [\"query-protocol\"],\n",
                "      \"decision_date\": \"2026-04-13\",\n",
                "      \"context\": \"The selected session discussed stable read-side contracts.\",\n",
                "      \"options\": [\n",
                "        {{\"status\": \"chosen\", \"description\": \"Keep new query fields additive.\"}},\n",
                "        {{\"status\": \"rejected\", \"description\": \"Ship breaking protocol changes.\"}}\n",
                "      ],\n",
                "      \"final_decision\": \"Keep the query protocol additive.\",\n",
                "      \"rationale\": \"Desktop already depends on the current v1 query shape.\",\n",
                "      \"consequences\": \"Future changes need additive migration paths.\",\n",
                "      \"evidence\": [\"codex:session-1#0\"]\n",
                "    }}\n",
                "  ],\n",
                "  \"run_summary\": {{\n",
                "    \"title\": \"Validated additive query decision\",\n",
                "    \"summary\": \"The session contained one durable product decision.\",\n",
                "    \"themes\": [\"query stability\"],\n",
                "    \"extracted_decision_count\": 1\n",
                "  }}\n",
                "}}\n",
                "JSON\n"
            ),
            barrier_dir = barrier_dir.to_string_lossy(),
        ),
    )?;

    let mut run_ids = Vec::new();
    for _ in 0..2 {
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
        run_ids.push(run_id);
    }

    let json_string_array = |value: &Value, field: &str| -> Result<Vec<String>> {
        value[field]
            .as_array()
            .with_context(|| format!("{field} should be an array"))?
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .with_context(|| format!("{field} should contain strings"))
            })
            .collect()
    };
    let toml_string_array = |value: &toml::Value, field: &str| -> Result<Vec<String>> {
        value[field]
            .as_array()
            .with_context(|| format!("run.toml {field} should be an array"))?
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .with_context(|| format!("run.toml {field} should contain strings"))
            })
            .collect()
    };

    let _ =
        wait_for_run_status_with_timeout(&root, &run_ids[0], "succeeded", Duration::from_secs(10))?;
    let _ =
        wait_for_run_status_with_timeout(&root, &run_ids[1], "succeeded", Duration::from_secs(10))?;

    let entries_output = run_darc([
        "query",
        "wiki",
        "entries",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;
    assert!(entries_output.status.success());
    let entries_value = parse_json(&entries_output.stdout, "stdout")?;
    let entries = entries_value["data"]["entries"]
        .as_array()
        .context("entries query should return an array")?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["display_id"], "DT-1");
    let entry_id = entries[0]["entry_id"]
        .as_str()
        .context("entry id should be present")?
        .to_owned();

    let digests_output = run_darc([
        "query",
        "wiki",
        "digests",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;
    assert!(digests_output.status.success());
    let digests_value = parse_json(&digests_output.stdout, "stdout")?;
    let digests = digests_value["data"]["digests"]
        .as_array()
        .context("digests query should return an array")?;
    assert_eq!(digests.len(), 2);
    let digest_ids = digests
        .iter()
        .map(|digest| {
            digest["digest_id"]
                .as_str()
                .map(str::to_owned)
                .context("digest id should be present")
        })
        .collect::<Result<BTreeSet<_>>>()?;
    assert_eq!(digest_ids.len(), 2);
    let digest_run_ids = digests
        .iter()
        .map(|digest| {
            assert_eq!(digest["extracted_decision_count"], 1);
            digest["run_id"]
                .as_str()
                .map(str::to_owned)
                .context("digest run id should be present")
        })
        .collect::<Result<BTreeSet<_>>>()?;
    assert_eq!(
        digest_run_ids,
        run_ids.iter().cloned().collect::<BTreeSet<_>>()
    );

    let mut created_runs = 0;
    let mut updated_runs = 0;
    let mut run_digest_ids = BTreeSet::new();
    for run_id in &run_ids {
        let run_output = run_darc([
            "query",
            "wiki",
            "run",
            "--root",
            root.to_string_lossy().as_ref(),
            "--project-id",
            "repo-abc123",
            "--run-id",
            run_id,
            "--json",
        ])?;
        assert!(run_output.status.success());
        let run_value = parse_json(&run_output.stdout, "stdout")?;
        let run = &run_value["data"]["run"];
        assert_eq!(run["run_id"], run_id.as_str());
        assert_eq!(run["status"], "succeeded");
        assert_eq!(run["use_provider_auth"], false);
        assert_eq!(
            run["selected_sessions"],
            Value::Array(vec![Value::String("codex:session-1".to_owned())])
        );
        assert_eq!(
            run["target_domains"],
            Value::Array(vec![Value::String("query-protocol".to_owned())])
        );
        assert_eq!(run["result"]["status"], "succeeded");
        assert_eq!(run["result"]["validation"]["valid"], true);
        assert_eq!(run["result"]["runtime"]["exit_code"], 0);
        assert_eq!(run["result"]["runtime"]["proposal_captured"], true);
        assert_eq!(run["result"]["runtime"]["use_provider_auth"], false);
        let digest_id = run["digest_id"]
            .as_str()
            .context("run digest id should be present")?
            .to_owned();
        assert!(digest_ids.contains(&digest_id));
        run_digest_ids.insert(digest_id.clone());

        let created_entry_ids = json_string_array(run, "created_entry_ids")?;
        let updated_entry_ids = json_string_array(run, "updated_entry_ids")?;
        match (created_entry_ids.as_slice(), updated_entry_ids.as_slice()) {
            ([created], []) if created == &entry_id => created_runs += 1,
            ([], [updated]) if updated == &entry_id => updated_runs += 1,
            _ => bail!("run `{run_id}` should either create or update the shared canonical entry"),
        }

        let run_dir = root
            .join("context-wiki/projects/repo-abc123/runs")
            .join(run_id);
        let run_toml: toml::Value = toml::from_str(&fs::read_to_string(run_dir.join("run.toml"))?)
            .context("failed to parse run.toml")?;
        assert_eq!(run_toml["status"].as_str(), Some("succeeded"));
        assert_eq!(run_toml["digest_id"].as_str(), Some(digest_id.as_str()));
        assert_eq!(run_toml["result_path"].as_str(), Some("result.json"));
        assert_eq!(
            toml_string_array(&run_toml, "created_entry_ids")?,
            created_entry_ids
        );
        assert_eq!(
            toml_string_array(&run_toml, "updated_entry_ids")?,
            updated_entry_ids
        );

        let result_value = parse_json(&fs::read(run_dir.join("result.json"))?, "result.json")?;
        assert_eq!(result_value["project_id"], "repo-abc123");
        assert_eq!(result_value["run_id"], run_id.as_str());
        assert_eq!(result_value["status"], "succeeded");
        assert_eq!(result_value["validation"]["valid"], true);
        assert_eq!(result_value["runtime"]["exit_code"], 0);
        assert_eq!(result_value["runtime"]["proposal_captured"], true);
    }

    assert_eq!(created_runs, 1);
    assert_eq!(updated_runs, 1);
    assert_eq!(run_digest_ids.len(), 2);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_digest_can_succeed_without_creating_decision_traces() -> Result<()> {
    let root = create_wiki_fixture_root("cli-wiki-runtime-zero")?;
    write_registry_domains(&root, &["query-protocol"])?;
    let codex = write_fake_cli(
        &root,
        "fake-codex-zero",
        concat!(
            "output=\"\"\n",
            "unexpected=\"\"\n",
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
            "      unexpected=\"$unexpected $1\"\n",
            "      shift\n",
            "      ;;\n",
            "  esac\n",
            "done\n",
            "prompt=$(cat)\n",
            "[ -z \"$unexpected\" ] || { echo \"unexpected args:$unexpected\" >&2; exit 64; }\n",
            "run_id=$(basename \"$(dirname \"$output\")\")\n",
            "cat > \"$output\" <<JSON\n",
            "{\n",
            "  \"schema\": \"darc.wiki.digest.proposal.v1\",\n",
            "  \"project_id\": \"repo-abc123\",\n",
            "  \"run_id\": \"$run_id\",\n",
            "  \"entries\": [],\n",
            "  \"run_summary\": {\n",
            "    \"title\": \"No durable decisions\",\n",
            "    \"summary\": \"The selected sessions did not contain any durable decisions worth preserving.\",\n",
            "    \"themes\": [\"routine execution\"],\n",
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
    let _ = wait_for_run_status(&root, &run_id, "succeeded")?;

    let entries_output = run_darc([
        "query",
        "wiki",
        "entries",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;
    assert!(entries_output.status.success());
    let entries_value = parse_json(&entries_output.stdout, "stdout")?;
    assert_eq!(entries_value["data"]["entries"], Value::Array(vec![]));

    let digests_output = run_darc([
        "query",
        "wiki",
        "digests",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;
    assert!(digests_output.status.success());
    let digests_value = parse_json(&digests_output.stdout, "stdout")?;
    let digest_id = digests_value["data"]["digests"][0]["digest_id"]
        .as_str()
        .context("digest id should be present")?
        .to_owned();

    let digest_output = run_darc([
        "query",
        "wiki",
        "digest",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--digest-id",
        &digest_id,
        "--json",
    ])?;
    assert!(digest_output.status.success());
    let digest_value = parse_json(&digest_output.stdout, "stdout")?;
    assert!(
        digest_value["data"]["digest"]["body_markdown"]
            .as_str()
            .unwrap()
            .contains("No durable decision-trace entries")
    );

    let run_output = run_darc([
        "query",
        "wiki",
        "run",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--run-id",
        &run_id,
        "--json",
    ])?;
    assert!(run_output.status.success());
    let run_value = parse_json(&run_output.stdout, "stdout")?;
    let run = &run_value["data"]["run"];
    assert_eq!(run["created_entry_ids"], Value::Array(vec![]));
    assert_eq!(run["updated_entry_ids"], Value::Array(vec![]));
    assert_eq!(run["digest_id"], digest_id);

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
            "unexpected=\"\"\n",
            "while [ \"$#\" -gt 0 ]; do\n",
            "  case \"$1\" in\n",
            "    --model|--input-format|--output-format|--json-schema|--permission-mode|--tools|--allowed-tools|--add-dir)\n",
            "      shift 2\n",
            "      ;;\n",
            "    --print|--bare|--strict-mcp-config|--disable-slash-commands|--no-session-persistence|--no-chrome)\n",
            "      shift\n",
            "      ;;\n",
            "    *)\n",
            "      unexpected=\"$unexpected $1\"\n",
            "      shift\n",
            "      ;;\n",
            "  esac\n",
            "done\n",
            "prompt=$(cat)\n",
            "[ -z \"$unexpected\" ] || { echo \"unexpected args:$unexpected\" >&2; exit 64; }\n",
            "run_id=$(printf '%s\\n' \"$prompt\" | awk -F'`' '/Set `run_id` to / { print $4; exit }')\n",
            "[ -n \"$run_id\" ] || { echo \"missing run_id in prompt\" >&2; exit 64; }\n",
            "cat <<JSON\n",
            "{\n",
            "  \"result\": \"done\",\n",
            "  \"structured_output\": {\n",
            "    \"schema\": \"darc.wiki.digest.proposal.v1\",\n",
            "    \"project_id\": \"repo-abc123\",\n",
            "    \"run_id\": \"$run_id\",\n",
            "    \"entries\": [\n",
            "      {\n",
            "        \"operation\": \"create\",\n",
            "        \"entry_type\": \"decision_trace\",\n",
            "        \"title\": \"Bad domain\",\n",
            "        \"category\": \"product\",\n",
            "        \"domains\": [\"not-allowed\"],\n",
            "        \"decision_date\": \"2026-04-13\",\n",
            "        \"context\": \"Context\",\n",
            "        \"options\": [{\"status\": \"chosen\", \"description\": \"Chosen\"}],\n",
            "        \"final_decision\": \"Decision\",\n",
            "        \"rationale\": \"Rationale\",\n",
            "        \"consequences\": \"Consequences\",\n",
            "        \"evidence\": [\"claude:session-2#0\"]\n",
            "      }\n",
            "    ],\n",
            "    \"run_summary\": {\n",
            "      \"title\": \"Invalid summary\",\n",
            "      \"summary\": \"Should fail domain validation.\",\n",
            "      \"themes\": [\"validation\"],\n",
            "      \"extracted_decision_count\": 1\n",
            "    }\n",
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
            "--use-provider-auth",
            "--json",
        ],
        [
            ("ANTHROPIC_API_KEY", std::ffi::OsStr::new("test-key")),
            ("DARC_WIKI_CLAUDE_BIN", claude.as_os_str()),
        ],
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
    let proposal = fs::read_to_string(run_dir.join("proposal.json"))?;
    assert!(!proposal.contains("\"structured_output\""));
    assert!(proposal.contains("\"domains\":[\"not-allowed\"]"));
    let result = fs::read_to_string(run_dir.join("result.json"))?;
    assert!(result.contains("\"status\": \"failed\""));
    assert!(result.contains("\"valid\": false"));
    assert!(result.contains("entries[0].domains[0]"));

    let query_output = run_darc([
        "query",
        "wiki",
        "run",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--run-id",
        &run_id,
        "--json",
    ])?;
    assert!(query_output.status.success());
    let query_value = parse_json(&query_output.stdout, "stdout")?;
    assert_eq!(query_value["schema"], "darc.query.wiki.run.v1");
    assert_eq!(query_value["data"]["run"]["run_id"], run_id);
    assert_eq!(query_value["data"]["run"]["use_provider_auth"], true);
    assert_eq!(
        query_value["data"]["run"]["error_code"],
        "proposal_validation_failed"
    );
    assert_eq!(query_value["data"]["run"]["result"]["status"], "failed");
    assert_eq!(
        query_value["data"]["run"]["result"]["runtime"]["use_provider_auth"],
        true
    );
    assert_eq!(
        query_value["data"]["run"]["result"]["validation"]["valid"],
        false
    );
    assert_eq!(
        query_value["data"]["run"]["result"]["validation"]["errors"][0]["path"],
        "entries[0].domains[0]"
    );

    remove_root(&root)?;
    Ok(())
}

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
    assert!(result.contains("\"status\": \"failed\""));
    assert!(result.contains("\"error_code\": \"runtime_invocation_failed\""));

    remove_root(&root)?;
    Ok(())
}
