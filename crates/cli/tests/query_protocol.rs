use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use darc_index::{
    open_index_database,
    policy::{derive_file_access_records, extract_tool_call_records},
};
use darc_paths::SourceKind;
use darc_rollout::model::NormalizedTurnStep;
use darc_test_utils::{unique_test_dir, write_file};
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

/// Stores one normalized turn fixture used to seed CLI protocol tests.
struct TurnFixture<'a> {
    project_id: &'a str,
    provider: &'a str,
    session_id: &'a str,
    turn_ordinal: i64,
    turn_id: &'a str,
    started_at: &'a str,
    completed_at: &'a str,
    status: &'a str,
    user_message: &'a str,
    final_answer_at: Option<&'a str>,
    final_answer_text: Option<&'a str>,
    steps_json: &'a str,
    step_count: i64,
    tool_call_count: i64,
    tool_output_count: i64,
    attachment_count: i64,
    delegation_count: i64,
    hook_summary_count: i64,
    has_final_answer: bool,
    duration_ms: i64,
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
    insert_session(
        &connection,
        "repo-abc123",
        "codex",
        "session-1",
        None,
        "primary",
        project_root.to_string_lossy().as_ref(),
    )?;
    insert_turn(
        &connection,
        TurnFixture {
            project_id: "repo-abc123",
            provider: "codex",
            session_id: "session-1",
            turn_ordinal: 0,
            turn_id: "turn-1",
            started_at: "2026-04-06T10:00:00Z",
            completed_at: "2026-04-06T10:00:05Z",
            status: "completed",
            user_message: "Inspect the repository status",
            final_answer_at: Some("2026-04-06T10:00:05Z"),
            final_answer_text: Some("Done."),
            steps_json: r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"},{"type":"tool_call_output","timestamp":"2026-04-06T10:00:02Z","call_id":"call-1","output":"# Repo"}]"##,
            step_count: 2,
            tool_call_count: 1,
            tool_output_count: 1,
            attachment_count: 0,
            delegation_count: 0,
            hook_summary_count: 0,
            has_final_answer: true,
            duration_ms: 5_000,
        },
    )?;
    connection.execute_batch("PRAGMA user_version = 1")?;

    Ok(root)
}

/// Inserts one normalized session row for CLI protocol tests.
fn insert_session(
    connection: &rusqlite::Connection,
    project_id: &str,
    provider: &str,
    session_id: &str,
    parent_session_id: Option<&str>,
    session_kind: &str,
    cwd: &str,
) -> Result<()> {
    connection.execute(
        "
        INSERT INTO sessions (
            project_id,
            provider,
            session_id,
            parent_session_id,
            session_kind,
            archive_path,
            cwd,
            cli_version,
            schema_id,
            determinism,
            source_size,
            source_mtime_ms
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '0.118.0', 'fixture', 'exact', 1, 1)
        ",
        (
            project_id,
            provider,
            session_id,
            parent_session_id,
            session_kind,
            format!("{provider}/{session_id}.jsonl"),
            cwd,
        ),
    )?;
    Ok(())
}

/// Inserts one normalized turn row for CLI protocol tests.
fn insert_turn(connection: &rusqlite::Connection, fixture: TurnFixture<'_>) -> Result<()> {
    connection.execute(
        "
        INSERT INTO turns (
            project_id,
            provider,
            session_id,
            turn_ordinal,
            turn_id,
            started_at,
            completed_at,
            status,
            user_message,
            final_answer_at,
            final_answer_text,
            steps_json,
            step_count,
            tool_call_count,
            tool_output_count,
            attachment_count,
            delegation_count,
            hook_summary_count,
            has_final_answer,
            duration_ms
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
        ",
        rusqlite::params![
            fixture.project_id,
            fixture.provider,
            fixture.session_id,
            fixture.turn_ordinal,
            fixture.turn_id,
            fixture.started_at,
            fixture.completed_at,
            fixture.status,
            fixture.user_message,
            fixture.final_answer_at,
            fixture.final_answer_text,
            fixture.steps_json,
            fixture.step_count,
            fixture.tool_call_count,
            fixture.tool_output_count,
            fixture.attachment_count,
            fixture.delegation_count,
            fixture.hook_summary_count,
            i64::from(fixture.has_final_answer),
            fixture.duration_ms,
        ],
    )?;
    let provider = match fixture.provider {
        "claude" => SourceKind::Claude,
        "codex" => SourceKind::Codex,
        other => anyhow::bail!("unsupported fixture provider `{other}`"),
    };
    let turn_ordinal =
        u64::try_from(fixture.turn_ordinal).context("fixture turn ordinal must be non-negative")?;
    let steps = serde_json::from_str::<Vec<NormalizedTurnStep>>(fixture.steps_json)
        .context("fixture steps_json should parse")?;
    let tool_calls = extract_tool_call_records(
        fixture.project_id,
        provider,
        fixture.session_id,
        turn_ordinal,
        &steps,
    );
    for record in &tool_calls {
        connection.execute(
            "
            INSERT INTO tool_calls (
                project_id,
                provider,
                session_id,
                turn_ordinal,
                call_ordinal,
                call_id,
                timestamp,
                tool_name,
                arguments_text,
                output_text,
                status,
                is_error
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ",
            rusqlite::params![
                record.project_id.as_str(),
                fixture.provider,
                record.session_id.as_str(),
                i64::try_from(record.turn_ordinal)
                    .context("turn ordinal exceeds SQLite INTEGER range")?,
                i64::try_from(record.call_ordinal)
                    .context("call ordinal exceeds SQLite INTEGER range")?,
                record.call_id.as_str(),
                record.timestamp.as_str(),
                record.tool_name.as_deref(),
                record.arguments_text.as_deref(),
                record.output_text.as_deref(),
                record.status.as_deref(),
                i64::from(record.is_error),
            ],
        )?;
    }
    for record in derive_file_access_records(&tool_calls) {
        connection.execute(
            "
            INSERT INTO file_accesses (
                project_id,
                provider,
                session_id,
                turn_ordinal,
                call_ordinal,
                call_id,
                timestamp,
                tool_name,
                access_type,
                path,
                repo_relative_path
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ",
            rusqlite::params![
                record.project_id.as_str(),
                fixture.provider,
                record.session_id.as_str(),
                i64::try_from(record.turn_ordinal)
                    .context("turn ordinal exceeds SQLite INTEGER range")?,
                i64::try_from(record.call_ordinal)
                    .context("call ordinal exceeds SQLite INTEGER range")?,
                record.call_id.as_str(),
                record.timestamp.as_str(),
                record.tool_name.as_str(),
                record.access_type.as_sql_text(),
                record.path.as_str(),
                record.repo_relative_path.as_deref(),
            ],
        )?;
    }
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
    assert_eq!(value["data"]["files"][0]["read_count"], 1);

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
