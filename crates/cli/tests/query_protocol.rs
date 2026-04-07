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

/// Creates one minimal darc root fixture with one configured project and one indexed session.
fn create_query_fixture_root(prefix: &str) -> Result<PathBuf> {
    let root = test_root(prefix);
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
    assert_eq!(value["data"]["sessions"][0]["session_id"], "session-1");
    assert_eq!(value["data"]["sessions"][0]["latest_status"], "completed");

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
    assert_eq!(value["data"]["insights"]["duration_ms"], 5_000);
    assert_eq!(value["data"]["insights"]["tool_call_count"], 1);
    assert_eq!(value["data"]["insights"]["tool_output_count"], 1);
    assert_eq!(value["data"]["insights"]["tools"][0]["name"], "Read");
    assert_eq!(value["data"]["insights"]["files"][0]["path"], "README.md");

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
