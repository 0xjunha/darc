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
use serde_json::{Value, json};

const PRIMARY_SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";
const SECONDARY_SESSION_ID: &str = "11111111-1111-4111-8111-111111111112";
const TERTIARY_SESSION_ID: &str = "11111111-1111-4111-8111-111111111113";
const UNKNOWN_SESSION_ID: &str = "11111111-1111-4111-8111-1111111111ff";
const PRIMARY_SESSION_PREFIX: &str = "11111111";

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
            PRIMARY_SESSION_ID,
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
                PRIMARY_SESSION_ID,
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
    insert_query_fixture_provider_session(root, SourceKind::Codex, session_id, started_at)
}

/// Adds one extra indexed session fixture for one provider at one specified latest-turn timestamp.
fn insert_query_fixture_provider_session(
    root: &Path,
    provider: SourceKind,
    session_id: &str,
    started_at: &str,
) -> Result<()> {
    let connection = open_index_database(&root.join("index.sqlite"))?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-abc123", provider, session_id, "/tmp/repo"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture::new(
            "repo-abc123",
            provider,
            session_id,
            0,
            started_at,
            "completed",
            "[]",
        ),
    )?;
    Ok(())
}

/// Adds one indexed turn that references a configured-repo file by absolute path.
fn insert_absolute_project_file_read_turn(root: &Path, turn_ordinal: i64) -> Result<()> {
    let connection = open_index_database(&root.join("index.sqlite"))?;
    let file_path = root.join("repo/src/lib.rs").to_string_lossy().into_owned();
    let arguments = json!({ "file_path": file_path }).to_string();
    let steps_json = json!([
        {
            "type": "tool_call",
            "timestamp": "2026-04-06T10:05:01Z",
            "call_id": "call-absolute",
            "name": "Read",
            "arguments": arguments
        }
    ])
    .to_string();
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            turn_id: Some("turn-absolute"),
            completed_at: Some("2026-04-06T10:05:05Z"),
            user_message: "Read one absolute project file",
            final_answer_at: Some("2026-04-06T10:05:05Z"),
            final_answer_text: Some("Done."),
            step_count: 1,
            tool_call_count: 1,
            tool_output_count: 0,
            duration_ms: 2_000,
            ..IndexedTurnFixture::new(
                "repo-abc123",
                SourceKind::Codex,
                PRIMARY_SESSION_ID,
                turn_ordinal,
                "2026-04-06T10:05:00Z",
                "completed",
                &steps_json,
            )
        },
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

/// Runs the compiled `darc` binary from one explicit current directory.
fn run_darc_in_dir<I, S>(current_dir: &Path, args: I) -> Result<std::process::Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new(darc_binary())
        .current_dir(current_dir)
        .args(args)
        .output()
        .context("failed to run compiled darc binary")
}

/// Parses one UTF-8 JSON value from captured process output.
fn parse_json(bytes: &[u8], stream: &str) -> Result<Value> {
    serde_json::from_slice(bytes).with_context(|| format!("failed to parse {stream} JSON"))
}

/// Returns whether captured output contains an ANSI control sequence.
fn contains_ansi(bytes: &[u8]) -> bool {
    bytes.windows(2).any(|window| window == b"\x1b[")
}

/// Returns whether captured output highlights one visible search-match string.
fn contains_highlighted_text(bytes: &[u8], text: &str) -> bool {
    String::from_utf8_lossy(bytes).contains(&format!("\x1b[1;95m{text}\x1b[0m\x1b[32m"))
}

/// Strips ANSI control sequences from captured process output.
fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\x1b' && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    output
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
fn workspace_query_color_flags_preserve_json_contract() -> Result<()> {
    let root = create_query_fixture_root("cli-query-color")?;
    let root_arg = root.to_string_lossy();

    let default_output = run_darc(["query", "workspace", "--root", root_arg.as_ref()])?;
    assert!(default_output.status.success());
    assert!(!contains_ansi(&default_output.stdout));
    let default_value = parse_json(&default_output.stdout, "stdout")?;
    assert_eq!(default_value["schema"], "darc.query.workspace.v1");

    let never_output = run_darc([
        "query",
        "--color",
        "never",
        "workspace",
        "--root",
        root_arg.as_ref(),
    ])?;
    assert!(never_output.status.success());
    assert!(!contains_ansi(&never_output.stdout));
    let never_value = parse_json(&never_output.stdout, "stdout")?;
    assert_eq!(never_value["schema"], "darc.query.workspace.v1");

    let always_output = run_darc([
        "query",
        "--color",
        "always",
        "workspace",
        "--root",
        root_arg.as_ref(),
    ])?;
    assert!(always_output.status.success());
    assert!(contains_ansi(&always_output.stdout));
    let stripped = strip_ansi(&always_output.stdout);
    let always_value = parse_json(&stripped, "stripped stdout")?;
    assert_eq!(always_value["schema"], "darc.query.workspace.v1");

    let always_with_no_color = Command::new(darc_binary())
        .env("NO_COLOR", "1")
        .args([
            "query",
            "--color",
            "always",
            "workspace",
            "--root",
            root_arg.as_ref(),
        ])
        .output()
        .context("failed to run compiled darc binary")?;
    assert!(always_with_no_color.status.success());
    assert!(contains_ansi(&always_with_no_color.stdout));

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
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.sessions.v1");
    assert_eq!(value["data"]["project_id"], "repo-abc123");
    assert_eq!(value["data"]["since"], Value::Null);
    assert_eq!(value["data"]["until"], Value::Null);
    assert_eq!(value["data"]["touched_path"], Value::Null);
    assert_eq!(value["data"]["view"], "compact");
    assert_eq!(
        value["data"]["sessions"][0]["session_id"],
        PRIMARY_SESSION_ID
    );
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
    assert_eq!(
        value["data"]["sessions"][0]["first_user_prompt_truncated"],
        false
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
fn sessions_query_defaults_project_id_from_current_directory() -> Result<()> {
    let root = create_query_fixture_root("cli-query-sessions-default-project")?;
    let project_root = root.join("repo");
    let output = run_darc_in_dir(
        &project_root,
        [
            "query",
            "sessions",
            "--root",
            root.to_string_lossy().as_ref(),
        ],
    )?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.sessions.v1");
    assert_eq!(value["data"]["project_id"], "repo-abc123");
    assert_eq!(value["data"]["limit"], 50);
    assert_eq!(value["data"]["offset"], 0);
    assert_eq!(value["data"]["has_more"], false);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn sessions_query_without_project_id_rejects_unconfigured_current_directory() -> Result<()> {
    let root = create_query_fixture_root("cli-query-sessions-missing-active-project")?;
    let output = run_darc_in_dir(
        &root,
        [
            "query",
            "sessions",
            "--root",
            root.to_string_lossy().as_ref(),
        ],
    )?;

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert!(
        value["error"]["message"]
            .as_str()
            .expect("message should be a string")
            .contains("current directory does not match any configured darc project")
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn query_parse_errors_emit_structured_json() -> Result<()> {
    let output = run_darc([
        "query",
        "search",
        "turns",
        "foo",
        "--mode",
        "literal",
        "--include-all-matched-paths",
        "--matched-path-limit",
        "3",
    ])?;

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert_eq!(value["error"]["code"], "invalid_arguments");
    assert_eq!(value["error"]["details"]["clap_kind"], "ArgumentConflict");
    let message = value["error"]["message"]
        .as_str()
        .context("error message should be a string")?;
    assert!(message.contains("--include-all-matched-paths"));
    assert!(message.contains("--matched-path-limit"));
    Ok(())
}

#[test]
fn query_unknown_arguments_emit_structured_json() -> Result<()> {
    let output = run_darc(["query", "workspace", "--json"])?;

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert_eq!(value["error"]["code"], "invalid_arguments");
    assert_eq!(value["error"]["details"]["clap_kind"], "UnknownArgument");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("unexpected argument '--json'"))
    );
    Ok(())
}

#[test]
fn query_help_stays_clap_text() -> Result<()> {
    let output = run_darc(["query", "--help"])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage: darc query [OPTIONS] <COMMAND>"));
    assert!(stdout.contains("--color <COLOR>"));
    assert!(serde_json::from_slice::<Value>(&output.stdout).is_err());
    Ok(())
}

#[test]
fn non_query_parse_errors_stay_clap_text() -> Result<()> {
    let output = run_darc(["refresh", "--bogus"])?;

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--bogus'"));
    assert!(serde_json::from_slice::<Value>(&output.stderr).is_err());
    Ok(())
}

#[test]
fn sessions_query_applies_touched_path_filter() -> Result<()> {
    let root = create_query_fixture_root("cli-query-sessions-touched-path")?;
    insert_query_fixture_session(&root, SECONDARY_SESSION_ID, "2026-04-07T10:00:00Z")?;
    let touched_path = root.join("repo").join("README.md");
    let touched_path = touched_path.to_string_lossy().into_owned();

    let output = run_darc([
        "query",
        "sessions",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--touched-path",
        &touched_path,
    ])?;

    assert!(output.status.success());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.sessions.v1");
    assert_eq!(value["data"]["touched_path"], touched_path);
    assert_eq!(value["data"]["limit"], 50);
    assert_eq!(value["data"]["offset"], 0);
    assert_eq!(value["data"]["has_more"], false);
    assert_eq!(
        value["data"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["session_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![PRIMARY_SESSION_ID]
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn files_query_path_mode_emits_success_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-files-path")?;
    let path = root.join("repo").join("README.md");
    let path = path.to_string_lossy().into_owned();

    let output = run_darc([
        "query",
        "files",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        &path,
    ])?;

    assert!(output.status.success());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.files.v1");
    assert_eq!(value["data"]["project_id"], "repo-abc123");
    assert_eq!(value["data"]["mode"], "path");
    assert_eq!(value["data"]["path"], path);
    assert_eq!(value["data"]["co_touched_with"], Value::Null);
    assert_eq!(value["data"]["limit"], 50);
    assert_eq!(value["data"]["offset"], 0);
    assert_eq!(value["data"]["has_more"], false);
    assert_eq!(
        value["data"]["sessions"][0]["session_id"],
        PRIMARY_SESSION_ID
    );
    assert_eq!(value["data"]["sessions"][0]["touch_count"], 1);
    assert_eq!(
        value["data"]["sessions"][0]["matched_paths"],
        serde_json::json!(["README.md"])
    );
    assert_eq!(
        value["data"]["sessions"][0]["matched_paths_truncated"],
        false
    );
    assert_eq!(value["data"]["files"], Value::Array(vec![]));

    remove_root(&root)?;
    Ok(())
}

#[test]
fn files_query_without_selector_emits_most_touched_files() -> Result<()> {
    let root = create_query_fixture_root("cli-query-files-top")?;

    let output = run_darc([
        "query",
        "files",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--limit",
        "5",
    ])?;

    assert!(output.status.success());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.files.v1");
    assert_eq!(value["data"]["project_id"], "repo-abc123");
    assert_eq!(value["data"]["mode"], "top");
    assert_eq!(value["data"]["path"], Value::Null);
    assert_eq!(value["data"]["co_touched_with"], Value::Null);
    assert_eq!(value["data"]["limit"], 5);
    assert_eq!(value["data"]["sessions"], Value::Array(vec![]));
    assert_eq!(value["data"]["files"][0]["path"], "README.md");
    assert_eq!(value["data"]["files"][0]["touch_count"], 1);
    assert_eq!(value["data"]["files"][0]["session_count"], 1);
    assert_eq!(value["data"]["files"][0]["read_count"], 1);
    assert_eq!(value["data"]["files"][0]["write_count"], 0);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn files_query_rejects_explicit_empty_selector() -> Result<()> {
    let root = create_query_fixture_root("cli-query-files-empty-selector")?;

    let output = run_darc([
        "query",
        "files",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--path",
        "",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("PATH/--path cannot be empty"))
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn files_query_co_touched_mode_accepts_time_bounds() -> Result<()> {
    let root = create_query_fixture_root("cli-query-files-co-touch-time")?;

    let output = run_darc([
        "query",
        "files",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--co-touched-with",
        "README.md",
        "--since",
        "2026-04-06T09:00:00Z",
        "--until",
        "2026-04-07T00:00:00Z",
    ])?;

    assert!(output.status.success());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.files.v1");
    assert_eq!(value["data"]["project_id"], "repo-abc123");
    assert_eq!(value["data"]["mode"], "co_touched_with");
    assert_eq!(value["data"]["path"], Value::Null);
    assert_eq!(value["data"]["co_touched_with"], "README.md");
    assert_eq!(value["data"]["since"], "2026-04-06T09:00:00Z");
    assert_eq!(value["data"]["until"], "2026-04-07T00:00:00Z");
    assert_eq!(value["data"]["sessions"], Value::Array(vec![]));
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
        PRIMARY_SESSION_ID,
    ])?;

    assert!(output.status.success());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.session_files.v1");
    assert_eq!(value["data"]["project_id"], "repo-abc123");
    assert_eq!(value["data"]["provider"], "codex");
    assert_eq!(value["data"]["session_id"], PRIMARY_SESSION_ID);
    assert_eq!(value["data"]["files"][0]["path"], "README.md");
    assert_eq!(value["data"]["files"][0]["read_count"], 1);
    assert_eq!(value["data"]["files"][0]["write_count"], 0);
    assert_eq!(value["data"]["files"][0]["first_turn_ordinal"], 0);
    assert_eq!(value["data"]["files"][0]["last_turn_ordinal"], 0);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn session_bundle_query_emits_success_envelope() -> Result<()> {
    let root = create_query_fixture_root("cli-query-session-bundle")?;

    let output = run_darc([
        "query",
        "session-bundle",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        PRIMARY_SESSION_ID,
    ])?;

    assert!(output.status.success());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.session_bundle.v1");
    assert_eq!(value["data"]["project_id"], "repo-abc123");
    assert_eq!(value["data"]["provider"], "codex");
    assert_eq!(value["data"]["session_id"], PRIMARY_SESSION_ID);
    assert_eq!(value["data"]["session_view"], "compact");
    assert_eq!(value["data"]["view"], "narrative");
    assert_eq!(value["data"]["turn_limit"], 50);
    assert_eq!(value["data"]["turn_offset"], 0);
    assert_eq!(value["data"]["turns_has_more"], false);
    assert_eq!(value["data"]["step_limit"], 50);
    assert_eq!(value["data"]["step_offset"], 0);
    assert_eq!(value["data"]["session_file_limit"], 100);
    assert_eq!(value["data"]["session_files_has_more"], false);
    assert_eq!(value["data"]["session"]["session_id"], PRIMARY_SESSION_ID);
    assert!(
        !value["data"]["session"]["first_user_prompt_truncated"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(value["data"]["turns"][0]["turn_ordinal"], 0);
    assert_eq!(value["data"]["turns"][0]["steps"][0]["type"], "tool_call");
    assert_eq!(value["data"]["turns"][0]["steps"][0]["arguments"], "");
    assert_eq!(
        value["data"]["session_files"]["files"][0]["path"],
        "README.md"
    );
    assert_eq!(
        value["data"]["session_files"]["files"][0]["read_count"],
        serde_json::json!(1)
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn session_scoped_query_requires_provider_for_cross_provider_session_id() -> Result<()> {
    let root = create_query_fixture_root("cli-query-session-provider-ambiguity")?;
    insert_query_fixture_provider_session(
        &root,
        SourceKind::Claude,
        PRIMARY_SESSION_ID,
        "2026-04-06T10:05:00Z",
    )?;

    let ambiguous_output = run_darc([
        "query",
        "session-files",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        PRIMARY_SESSION_ID,
    ])?;
    assert!(!ambiguous_output.status.success());
    let ambiguous_value = parse_json(&ambiguous_output.stderr, "stderr")?;
    assert_eq!(ambiguous_value["schema"], "darc.error.v1");
    assert_eq!(ambiguous_value["error"]["code"], "ambiguous_session");
    assert_eq!(
        ambiguous_value["error"]["details"]["query"],
        PRIMARY_SESSION_ID
    );

    let provider_output = run_darc([
        "query",
        "session-files",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        PRIMARY_SESSION_ID,
    ])?;
    assert!(provider_output.status.success());
    let provider_value = parse_json(&provider_output.stdout, "stdout")?;
    assert_eq!(provider_value["data"]["provider"], "codex");

    remove_root(&root)?;
    Ok(())
}

#[test]
fn session_files_query_rejects_invalid_session_id() -> Result<()> {
    let root = create_query_fixture_root("cli-query-session-files-invalid-id")?;
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
        "abc",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert_eq!(value["error"]["code"], "invalid_session_id");
    assert_eq!(value["error"]["details"]["session"], "abc");

    remove_root(&root)?;
    Ok(())
}

#[test]
fn session_files_query_rejects_prefix_session_id_with_resolver_hint() -> Result<()> {
    let root = create_query_fixture_root("cli-query-session-files-prefix-id")?;
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
        PRIMARY_SESSION_PREFIX,
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert_eq!(value["error"]["code"], "unknown_session");
    assert_eq!(value["error"]["details"]["session"], PRIMARY_SESSION_PREFIX);
    assert_eq!(value["error"]["details"]["looks_like_prefix"], true);
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("resolve-session"))
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn session_files_query_rejects_unknown_uuid_session_id() -> Result<()> {
    let root = create_query_fixture_root("cli-query-session-files-unknown-id")?;
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
        UNKNOWN_SESSION_ID,
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert_eq!(value["error"]["code"], "unknown_session");
    assert_eq!(value["error"]["details"]["session"], UNKNOWN_SESSION_ID);
    assert_eq!(value["error"]["details"]["looks_like_prefix"], false);

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
                PRIMARY_SESSION_ID,
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
            final_answer_text: Some("Follow-up patch done."),
            has_final_answer: true,
            ..IndexedTurnFixture::new(
                "repo-abc123",
                SourceKind::Codex,
                PRIMARY_SESSION_ID,
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
        "--view",
        "full",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.sessions.v1");
    assert_eq!(value["data"]["view"], "full");
    assert_eq!(
        value["data"]["sessions"][0]["first_turn_at"],
        "2026-04-06T10:00:00Z"
    );
    assert_eq!(
        value["data"]["sessions"][0]["first_user_prompt"],
        "Inspect the repository status"
    );
    assert_eq!(
        value["data"]["sessions"][0]["final_agent_message"],
        "Follow-up patch done."
    );
    assert_eq!(
        value["data"]["sessions"][0]["final_agent_message_truncated"],
        false
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
    let early_session_id = "11111111-1111-4111-8111-111111111114";
    insert_query_fixture_session(&root, early_session_id, "2026-04-05T10:00:00Z")?;
    insert_query_fixture_session(&root, SECONDARY_SESSION_ID, "2026-04-07T10:00:00Z")?;

    let since_output = run_darc([
        "query",
        "sessions",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--since",
        "2026-04-06T00:00:00Z",
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
        vec![SECONDARY_SESSION_ID, PRIMARY_SESSION_ID]
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
        vec![PRIMARY_SESSION_ID, early_session_id]
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
        vec![PRIMARY_SESSION_ID]
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
        PRIMARY_SESSION_ID,
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.turns.v1");
    assert_eq!(value["data"]["since"], Value::Null);
    assert_eq!(value["data"]["until"], Value::Null);
    assert_eq!(value["data"]["view"], "full");
    assert_eq!(value["data"]["limit"], 50);
    assert_eq!(value["data"]["offset"], 0);
    assert_eq!(value["data"]["has_more"], false);
    assert_eq!(value["data"]["turns"][0]["turn_id"], "turn-1");
    assert_eq!(value["data"]["turns"][0]["step_count"], 2);
    assert_eq!(value["data"]["turns"][0]["tool_call_count"], 1);
    assert_eq!(value["data"]["turns"][0]["agent_answer_preview"], "Done.");
    assert!(
        !value["data"]["turns"][0]
            .as_object()
            .unwrap()
            .contains_key("agent_answer_preview_truncated")
    );
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
fn turns_query_rejects_invalid_absolute_time_bounds() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turns-invalid-bound")?;
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
        PRIMARY_SESSION_ID,
        "--since",
        "2026-99-99T00:00:00Z",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("UTC ISO-8601"))
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turns_query_applies_since_and_until_filters_in_session_mode() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turns-bounds")?;
    let connection = open_index_database(&root.join("index.sqlite"))?;
    for (turn_ordinal, started_at) in [
        (1, "2026-04-06T09:59:00Z"),
        (2, "2026-04-06T10:01:00Z"),
        (3, "2026-04-06T10:03:00Z"),
    ] {
        let turn_id = format!("turn-{turn_ordinal}");
        let completed_at = started_at.replace(":00Z", ":05Z");
        let user_message = format!("Turn {turn_ordinal}");
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture {
                turn_id: Some(turn_id.as_str()),
                completed_at: Some(completed_at.as_str()),
                user_message: user_message.as_str(),
                step_count: 1,
                tool_call_count: 1,
                tool_output_count: 1,
                duration_ms: 5_000,
                has_final_answer: true,
                ..IndexedTurnFixture::new(
                    "repo-abc123",
                    SourceKind::Codex,
                    PRIMARY_SESSION_ID,
                    turn_ordinal,
                    started_at,
                    "completed",
                    "[]",
                )
            },
        )?;
    }
    drop(connection);

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
        PRIMARY_SESSION_ID,
        "--since",
        "2026-04-06T10:00:00Z",
        "--until",
        "2026-04-06T10:03:00Z",
    ])?;

    assert!(output.status.success());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["data"]["since"], "2026-04-06T10:00:00Z");
    assert_eq!(value["data"]["until"], "2026-04-06T10:03:00Z");
    assert_eq!(value["data"]["limit"], 50);
    assert_eq!(value["data"]["offset"], 0);
    assert_eq!(value["data"]["has_more"], false);
    assert_eq!(
        value["data"]["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| turn["turn_ordinal"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![0, 2]
    );

    let page_output = run_darc([
        "query",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--session-id",
        PRIMARY_SESSION_ID,
        "--limit",
        "2",
        "--offset",
        "1",
    ])?;

    assert!(page_output.status.success());
    let page_value = parse_json(&page_output.stdout, "stdout")?;
    assert_eq!(page_value["data"]["limit"], 2);
    assert_eq!(page_value["data"]["offset"], 1);
    assert_eq!(page_value["data"]["has_more"], true);
    assert_eq!(
        page_value["data"]["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| turn["turn_ordinal"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turns_query_oneline_view_emits_compact_rows() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turns-oneline")?;
    let full_output = run_darc([
        "query",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--session-id",
        PRIMARY_SESSION_ID,
    ])?;
    let oneline_output = run_darc([
        "query",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--session-id",
        PRIMARY_SESSION_ID,
        "--view",
        "oneline",
    ])?;

    assert!(full_output.status.success());
    assert!(oneline_output.status.success());
    let value = parse_json(&oneline_output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.turns.v1");
    assert_eq!(value["data"]["view"], "oneline");
    assert_eq!(value["data"]["limit"], 50);
    assert_eq!(value["data"]["offset"], 0);
    assert_eq!(value["data"]["has_more"], false);
    assert_eq!(value["data"]["turns"][0]["turn_ordinal"], 0);
    assert_eq!(value["data"]["turns"][0]["role"], "user");
    assert_eq!(
        value["data"]["turns"][0]["user_prompt_preview"],
        "Inspect the repository status"
    );
    assert!(
        !value["data"]["turns"][0]
            .as_object()
            .unwrap()
            .contains_key("user_prompt_preview_truncated")
    );
    assert_eq!(value["data"]["turns"][0]["user_prompt_preview_chars"], 29);
    assert_eq!(value["data"]["turns"][0]["user_prompt_total_chars"], 29);
    assert_eq!(value["data"]["turns"][0]["agent_answer_preview"], "Done.");
    assert!(
        !value["data"]["turns"][0]
            .as_object()
            .unwrap()
            .contains_key("agent_answer_preview_truncated")
    );
    assert_eq!(value["data"]["turns"][0]["agent_answer_preview_chars"], 5);
    assert_eq!(value["data"]["turns"][0]["agent_answer_total_chars"], 5);
    assert_eq!(value["data"]["turns"][0]["step_count"], 2);
    assert_eq!(value["data"]["turns"][0]["tool_call_count"], 1);
    assert!(
        value["data"]["turns"][0]
            .as_object()
            .unwrap()
            .get("turn_id")
            .is_none()
    );
    assert!(oneline_output.stdout.len() < full_output.stdout.len());

    remove_root(&root)?;
    Ok(())
}

#[test]
fn search_turns_query_emits_literal_evidence_matches() -> Result<()> {
    let root = create_query_fixture_root("cli-query-search-literal")?;
    let connection = open_index_database(&root.join("index.sqlite"))?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            turn_id: Some("turn-2"),
            completed_at: Some("2026-04-06T10:05:05Z"),
            user_message: "Run the CLI command with an exact flag",
            final_answer_at: Some("2026-04-06T10:05:05Z"),
            final_answer_text: Some("Captured the output."),
            step_count: 2,
            tool_call_count: 1,
            tool_output_count: 1,
            has_final_answer: true,
            duration_ms: 5_000,
            ..IndexedTurnFixture::new(
                "repo-abc123",
                SourceKind::Codex,
                PRIMARY_SESSION_ID,
                1,
                "2026-04-06T10:05:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:05:01Z","call_id":"call-2","name":"exec_command","arguments":"{\"cmd\":\"darc query search turns --mode literal --query --output-last-message\",\"workdir\":\"/tmp/repo\"}"},{"type":"tool_call_output","timestamp":"2026-04-06T10:05:02Z","call_id":"call-2","output":"DARC_CODEX_BIN=/tmp/darc"}]"##,
            )
        },
    )?;
    drop(connection);

    let output = run_darc([
        "query",
        "search",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--mode",
        "literal",
        "--query",
        "--output-last-message",
        "--match-limit",
        "1",
        "--since",
        "2026-04-06T00:00:00Z",
        "--until",
        "2026-04-07T00:00:00Z",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.search.turns.v1");
    assert_eq!(value["data"]["mode"], "literal");
    assert_eq!(value["data"]["include_tool_output"], false);
    assert_eq!(value["data"]["fields"], Value::Array(vec![]));
    assert_eq!(value["data"]["excluded_fields"], Value::Array(vec![]));
    assert_eq!(value["data"]["match_limit"], 1);
    assert_eq!(value["data"]["since"], "2026-04-06T00:00:00Z");
    assert_eq!(value["data"]["until"], "2026-04-07T00:00:00Z");
    assert_eq!(value["data"]["hits"][0]["turn_ordinal"], 1);
    assert_eq!(
        value["data"]["hits"][0]["agent_answer_preview"],
        "Captured the output."
    );
    assert!(
        value["data"]["hits"][0]["matches"][0]["evidence_ordinal"]
            .as_u64()
            .is_some_and(|ordinal| ordinal > 0)
    );
    assert_eq!(
        value["data"]["hits"][0]["matches"][0]["field"],
        "tool_arguments"
    );
    assert_eq!(value["data"]["hits"][0]["matches_count"], 1);
    assert!(
        value["data"]["hits"][0]["matches"][0]["snippet"]
            .as_str()
            .is_some_and(|snippet| snippet.contains("--output-last-message"))
    );

    let literal_colored = run_darc([
        "query",
        "--color",
        "always",
        "search",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--mode",
        "literal",
        "--query",
        "--output-last-message",
        "--match-limit",
        "1",
        "--since",
        "2026-04-06T00:00:00Z",
        "--until",
        "2026-04-07T00:00:00Z",
    ])?;

    assert!(literal_colored.status.success());
    assert!(contains_ansi(&literal_colored.stdout));
    assert!(contains_highlighted_text(
        &literal_colored.stdout,
        "--output-last-message"
    ));
    let literal_stripped = strip_ansi(&literal_colored.stdout);
    let literal_colored_value = parse_json(&literal_stripped, "stripped stdout")?;
    assert_eq!(
        literal_colored_value["data"]["hits"][0]["matches"][0]["snippet"],
        value["data"]["hits"][0]["matches"][0]["snippet"]
    );

    let literal_content_only = run_darc([
        "query",
        "search",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--mode",
        "literal",
        "--query",
        "Captured the output.",
        "--field",
        "final-answer",
    ])?;

    assert!(literal_content_only.status.success());
    let content_value = parse_json(&literal_content_only.stdout, "stdout")?;
    assert_eq!(
        content_value["data"]["fields"],
        serde_json::json!(["final_answer"])
    );
    assert_eq!(
        content_value["data"]["hits"][0]["matches"][0]["field"],
        "final_answer"
    );

    let regex_perl_space = run_darc([
        "query",
        "search",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--mode",
        "regex",
        "--query",
        r"Run\s+the\s+CLI",
        "--field",
        "user-message",
    ])?;

    assert!(regex_perl_space.status.success());
    let regex_perl_space_value = parse_json(&regex_perl_space.stdout, "stdout")?;
    assert_eq!(regex_perl_space_value["data"]["mode"], "regex");
    assert_eq!(
        regex_perl_space_value["data"]["hits"][0]["matches"][0]["field"],
        "user_message"
    );

    let regex_colored = run_darc([
        "query",
        "--color",
        "always",
        "search",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--mode",
        "regex",
        "--query",
        r"Run\s+the\s+CLI",
        "--field",
        "user-message",
    ])?;

    assert!(regex_colored.status.success());
    assert!(contains_ansi(&regex_colored.stdout));
    assert!(contains_highlighted_text(
        &regex_colored.stdout,
        "Run the CLI"
    ));
    let regex_stripped = strip_ansi(&regex_colored.stdout);
    let regex_colored_value = parse_json(&regex_stripped, "stripped stdout")?;
    assert_eq!(
        regex_colored_value["data"]["hits"][0]["matches"][0]["snippet"],
        regex_perl_space_value["data"]["hits"][0]["matches"][0]["snippet"]
    );

    let literal_without_tool_args = run_darc([
        "query",
        "search",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--mode",
        "literal",
        "--query",
        "--output-last-message",
        "--exclude-field",
        "tool-arguments",
    ])?;

    assert!(literal_without_tool_args.status.success());
    let excluded_value = parse_json(&literal_without_tool_args.stdout, "stdout")?;
    assert_eq!(
        excluded_value["data"]["excluded_fields"],
        serde_json::json!(["tool_arguments"])
    );
    assert_eq!(excluded_value["data"]["hits"], Value::Array(vec![]));

    let literal_hidden_output = run_darc([
        "query",
        "search",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--mode",
        "literal",
        "--query",
        "DARC_CODEX_BIN=/tmp/darc",
    ])?;

    assert!(literal_hidden_output.status.success());
    let hidden_value = parse_json(&literal_hidden_output.stdout, "stdout")?;
    assert_eq!(hidden_value["data"]["include_tool_output"], false);
    assert_eq!(hidden_value["data"]["hits"], Value::Array(vec![]));

    let literal_output = run_darc([
        "query",
        "search",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--mode",
        "literal",
        "--query",
        "DARC_CODEX_BIN=/tmp/darc",
        "--include-tool-output",
    ])?;

    assert!(literal_output.status.success());
    let literal_output_value = parse_json(&literal_output.stdout, "stdout")?;
    assert_eq!(literal_output_value["data"]["include_tool_output"], true);
    assert_eq!(
        literal_output_value["data"]["hits"][0]["matches"][0]["field"],
        "tool_output"
    );

    let regex_output = run_darc([
        "query",
        "search",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--mode",
        "regex",
        "--query",
        "DARC_[A-Z_]+_BIN",
        "--include-tool-output",
    ])?;

    assert!(regex_output.status.success());
    let regex_value = parse_json(&regex_output.stdout, "stdout")?;
    assert_eq!(regex_value["data"]["mode"], "regex");
    assert_eq!(regex_value["data"]["include_tool_output"], true);
    assert_eq!(
        regex_value["data"]["hits"][0]["matches"][0]["field"],
        "tool_output"
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turns_query_rejects_removed_grep_flag() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turns-grep-removed")?;
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
    ])?;

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert_eq!(value["error"]["code"], "invalid_arguments");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("unexpected argument '--grep'"))
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turns_query_help_lists_positional_session_and_optional_provider() -> Result<()> {
    let output = run_darc(["query", "turns", "--help"])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--session-id <SESSION_ID>"));
    assert!(stdout.contains("--provider <PROVIDER>      Disambiguate a cross-provider session id"));
    assert!(stdout.contains("required unless --session-id is set"));
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
        PRIMARY_SESSION_ID,
        "0",
        "--include-raw",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.turn.v1");
    assert_eq!(value["data"]["session_id"], PRIMARY_SESSION_ID);
    assert_eq!(value["data"]["step_limit"], 50);
    assert_eq!(value["data"]["step_offset"], 0);
    assert_eq!(value["data"]["steps_has_more"], false);
    assert_eq!(value["data"]["steps"][0]["type"], "tool_call");
    assert_eq!(
        value["data"]["steps"][0]["arguments"],
        "{\"file_path\":\"README.md\"}"
    );
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
        PRIMARY_SESSION_ID,
        "0",
        "--include-insights",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.turn.v1");
    assert_eq!(value["data"]["step_count"], 2);
    assert_eq!(value["data"]["step_limit"], 50);
    assert_eq!(value["data"]["step_offset"], 0);
    assert_eq!(value["data"]["steps_has_more"], false);
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
fn turn_query_embedded_insights_normalize_absolute_project_paths() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turn-insights-paths")?;
    insert_absolute_project_file_read_turn(&root, 1)?;
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
        PRIMARY_SESSION_ID,
        "1",
        "--include-insights",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.turn.v1");
    assert_eq!(value["data"]["insights"]["files"][0]["path"], "src/lib.rs");

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
        PRIMARY_SESSION_ID,
        "--turn-ordinal",
        "0",
        "--view",
        "narrative",
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
        "Inspect",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.search.turns.v1");
    assert_eq!(value["data"]["mode"], "keyword");
    assert!(value["data"]["match_limit"].is_null());
    assert_eq!(value["data"]["hits"][0]["session_id"], PRIMARY_SESSION_ID);
    assert_eq!(
        value["data"]["hits"][0]["user_prompt_preview"],
        "Inspect the repository status"
    );
    assert!(
        !value["data"]["hits"][0]
            .as_object()
            .unwrap()
            .contains_key("user_prompt_preview_truncated")
    );
    assert_eq!(value["data"]["hits"][0]["agent_answer_preview"], "Done.");
    assert!(
        !value["data"]["hits"][0]
            .as_object()
            .unwrap()
            .contains_key("agent_answer_preview_truncated")
    );
    assert_eq!(
        value["data"]["hits"][0]["matched_paths"],
        Value::Array(vec![])
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn search_turns_query_allows_cross_provider_session_id_filter() -> Result<()> {
    let root = create_query_fixture_root("cli-query-search-cross-provider-session")?;
    insert_query_fixture_provider_session(
        &root,
        SourceKind::Claude,
        PRIMARY_SESSION_ID,
        "2026-04-06T10:01:00Z",
    )?;

    let output = run_darc([
        "query",
        "search",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "Inspect",
        "--session-id",
        PRIMARY_SESSION_ID,
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.search.turns.v1");
    assert_eq!(value["data"]["provider"], Value::Null);
    assert_eq!(value["data"]["session_id"], PRIMARY_SESSION_ID);
    let mut providers = value["data"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["provider"].as_str().unwrap())
        .collect::<Vec<_>>();
    providers.sort_unstable();
    assert_eq!(providers, vec!["claude", "codex"]);

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
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.search.turns.v1");
    assert_eq!(value["data"]["mode"], "file_name");
    assert_eq!(value["data"]["matched_path_limit"], 20);
    assert_eq!(value["data"]["hits"][0]["matched_paths"][0], "README.md");
    assert_eq!(value["data"]["hits"][0]["matched_paths_truncated"], false);

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
    ])?;

    assert!(path_output.status.success());
    let path_value = parse_json(&path_output.stdout, "stdout")?;
    assert_eq!(path_value["schema"], "darc.query.search.turns.v1");
    assert_eq!(path_value["data"]["mode"], "file_path");
    assert_eq!(
        path_value["data"]["hits"][0]["matched_paths"][0],
        "README.md"
    );

    let fragment_output = run_darc([
        "query",
        "search",
        "turns",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--mode",
        "path-fragment",
        "--query",
        "README",
    ])?;

    assert!(fragment_output.status.success());
    let fragment_value = parse_json(&fragment_output.stdout, "stdout")?;
    assert_eq!(fragment_value["data"]["mode"], "path_fragment");
    assert_eq!(
        fragment_value["data"]["hits"][0]["matched_paths"][0],
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
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.insights.workspace.v1");
    assert_eq!(value["data"]["recent_session_limit"], 50);
    assert_eq!(value["data"]["recent_session_offset"], 0);
    assert_eq!(value["data"]["recent_sessions_has_more"], false);
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
        "--provider",
        "codex",
        "--turn-limit",
        "1000",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.insights.project.v1");
    assert_eq!(value["data"]["provider"], "codex");
    assert_eq!(value["data"]["turn_limit"], 1000);
    assert_eq!(value["data"]["inspected_turn_count"], 1);
    assert_eq!(value["data"]["turns_has_more"], false);
    assert_eq!(value["data"]["most_common_tools"][0]["name"], "Read");
    assert_eq!(value["data"]["total_time_ms"], 5000);
    assert_eq!(value["data"]["most_read_files"][0]["path"], "README.md");
    assert!(value["data"]["most_read_files"][0]["repo_relative_path"].is_null());

    remove_root(&root)?;
    Ok(())
}

#[test]
fn project_insights_query_normalizes_absolute_project_paths() -> Result<()> {
    let root = create_query_fixture_root("cli-query-project-insights-paths")?;
    insert_absolute_project_file_read_turn(&root, 1)?;
    let output = run_darc([
        "query",
        "insights",
        "project",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--turn-limit",
        "1000",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.insights.project.v1");
    let read_files = value["data"]["most_read_files"]
        .as_array()
        .expect("most_read_files should be an array");
    assert!(
        read_files
            .iter()
            .any(|file| file["path"].as_str() == Some("src/lib.rs"))
    );
    let project_root = root.join("repo").to_string_lossy().into_owned();
    assert!(read_files.iter().all(|file| {
        file["path"]
            .as_str()
            .is_none_or(|path| !path.starts_with(&project_root))
    }));

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
        PRIMARY_SESSION_ID,
        "0",
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
    assert!(value["data"]["files"][0]["repo_relative_path"].is_null());
    assert_eq!(value["data"]["files"][0]["read_count"], 1);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turn_insights_query_normalizes_absolute_project_paths() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turn-insights-paths")?;
    insert_absolute_project_file_read_turn(&root, 1)?;
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
        PRIMARY_SESSION_ID,
        "1",
    ])?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.insights.turn.v1");
    assert_eq!(value["data"]["files"][0]["path"], "src/lib.rs");

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
                PRIMARY_SESSION_ID,
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
        PRIMARY_SESSION_ID,
        "--turn-ordinal",
        "1",
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
        PRIMARY_SESSION_ID,
        "--turn-ordinal",
        "9",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains(&format!(
                "turn 9 was not found in session {} for provider codex",
                PRIMARY_SESSION_ID
            ))
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turns_query_rejects_prefix_session_id() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turns-prefix-id")?;
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
        PRIMARY_SESSION_PREFIX,
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert_eq!(value["error"]["code"], "unknown_session");
    assert_eq!(value["error"]["details"]["session"], PRIMARY_SESSION_PREFIX);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn search_turns_query_rejects_unknown_session_id_filter() -> Result<()> {
    let root = create_query_fixture_root("cli-query-search-unknown-session-id")?;
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
        "--session-id",
        UNKNOWN_SESSION_ID,
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert_eq!(value["error"]["code"], "unknown_session");
    assert_eq!(value["error"]["details"]["session"], UNKNOWN_SESSION_ID);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn search_turns_query_rejects_tool_output_flag_for_keyword_mode() -> Result<()> {
    let root = create_query_fixture_root("cli-query-search-tool-output-keyword")?;
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
        "--include-tool-output",
    ])?;

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "--include-tool-output is only supported with --mode literal or --mode regex"
        )
    );

    let match_limit_output = run_darc([
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
        "--match-limit",
        "3",
    ])?;

    assert!(!match_limit_output.status.success());
    assert!(
        String::from_utf8_lossy(&match_limit_output.stderr)
            .contains("--match-limit is only supported with --mode literal or --mode regex")
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turn_query_rejects_explicit_narrative_raw_conflict() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turn-narrative-raw-conflict")?;
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
        PRIMARY_SESSION_ID,
        "--turn-ordinal",
        "0",
        "--view",
        "narrative",
        "--include-raw",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert!(
        value["error"]["message"]
            .as_str()
            .expect("error message should be a string")
            .contains("--include-raw requires --view full")
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turn_query_rejects_invalid_session_id() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turn-invalid-session-id")?;
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
        "abc",
        "--turn-ordinal",
        "0",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert_eq!(value["error"]["code"], "invalid_session_id");
    assert_eq!(value["error"]["details"]["session"], "abc");

    remove_root(&root)?;
    Ok(())
}

#[test]
fn turn_insights_query_rejects_prefix_session_id() -> Result<()> {
    let root = create_query_fixture_root("cli-query-turn-insights-prefix-id")?;
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
        PRIMARY_SESSION_PREFIX,
        "--turn-ordinal",
        "0",
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert_eq!(value["error"]["code"], "unknown_session");
    assert_eq!(value["error"]["details"]["session"], PRIMARY_SESSION_PREFIX);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn resolve_session_query_emits_single_match_success() -> Result<()> {
    let root = create_query_fixture_root("cli-query-resolve-session-single")?;
    let output = run_darc([
        "query",
        "resolve-session",
        PRIMARY_SESSION_PREFIX,
        "--root",
        root.to_string_lossy().as_ref(),
    ])?;

    assert!(output.status.success());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.resolve_session.v1");
    assert_eq!(value["data"]["query"], PRIMARY_SESSION_PREFIX);
    assert_eq!(value["data"]["matches"][0]["project_id"], "repo-abc123");
    assert_eq!(value["data"]["matches"][0]["provider"], "codex");
    assert_eq!(
        value["data"]["matches"][0]["session_id"],
        PRIMARY_SESSION_ID
    );
    assert_eq!(value["data"]["total"], 1);
    assert_eq!(value["data"]["truncated"], false);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn resolve_session_query_lists_matches_and_reports_ambiguity() -> Result<()> {
    let root = create_query_fixture_root("cli-query-resolve-session-multi")?;
    insert_query_fixture_session(&root, SECONDARY_SESSION_ID, "2026-04-07T10:00:00Z")?;
    insert_query_fixture_provider_session(
        &root,
        SourceKind::Claude,
        TERTIARY_SESSION_ID,
        "2026-04-07T11:00:00Z",
    )?;

    let output = run_darc([
        "query",
        "resolve-session",
        PRIMARY_SESSION_PREFIX,
        "--root",
        root.to_string_lossy().as_ref(),
    ])?;
    assert!(output.status.success());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(
        value["data"]["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["project_id"].as_str().unwrap(),
                    row["provider"].as_str().unwrap(),
                    row["session_id"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("repo-abc123", "claude", TERTIARY_SESSION_ID),
            ("repo-abc123", "codex", PRIMARY_SESSION_ID),
            ("repo-abc123", "codex", SECONDARY_SESSION_ID),
        ]
    );

    let provider_output = run_darc([
        "query",
        "resolve-session",
        PRIMARY_SESSION_PREFIX,
        "--root",
        root.to_string_lossy().as_ref(),
        "--provider",
        "codex",
    ])?;
    assert!(provider_output.status.success());
    let provider_value = parse_json(&provider_output.stdout, "stdout")?;
    assert_eq!(
        provider_value["data"]["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["project_id"].as_str().unwrap(),
                    row["session_id"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("repo-abc123", PRIMARY_SESSION_ID),
            ("repo-abc123", SECONDARY_SESSION_ID)
        ]
    );

    let ambiguous_output = run_darc([
        "query",
        "resolve-session",
        PRIMARY_SESSION_PREFIX,
        "--root",
        root.to_string_lossy().as_ref(),
        "--pick-one",
    ])?;
    assert!(!ambiguous_output.status.success());
    let ambiguous_value = parse_json(&ambiguous_output.stderr, "stderr")?;
    assert_eq!(ambiguous_value["schema"], "darc.error.v1");
    assert_eq!(ambiguous_value["error"]["code"], "ambiguous_session");
    assert_eq!(
        ambiguous_value["error"]["details"]["query"],
        PRIMARY_SESSION_PREFIX
    );
    assert_eq!(
        ambiguous_value["error"]["details"]["matches"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    remove_root(&root)?;
    Ok(())
}

#[test]
fn resolve_session_query_reports_unknown_full_uuid() -> Result<()> {
    let root = create_query_fixture_root("cli-query-resolve-session-unknown")?;
    let output = run_darc([
        "query",
        "resolve-session",
        UNKNOWN_SESSION_ID,
        "--root",
        root.to_string_lossy().as_ref(),
    ])?;

    assert!(!output.status.success());
    let value = parse_json(&output.stderr, "stderr")?;
    assert_eq!(value["schema"], "darc.error.v1");
    assert_eq!(value["error"]["code"], "unknown_session");
    assert_eq!(value["error"]["details"]["query"], UNKNOWN_SESSION_ID);
    assert_eq!(value["error"]["details"]["looks_like_prefix"], false);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn resolve_session_query_reports_truncation() -> Result<()> {
    let root = create_query_fixture_root("cli-query-resolve-session-truncated")?;
    for suffix in 0_u16..60 {
        let session_id = format!("11111111-1111-4111-8111-{suffix:012x}");
        insert_query_fixture_session(&root, &session_id, "2026-04-07T10:00:00Z")?;
    }

    let output = run_darc([
        "query",
        "resolve-session",
        PRIMARY_SESSION_PREFIX,
        "--root",
        root.to_string_lossy().as_ref(),
    ])?;

    assert!(output.status.success());
    let value = parse_json(&output.stdout, "stdout")?;
    assert_eq!(value["schema"], "darc.query.resolve_session.v1");
    assert_eq!(value["data"]["total"], 61);
    assert_eq!(value["data"]["truncated"], true);
    assert_eq!(value["data"]["matches"].as_array().unwrap().len(), 50);

    remove_root(&root)?;
    Ok(())
}

#[test]
fn resolve_session_pick_one_feeds_session_bundle() -> Result<()> {
    let root = create_query_fixture_root("cli-query-resolve-session-e2e")?;
    let resolved = run_darc([
        "query",
        "resolve-session",
        PRIMARY_SESSION_PREFIX,
        "--root",
        root.to_string_lossy().as_ref(),
        "--pick-one",
    ])?;
    assert!(resolved.status.success());
    let resolved_value = parse_json(&resolved.stdout, "stdout")?;
    let resolved_session_id = resolved_value["data"]["match"]["session_id"]
        .as_str()
        .context("missing resolved session id")?
        .to_owned();
    assert_eq!(resolved_session_id, PRIMARY_SESSION_ID);

    let bundle = run_darc([
        "query",
        "session-bundle",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--session-id",
        &resolved_session_id,
    ])?;
    assert!(bundle.status.success());

    let prefix_bundle = run_darc([
        "query",
        "session-bundle",
        "--root",
        root.to_string_lossy().as_ref(),
        "--project-id",
        "repo-abc123",
        "--provider",
        "codex",
        "--session-id",
        PRIMARY_SESSION_PREFIX,
    ])?;
    assert!(!prefix_bundle.status.success());
    let prefix_value = parse_json(&prefix_bundle.stderr, "stderr")?;
    assert_eq!(prefix_value["error"]["code"], "unknown_session");

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
