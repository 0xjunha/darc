use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use darc_index::open_index_database;
use darc_paths::SourceKind;
use darc_test_utils::{
    IndexedSessionFixture, IndexedTurnFixture, insert_indexed_session, insert_indexed_turn,
    unique_test_dir, write_file,
};
use serde_json::Value;

/// Stores one minimal darc config fixture written for CLI protocol tests.
#[derive(serde::Serialize)]
struct ConfigFixture {
    version: u32,
    root: String,
    projects: Vec<ProjectFixture>,
}

/// Stores one configured project fixture written for CLI protocol tests.
#[derive(serde::Serialize)]
struct ProjectFixture {
    id: String,
    name: String,
    local_path: String,
    sessions_root: String,
    known_paths: Vec<String>,
}

/// Builds one temporary darc root for CLI protocol tests.
fn test_root(prefix: &str) -> PathBuf {
    unique_test_dir(prefix)
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

/// Builds one configured project fixture for CLI protocol tests.
fn project_fixture(root: &Path, project_id: &str) -> ProjectFixture {
    let project_root = root.join("repo");
    let sessions_root = root.join(format!("projects/{project_id}/sessions"));
    ProjectFixture {
        id: project_id.to_owned(),
        name: "repo".to_owned(),
        local_path: project_root.to_string_lossy().into_owned(),
        sessions_root: sessions_root.to_string_lossy().into_owned(),
        known_paths: Vec::new(),
    }
}

/// Creates one minimal darc root fixture with one configured project and one indexed session.
fn create_query_fixture_root(prefix: &str) -> Result<PathBuf> {
    let root = test_root(prefix);
    let project_root = root.join("repo");
    let sessions_root = root.join("projects/repo-abc123/sessions");
    fs::create_dir_all(&project_root)?;
    fs::create_dir_all(&sessions_root)?;

    write_config_fixture(&root, vec![project_fixture(&root, "repo-abc123")])?;

    let connection = open_index_database(&root.join("index.sqlite"))?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new(
            "repo-abc123",
            SourceKind::Codex,
            "session-1",
            project_root.to_string_lossy().as_ref(),
        ),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            turn_id: Some("turn-1"),
            completed_at: Some("2026-04-06T10:00:05Z"),
            user_message: "Inspect the repository status",
            final_answer_at: Some("2026-04-06T10:00:05Z"),
            final_answer_text: Some("Done."),
            step_count: 2,
            tool_call_count: 1,
            tool_output_count: 1,
            attachment_count: 0,
            delegation_count: 0,
            hook_summary_count: 0,
            has_final_answer: true,
            duration_ms: 5_000,
            effective_agent_runtime_ms: Some(6_500),
            provider_total_token_count: Some(300),
            input_uncached_token_count: Some(120),
            cache_read_token_count: Some(80),
            cache_write_token_count: None,
            output_token_count: Some(121),
            reasoning_token_count: Some(20),
            total_token_count: Some(321),
            primary_model: Some("gpt-5.4"),
            changed_file_count: 1,
            added_line_count: 2,
            removed_line_count: 1,
            ..IndexedTurnFixture::new(
                "repo-abc123",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"},{"type":"tool_call_output","timestamp":"2026-04-06T10:00:02Z","call_id":"call-1","output":"# Repo"}]"##,
            )
        },
    )?;
    connection.execute_batch("PRAGMA user_version = 1")?;

    Ok(root)
}

/// Adds one extra indexed session fixture at one specified latest-turn timestamp.
fn insert_query_fixture_session(root: &Path, session_id: &str, started_at: &str) -> Result<()> {
    let connection = open_index_database(&root.join("index.sqlite"))?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-abc123", SourceKind::Codex, session_id, "/tmp/repo"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture::new(
            "repo-abc123",
            SourceKind::Codex,
            session_id,
            0,
            started_at,
            "completed",
            "[]",
        ),
    )?;
    Ok(())
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

#[test]
fn workspace_query_emits_success_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-workspace")?;
    let output = run_darc([
        "query",
        "workspace",
        "--root",
        root.to_string_lossy().as_ref(),
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.workspace.v1");
    assert_eq!(value["data"]["projects"][0]["id"], "repo-abc123");
    assert_eq!(value["data"]["projects"][0]["session_count"], 1);
    assert_eq!(value["data"]["projects"][0]["turn_count"], 1);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_registry_query_emits_success_envelope_without_database() -> Result<()> {
    let root = create_query_fixture_root("cli-query-wiki-registry")?;
    fs::remove_file(root.join("index.sqlite"))?;

    let output = run_darc([
        "query",
        "wiki",
        "registry",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.wiki.registry.v1");
    assert_eq!(value["data"]["project_id"], "repo-abc123");
    assert_eq!(value["data"]["schema_version"], 1);
    assert_eq!(
        value["data"]["categories"],
        serde_json::json!(["architecture", "data", "product", "process"])
    );
    assert_eq!(value["data"]["domains"], Value::Array(vec![]));
    assert!(!root.join("context-wiki").exists());

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_registry_query_rejects_invalid_configured_project_id() -> Result<()> {
    let root = create_query_fixture_root("cli-query-wiki-invalid-id")?;
    write_config_fixture(&root, vec![project_fixture(&root, "../../escaped-project")])?;

    let output = run_darc([
        "query",
        "wiki",
        "registry",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "../../escaped-project",
        "--json",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid")
    );
    assert!(!root.join("escaped-project").exists());
    assert!(!root.join("context-wiki").exists());

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_registry_query_rejects_storage_version_mismatch() -> Result<()> {
    let root = create_query_fixture_root("cli-query-wiki-version-mismatch")?;
    write_file(&root.join("context-wiki/VERSION"), "999\n")?;

    let output = run_darc([
        "query",
        "wiki",
        "registry",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unsupported storage version")
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_registry_query_rejects_registry_schema_version_mismatch() -> Result<()> {
    let root = create_query_fixture_root("cli-query-wiki-registry-schema")?;
    write_file(&root.join("context-wiki/VERSION"), "1\n")?;
    write_file(
        &root.join("context-wiki/projects/repo-abc123/registry/categories.toml"),
        "schema_version = 999\ncategories = [\"architecture\"]\n",
    )?;

    let output = run_darc([
        "query",
        "wiki",
        "registry",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unsupported schema version")
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_entries_query_emits_empty_success_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-wiki-entries")?;
    let output = run_darc([
        "query",
        "wiki",
        "entries",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.wiki.entries.v1");
    assert_eq!(value["data"]["project_id"], "repo-abc123");
    assert_eq!(value["data"]["entries"], Value::Array(vec![]));

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_entries_query_rejects_cross_project_entry() -> Result<()> {
    let root = create_query_fixture_root("cli-query-wiki-entries-mismatch")?;
    write_file(&root.join("context-wiki/VERSION"), "1\n")?;
    write_file(
        &root.join("context-wiki/projects/repo-abc123/entries/product/cw_01entry.md"),
        concat!(
            "+++\n",
            "schema_version = 1\n",
            "entry_id = \"cw_01entry\"\n",
            "entry_type = \"decision_trace\"\n",
            "project_id = \"other-project\"\n",
            "title = \"Misplaced entry\"\n",
            "category = \"product\"\n",
            "domains = []\n",
            "status = \"active\"\n",
            "created_at = \"2026-04-13T10:00:00Z\"\n",
            "updated_at = \"2026-04-13T10:00:00Z\"\n",
            "decision_date = \"2026-04-13\"\n",
            "evidence = []\n",
            "created_by_run_id = \"cwrun_01entry\"\n",
            "updated_by_run_id = \"cwrun_01entry\"\n",
            "supersedes = []\n",
            "+++\n",
            "\n",
            "body\n"
        ),
    )?;

    let output = run_darc([
        "query",
        "wiki",
        "entries",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("belongs to project")
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_runs_query_emits_api_shaped_success_envelope_without_registry_dependency() -> Result<()> {
    let root = create_query_fixture_root("cli-query-wiki-runs")?;
    write_file(&root.join("context-wiki/VERSION"), "1\n")?;
    write_file(
        &root.join("context-wiki/projects/repo-abc123/registry/domains.toml"),
        "=\n",
    )?;
    write_file(
        &root.join("context-wiki/projects/repo-abc123/runs/cwrun_01run/run.toml"),
        concat!(
            "schema_version = 1\n",
            "run_id = \"cwrun_01run\"\n",
            "project_id = \"repo-abc123\"\n",
            "status = \"queued\"\n",
            "phase = \"preparing_context\"\n",
            "created_at = \"2026-04-13T10:00:00Z\"\n",
            "updated_at = \"2026-04-13T10:00:00Z\"\n",
            "attempt = 1\n",
            "cancel_requested = false\n",
            "selected_sessions = []\n",
            "target_categories = []\n",
            "target_domains = []\n",
            "created_entry_ids = []\n",
            "updated_entry_ids = []\n"
        ),
    )?;

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

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.wiki.runs.v1");
    assert_eq!(value["data"]["project_id"], "repo-abc123");
    let run = &value["data"]["runs"][0];
    let run = run.as_object().expect("run should be an object");
    assert_eq!(run["run_id"], "cwrun_01run");
    assert_eq!(run["status"], "queued");
    assert_eq!(run["phase"], "preparing_context");
    assert!(run.contains_key("finished_at"));
    assert!(run["finished_at"].is_null());
    assert!(run.contains_key("headline"));
    assert!(run["headline"].is_null());
    assert!(!run.contains_key("run_dir"));
    assert!(!run.contains_key("run_state_path"));

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_runs_query_repairs_stale_running_runs_to_interrupted() -> Result<()> {
    let root = create_query_fixture_root("cli-query-wiki-runs-stale")?;
    write_file(&root.join("context-wiki/VERSION"), "1\n")?;
    write_file(
        &root.join("context-wiki/projects/repo-abc123/runs/cwrun_01stale/run.toml"),
        concat!(
            "schema_version = 1\n",
            "run_id = \"cwrun_01stale\"\n",
            "project_id = \"repo-abc123\"\n",
            "status = \"running\"\n",
            "phase = \"waiting_for_agent\"\n",
            "created_at = \"2026-04-13T10:00:00Z\"\n",
            "started_at = \"2026-04-13T10:00:01Z\"\n",
            "updated_at = \"2026-04-13T10:00:02Z\"\n",
            "heartbeat_at = \"2026-04-13T10:00:02Z\"\n",
            "attempt = 1\n",
            "cancel_requested = false\n",
            "pid = 4294967295\n",
            "selected_sessions = []\n",
            "target_categories = []\n",
            "target_domains = []\n",
            "created_entry_ids = []\n",
            "updated_entry_ids = []\n"
        ),
    )?;

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

    assert!(output.status.success());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.wiki.runs.v1");
    assert_eq!(value["data"]["runs"][0]["run_id"], "cwrun_01stale");
    assert_eq!(value["data"]["runs"][0]["status"], "interrupted");
    let run_toml = fs::read_to_string(
        root.join("context-wiki/projects/repo-abc123/runs/cwrun_01stale/run.toml"),
    )?;
    assert!(run_toml.contains("status = \"interrupted\""));
    assert!(run_toml.contains("error_code = \"worker_interrupted\""));

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_digests_query_emits_empty_success_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-wiki-digests")?;
    let output = run_darc([
        "query",
        "wiki",
        "digests",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.wiki.digests.v1");
    assert_eq!(value["data"]["project_id"], "repo-abc123");
    assert_eq!(value["data"]["digests"], Value::Array(vec![]));

    remove_root(&root)?;
    Ok(())
}

#[test]
fn wiki_digests_query_rejects_cross_project_digest() -> Result<()> {
    let root = create_query_fixture_root("cli-query-wiki-digests-mismatch")?;
    write_file(&root.join("context-wiki/VERSION"), "1\n")?;
    write_file(
        &root.join("context-wiki/projects/repo-abc123/digests/dg_01digest.md"),
        concat!(
            "+++\n",
            "schema_version = 1\n",
            "digest_id = \"dg_01digest\"\n",
            "project_id = \"other-project\"\n",
            "run_id = \"cwrun_01digest\"\n",
            "title = \"Misplaced digest\"\n",
            "created_at = \"2026-04-13T10:00:00Z\"\n",
            "updated_at = \"2026-04-13T10:00:00Z\"\n",
            "extracted_decision_count = 0\n",
            "+++\n",
            "\n",
            "body\n"
        ),
    )?;

    let output = run_darc([
        "query",
        "wiki",
        "digests",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("belongs to project")
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn sessions_query_emits_success_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-sessions")?;
    let output = run_darc([
        "query",
        "sessions",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.sessions.v1");
    assert_eq!(value["data"]["project_id"], "repo-abc123");
    assert_eq!(value["data"]["since"], Value::Null);
    assert_eq!(value["data"]["until"], Value::Null);
    assert_eq!(value["data"]["touched_path"], Value::Null);
    assert_eq!(value["data"]["sessions"][0]["session_id"], "session-1");
    assert_eq!(value["data"]["sessions"][0]["latest_status"], "completed");
    assert_eq!(value["data"]["sessions"][0]["primary_model"], "gpt-5.4");
    assert_eq!(
        value["data"]["sessions"][0]["first_turn_at"],
        "2026-04-06T10:00:00Z"
    );
    assert_eq!(
        value["data"]["sessions"][0]["first_user_prompt"],
        "Inspect the repository status"
    );
    assert_eq!(value["data"]["sessions"][0]["aborted_turn_count"], 0);
    assert_eq!(
        value["data"]["sessions"][0]["edited_files"],
        Value::Array(vec![])
    );
    assert_eq!(value["data"]["sessions"][0]["total_token_count"], 321);
    assert_eq!(
        value["data"]["sessions"][0]["token_usage"]["input_uncached_token_count"],
        120
    );
    assert_eq!(
        value["data"]["sessions"][0]["token_usage"]["cache_read_token_count"],
        80
    );
    assert_eq!(
        value["data"]["sessions"][0]["token_usage"]["cache_write_token_count"],
        Value::Null
    );
    assert_eq!(
        value["data"]["sessions"][0]["token_usage"]["output_token_count"],
        121
    );
    assert_eq!(
        value["data"]["sessions"][0]["token_usage"]["reasoning_token_count"],
        20
    );
    assert_eq!(
        value["data"]["sessions"][0]["token_usage"]["provider_total_token_count"],
        300
    );
    assert_eq!(
        value["data"]["sessions"][0]["token_usage"]["normalized_total_token_count"],
        321
    );
    assert_eq!(
        value["data"]["sessions"][0]["effective_agent_runtime_ms"],
        6500
    );
    assert_eq!(value["data"]["sessions"][0]["changed_file_count"], 1);
    assert_eq!(value["data"]["sessions"][0]["added_line_count"], 2);
    assert_eq!(value["data"]["sessions"][0]["removed_line_count"], 1);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn sessions_query_applies_touched_path_filter() -> Result<()> {
    let root = create_query_fixture_root("cli-query-sessions-touched-path")?;
    insert_query_fixture_session(&root, "session-2", "2026-04-07T10:00:00Z")?;

    let output = run_darc([
        "query",
        "sessions",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--touched-path",
        "README.md",
        "--json",
    ])?;

    assert!(output.status.success());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.sessions.v1");
    assert_eq!(value["data"]["touched_path"], "README.md");
    assert_eq!(
        value["data"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["session_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["session-1"]
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn files_query_path_mode_emits_success_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-files-path")?;

    let output = run_darc([
        "query",
        "files",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--path",
        "README.md",
        "--json",
    ])?;

    assert!(output.status.success());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.files.v1");
    assert_eq!(value["data"]["project_id"], "repo-abc123");
    assert_eq!(value["data"]["mode"], "path");
    assert_eq!(value["data"]["path"], "README.md");
    assert_eq!(value["data"]["co_touched_with"], Value::Null);
    assert_eq!(value["data"]["sessions"][0]["session_id"], "session-1");
    assert_eq!(value["data"]["sessions"][0]["touch_count"], 1);
    assert_eq!(
        value["data"]["sessions"][0]["matched_paths"],
        serde_json::json!(["README.md"])
    );
    assert_eq!(value["data"]["files"], Value::Array(vec![]));

    remove_root(&root)?;
    Ok(())
}

#[test]
fn session_files_query_emits_success_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-session-files")?;

    let output = run_darc([
        "query",
        "session-files",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--session-id",
        "session-1",
        "--json",
    ])?;

    assert!(output.status.success());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.session_files.v1");
    assert_eq!(value["data"]["project_id"], "repo-abc123");
    assert_eq!(value["data"]["provider"], "codex");
    assert_eq!(value["data"]["session_id"], "session-1");
    assert_eq!(value["data"]["files"][0]["path"], "README.md");
    assert_eq!(value["data"]["files"][0]["read_count"], 1);
    assert_eq!(value["data"]["files"][0]["write_count"], 0);
    assert_eq!(value["data"]["files"][0]["first_turn_ordinal"], 0);
    assert_eq!(value["data"]["files"][0]["last_turn_ordinal"], 0);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn sessions_query_includes_first_turn_abort_counts_and_edited_files() -> Result<()> {
    let root = create_query_fixture_root("cli-query-sessions-fields")?;
    let connection = open_index_database(&root.join("index.sqlite"))?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            turn_id: Some("turn-2"),
            completed_at: Some("2026-04-06T10:10:05Z"),
            user_message: "Cancel the in-flight change",
            step_count: 0,
            duration_ms: 5_000,
            ..IndexedTurnFixture::new(
                "repo-abc123",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-06T10:10:00Z",
                "aborted",
                "[]",
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            turn_id: Some("turn-3"),
            completed_at: Some("2026-04-06T10:20:05Z"),
            user_message: "Write the follow-up patch",
            step_count: 4,
            tool_call_count: 4,
            duration_ms: 5_000,
            has_final_answer: true,
            ..IndexedTurnFixture::new(
                "repo-abc123",
                SourceKind::Codex,
                "session-1",
                2,
                "2026-04-06T10:20:00Z",
                "completed",
                r#"[{"type":"tool_call","timestamp":"2026-04-06T10:20:01Z","call_id":"call-2","name":"Write","arguments":"{\"file_path\":\"src/z.rs\"}"},{"type":"tool_call","timestamp":"2026-04-06T10:20:02Z","call_id":"call-3","name":"Edit","arguments":"{\"file_path\":\"src/a.rs\"}"},{"type":"tool_call","timestamp":"2026-04-06T10:20:03Z","call_id":"call-4","name":"Edit","arguments":"{\"file_path\":\"src/a.rs\"}"},{"type":"tool_call","timestamp":"2026-04-06T10:20:04Z","call_id":"call-5","name":"Read","arguments":"{\"file_path\":\"README.md\"}"}]"#,
            )
        },
    )?;
    drop(connection);

    let output = run_darc([
        "query",
        "sessions",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.sessions.v1");
    assert_eq!(
        value["data"]["sessions"][0]["first_turn_at"],
        "2026-04-06T10:00:00Z"
    );
    assert_eq!(
        value["data"]["sessions"][0]["first_user_prompt"],
        "Inspect the repository status"
    );
    assert_eq!(value["data"]["sessions"][0]["aborted_turn_count"], 1);
    assert_eq!(
        value["data"]["sessions"][0]["edited_files"],
        serde_json::json!(["src/a.rs", "src/z.rs"])
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn sessions_query_applies_since_and_until_filters() -> Result<()> {
    let root = create_query_fixture_root("cli-query-sessions-bounds")?;
    insert_query_fixture_session(&root, "session-0", "2026-04-05T10:00:00Z")?;
    insert_query_fixture_session(&root, "session-2", "2026-04-07T10:00:00Z")?;

    let since_output = run_darc([
        "query",
        "sessions",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--since",
        "2026-04-06T00:00:00Z",
        "--json",
    ])?;
    assert!(since_output.status.success());
    let since_value = parse_json(&since_output.stdout, "stdout")?;
    assert_eq!(
        since_value["data"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["session_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["session-2", "session-1"]
    );

    let until_output = run_darc([
        "query",
        "sessions",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--until",
        "2026-04-07T00:00:00Z",
        "--json",
    ])?;
    assert!(until_output.status.success());
    let until_value = parse_json(&until_output.stdout, "stdout")?;
    assert_eq!(
        until_value["data"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["session_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["session-1", "session-0"]
    );

    let bounded_output = run_darc([
        "query",
        "sessions",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--since",
        "2026-04-06T00:00:00Z",
        "--until",
        "2026-04-07T00:00:00Z",
        "--json",
    ])?;
    assert!(bounded_output.status.success());
    let bounded_value = parse_json(&bounded_output.stdout, "stdout")?;
    assert_eq!(
        bounded_value["data"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["session_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["session-1"]
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turns_query_emits_success_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turns")?;
    let output = run_darc([
        "query",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--session-id",
        "session-1",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.turns.v1");
    assert_eq!(value["data"]["turns"][0]["turn_id"], "turn-1");
    assert_eq!(value["data"]["turns"][0]["step_count"], 2);
    assert_eq!(value["data"]["turns"][0]["primary_model"], "gpt-5.4");
    assert_eq!(value["data"]["turns"][0]["total_token_count"], 321);
    assert_eq!(
        value["data"]["turns"][0]["token_usage"]["input_uncached_token_count"],
        120
    );
    assert_eq!(
        value["data"]["turns"][0]["token_usage"]["cache_read_token_count"],
        80
    );
    assert_eq!(
        value["data"]["turns"][0]["token_usage"]["cache_write_token_count"],
        Value::Null
    );
    assert_eq!(
        value["data"]["turns"][0]["token_usage"]["output_token_count"],
        121
    );
    assert_eq!(
        value["data"]["turns"][0]["token_usage"]["reasoning_token_count"],
        20
    );
    assert_eq!(
        value["data"]["turns"][0]["token_usage"]["provider_total_token_count"],
        300
    );
    assert_eq!(
        value["data"]["turns"][0]["token_usage"]["normalized_total_token_count"],
        321
    );
    assert_eq!(
        value["data"]["turns"][0]["effective_agent_runtime_ms"],
        6500
    );
    assert_eq!(value["data"]["turns"][0]["changed_file_count"], 1);
    assert_eq!(value["data"]["turns"][0]["added_line_count"], 2);
    assert_eq!(value["data"]["turns"][0]["removed_line_count"], 1);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turns_query_grep_emits_match_and_context_rows() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turns-grep")?;
    let connection = open_index_database(&root.join("index.sqlite"))?;
    let matched_path = root.join("repo/crates/index/src/index_db/schema.rs");
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            turn_id: Some("turn-2"),
            completed_at: Some("2026-04-06T10:05:05Z"),
            user_message: "Please switch to staged init for the index bootstrap",
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 5_000,
            ..IndexedTurnFixture::new(
                "repo-abc123",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-06T10:05:00Z",
                "completed",
                &format!(
                    r#"[{{"type":"tool_call","timestamp":"2026-04-06T10:05:01Z","call_id":"call-2","name":"Read","arguments":"{{\"path\":\"{}\"}}"}}]"#,
                    matched_path.display()
                ),
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            turn_id: Some("turn-3"),
            completed_at: Some("2026-04-06T10:10:05Z"),
            user_message: "Apply the follow-up update after that",
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 5_000,
            ..IndexedTurnFixture::new(
                "repo-abc123",
                SourceKind::Codex,
                "session-1",
                2,
                "2026-04-06T10:10:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:10:01Z","call_id":"call-3","name":"Read","arguments":"{\"file_path\":\"Cargo.toml\"}"}]"##,
            )
        },
    )?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-abc123", SourceKind::Codex, "session-2", "/tmp/repo"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Please switch to staged init for the docs too",
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 5_000,
            ..IndexedTurnFixture::new(
                "repo-abc123",
                SourceKind::Codex,
                "session-2",
                0,
                "2026-04-06T11:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T11:00:01Z","call_id":"call-4","name":"Read","arguments":"{\"file_path\":\"docs/query-protocol.md\"}"}]"##,
            )
        },
    )?;

    let output = run_darc([
        "query",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--grep",
        "staged init",
        "--role",
        "user",
        "--context",
        "1",
        "--since",
        "2026-04-06T00:00:00Z",
        "--until",
        "2026-04-07T00:00:00Z",
        "--touched-path",
        "crates/index/**",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.turn_matches.v1");
    assert_eq!(value["data"]["provider"], Value::Null);
    assert_eq!(value["data"]["session_id"], Value::Null);
    assert_eq!(value["data"]["role"], "user");
    assert_eq!(value["data"]["context"], 1);
    assert_eq!(
        value["data"]["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| turn["turn_ordinal"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(value["data"]["turns"][0]["match_kind"], "context");
    assert_eq!(value["data"]["turns"][1]["match_kind"], "match");
    assert_eq!(value["data"]["turns"][2]["match_kind"], "context");
    assert_eq!(value["data"]["turns"][0]["match_snippet"], Value::Null);
    assert!(
        value["data"]["turns"][1]["match_snippet"]
            .as_str()
            .is_some_and(|snippet| snippet.contains("staged init"))
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turns_query_grep_rejects_context_over_limit() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turns-grep-limit")?;
    let output = run_darc([
        "query",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--grep",
        "staged init",
        "--context",
        "51",
        "--json",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("--context must be at most 50 turns"))
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turn_query_emits_success_envelope_and_raw_field() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turn")?;
    let output = run_darc([
        "query",
        "turn",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--session-id",
        "session-1",
        "--turn-ordinal",
        "0",
        "--include-raw",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.turn.v1");
    assert_eq!(value["data"]["session_id"], "session-1");
    assert_eq!(value["data"]["steps"][0]["type"], "tool_call");
    assert!(value["data"]["raw_steps_json"].is_string());
    assert!(value["data"]["insights"].is_null());

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turn_query_can_embed_derived_insights() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turn-with-insights")?;
    let output = run_darc([
        "query",
        "turn",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--session-id",
        "session-1",
        "--turn-ordinal",
        "0",
        "--include-insights",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.turn.v1");
    assert_eq!(value["data"]["step_count"], 2);
    assert_eq!(value["data"]["insights"]["primary_model"], "gpt-5.4");
    assert_eq!(value["data"]["insights"]["duration_ms"], 5_000);
    assert_eq!(
        value["data"]["insights"]["effective_agent_runtime_ms"],
        6_500
    );
    assert_eq!(value["data"]["insights"]["total_token_count"], 321);
    assert_eq!(
        value["data"]["insights"]["token_usage"]["input_uncached_token_count"],
        120
    );
    assert_eq!(
        value["data"]["insights"]["token_usage"]["cache_read_token_count"],
        80
    );
    assert_eq!(
        value["data"]["insights"]["token_usage"]["cache_write_token_count"],
        Value::Null
    );
    assert_eq!(
        value["data"]["insights"]["token_usage"]["output_token_count"],
        121
    );
    assert_eq!(
        value["data"]["insights"]["token_usage"]["reasoning_token_count"],
        20
    );
    assert_eq!(
        value["data"]["insights"]["token_usage"]["provider_total_token_count"],
        300
    );
    assert_eq!(
        value["data"]["insights"]["token_usage"]["normalized_total_token_count"],
        321
    );
    assert_eq!(value["data"]["insights"]["changed_file_count"], 1);
    assert_eq!(value["data"]["insights"]["added_line_count"], 2);
    assert_eq!(value["data"]["insights"]["removed_line_count"], 1);
    assert_eq!(value["data"]["insights"]["tool_call_count"], 1);
    assert_eq!(value["data"]["insights"]["tool_output_count"], 1);
    assert_eq!(value["data"]["insights"]["tools"][0]["name"], "Read");
    assert_eq!(value["data"]["insights"]["files"][0]["path"], "README.md");

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turn_query_narrative_view_strips_bulky_fields() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turn-narrative")?;
    let output = run_darc([
        "query",
        "turn",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--session-id",
        "session-1",
        "--turn-ordinal",
        "0",
        "--view",
        "narrative",
        "--include-raw",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.turn.v1");
    assert_eq!(value["data"]["steps"][0]["type"], "tool_call");
    assert_eq!(value["data"]["steps"][0]["arguments"], "");
    assert_eq!(value["data"]["steps"][1]["type"], "tool_call_output");
    assert_eq!(value["data"]["steps"][1]["output"], "");
    assert!(value["data"]["raw_steps_json"].is_null());

    remove_root(&root)?;
    Ok(())
}

#[test]
fn search_turns_query_emits_keyword_search_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-search-keyword")?;
    let output = run_darc([
        "query",
        "search",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--mode",
        "keyword",
        "--query",
        "Inspect",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.search.turns.v1");
    assert_eq!(value["data"]["mode"], "keyword");
    assert_eq!(value["data"]["hits"][0]["session_id"], "session-1");
    assert_eq!(
        value["data"]["hits"][0]["matched_paths"],
        Value::Array(vec![])
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn search_turns_query_emits_file_search_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-search-file")?;
    let output = run_darc([
        "query",
        "search",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--mode",
        "file-name",
        "--query",
        "README.md",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.search.turns.v1");
    assert_eq!(value["data"]["mode"], "file_name");
    assert_eq!(value["data"]["hits"][0]["matched_paths"][0], "README.md");

    let path_output = run_darc([
        "query",
        "search",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--mode",
        "file-path",
        "--query",
        "README.md",
        "--json",
    ])?;

    assert!(path_output.status.success());
    let path_value = parse_json(&path_output.stdout, "stdout")?;
    assert_eq!(path_value["schema"], "darc.query.search.turns.v1");
    assert_eq!(path_value["data"]["mode"], "file_path");
    assert_eq!(
        path_value["data"]["hits"][0]["matched_paths"][0],
        "README.md"
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn workspace_insights_query_emits_success_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-workspace-insights")?;
    let output = run_darc([
        "query",
        "insights",
        "workspace",
        "--root",
        root.to_string_lossy().as_ref(),
        "--window",
        "7d",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.insights.workspace.v1");
    assert_eq!(value["data"]["active_session_count"], 1);
    assert_eq!(value["data"]["included_turn_count"], 1);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn project_insights_query_emits_success_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-project-insights")?;
    let output = run_darc([
        "query",
        "insights",
        "project",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--limit",
        "1000",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.insights.project.v1");
    assert_eq!(value["data"]["most_common_tools"][0]["name"], "Read");
    assert_eq!(value["data"]["total_time_ms"], 5000);
    assert_eq!(
        value["data"]["most_read_files"][0]["repo_relative_path"],
        "README.md"
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turn_insights_query_emits_success_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turn-insights")?;
    let output = run_darc([
        "query",
        "insights",
        "turn",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--session-id",
        "session-1",
        "--turn-ordinal",
        "0",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.insights.turn.v1");
    assert_eq!(value["data"]["primary_model"], "gpt-5.4");
    assert_eq!(value["data"]["total_token_count"], 321);
    assert_eq!(
        value["data"]["token_usage"]["input_uncached_token_count"],
        120
    );
    assert_eq!(value["data"]["token_usage"]["cache_read_token_count"], 80);
    assert_eq!(
        value["data"]["token_usage"]["cache_write_token_count"],
        Value::Null
    );
    assert_eq!(value["data"]["token_usage"]["output_token_count"], 121);
    assert_eq!(value["data"]["token_usage"]["reasoning_token_count"], 20);
    assert_eq!(
        value["data"]["token_usage"]["provider_total_token_count"],
        300
    );
    assert_eq!(
        value["data"]["token_usage"]["normalized_total_token_count"],
        321
    );
    assert_eq!(value["data"]["effective_agent_runtime_ms"], 6500);
    assert_eq!(value["data"]["changed_file_count"], 1);
    assert_eq!(value["data"]["added_line_count"], 2);
    assert_eq!(value["data"]["removed_line_count"], 1);
    assert_eq!(value["data"]["tool_call_count"], 1);
    assert_eq!(value["data"]["tool_output_count"], 1);
    assert_eq!(value["data"]["tools"][0]["name"], "Read");
    assert_eq!(value["data"]["files"][0]["path"], "README.md");
    assert_eq!(value["data"]["files"][0]["repo_relative_path"], "README.md");
    assert_eq!(value["data"]["files"][0]["read_count"], 1);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turn_insights_query_emits_shell_commands() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turn-insights-shell")?;
    let connection = open_index_database(&root.join("index.sqlite"))?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            turn_id: Some("turn-2"),
            completed_at: Some("2026-04-06T10:10:03Z"),
            user_message: "Run shell commands",
            final_answer_at: Some("2026-04-06T10:10:03Z"),
            final_answer_text: Some("Done."),
            step_count: 1,
            tool_call_count: 1,
            tool_output_count: 0,
            attachment_count: 0,
            delegation_count: 0,
            hook_summary_count: 0,
            has_final_answer: true,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-abc123",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-06T10:10:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:10:01Z","call_id":"call-2","name":"exec_command","arguments":"{\"cmd\":\"rg -n \\\"query_turn_insights\\\" crates/query/src/query.rs -S\",\"workdir\":\"/tmp/repo\"}"}]"##,
            )
        },
    )?;
    drop(connection);

    let output = run_darc([
        "query",
        "insights",
        "turn",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--session-id",
        "session-1",
        "--turn-ordinal",
        "1",
        "--json",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.insights.turn.v1");
    assert_eq!(
        value["data"]["shell_commands"][0]["tool_name"],
        "exec_command"
    );
    assert_eq!(
        value["data"]["shell_commands"][0]["command_text"],
        r#"rg -n "query_turn_insights" crates/query/src/query.rs -S"#
    );
    assert_eq!(value["data"]["shell_commands"][0]["workdir"], "/tmp/repo");

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turn_insights_query_missing_turn_emits_error_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turn-insights-missing")?;
    let output = run_darc([
        "query",
        "insights",
        "turn",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--session-id",
        "session-1",
        "--turn-ordinal",
        "9",
        "--json",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("turn 9 was not found in session session-1 for provider codex")
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn query_errors_emit_structured_stderr_envelope() -> Result<()> {
    let root = test_root("cli-query-error");
    let missing_root = root.join("missing-root");
    let output = run_darc([
        "query",
        "sessions",
        "--root",
        missing_root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--json",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Darc root was not found")
    );

    Ok(())
}
