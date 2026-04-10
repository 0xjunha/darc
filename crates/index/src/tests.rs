#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use darc_paths::SourceKind;
use darc_rollout::{
    ParseDeterminism,
    codex::CodexRollout,
    model::{
        NormalizedTurn as CodexTurn, NormalizedTurnMessage as CodexTurnMessage,
        NormalizedTurnStatus as CodexTurnStatus, NormalizedTurnStep as CodexTurnStep,
    },
};
use rusqlite::Connection;
use serde_json::Value;

use crate::{
    INDEX_DB_FILE_NAME,
    engine::{
        TEST_PROJECT_ID, file_snapshot, index_project_codex_turns_from,
        index_project_sessions_from, parse_codex_rollout,
    },
};

static UNIQUE_TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn parses_two_turns_with_event_boundaries() -> Result<()> {
    let rollout = parse_fixture(
        r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-two-turns","cwd":"/tmp/repo","cli_version":"0.118.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}
{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"duplicate"}]}}
{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"user_message","message":"First task"}}
{"timestamp":"2026-01-01T00:00:04Z","type":"event_msg","payload":{"type":"agent_message","phase":"commentary","message":"duplicate commentary"}}
{"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Checking files."}]}}
{"timestamp":"2026-01-01T00:00:06Z","type":"response_item","payload":{"type":"reasoning","summary":["scan"],"encrypted_content":"secret"}}
{"timestamp":"2026-01-01T00:00:07Z","type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"exec_command","arguments":"{\"cmd\":\"ls\"}"}}
{"timestamp":"2026-01-01T00:00:08Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"ok"}}
{"timestamp":"2026-01-01T00:00:09Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"First reply"}]}}
{"timestamp":"2026-01-01T00:00:10Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}
{"timestamp":"2026-01-01T00:00:11Z","type":"event_msg","payload":{"type":"user_message","message":"Second task"}}
{"timestamp":"2026-01-01T00:00:12Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Second reply"}]}}
"#,
    )?;

    assert_eq!(
        rollout,
        CodexRollout {
            session_id: "fixture-two-turns".to_owned(),
            cwd: Path::new("/tmp/repo").to_path_buf(),
            cli_version: "0.118.0".to_owned(),
            schema_id: "codex.turn_lifecycle".to_owned(),
            determinism: ParseDeterminism::Exact,
            turns: vec![
                CodexTurn {
                    turn_id: Some("turn-1".to_owned()),
                    user_message: "First task".to_owned(),
                    final_answer: Some(CodexTurnMessage {
                        timestamp: "2026-01-01T00:00:09Z".to_owned(),
                        text: "First reply".to_owned(),
                    }),
                    started_at: "2026-01-01T00:00:03Z".to_owned(),
                    completed_at: Some("2026-01-01T00:00:09Z".to_owned()),
                    status: CodexTurnStatus::Completed,
                    primary_model: None,
                    total_token_count: None,
                    steps: vec![
                        CodexTurnStep::Commentary {
                            timestamp: "2026-01-01T00:00:05Z".to_owned(),
                            text: "Checking files.".to_owned(),
                        },
                        CodexTurnStep::Reasoning {
                            timestamp: "2026-01-01T00:00:06Z".to_owned(),
                            summary: vec!["scan".to_owned()],
                            encrypted: true,
                        },
                        CodexTurnStep::ToolCall {
                            timestamp: "2026-01-01T00:00:07Z".to_owned(),
                            call_id: "call-1".to_owned(),
                            name: "exec_command".to_owned(),
                            arguments: "{\"cmd\":\"ls\"}".to_owned(),
                        },
                        CodexTurnStep::ToolCallOutput {
                            timestamp: "2026-01-01T00:00:08Z".to_owned(),
                            call_id: "call-1".to_owned(),
                            output: "ok".to_owned(),
                        },
                    ],
                },
                CodexTurn {
                    turn_id: Some("turn-2".to_owned()),
                    user_message: "Second task".to_owned(),
                    final_answer: Some(CodexTurnMessage {
                        timestamp: "2026-01-01T00:00:12Z".to_owned(),
                        text: "Second reply".to_owned(),
                    }),
                    started_at: "2026-01-01T00:00:11Z".to_owned(),
                    completed_at: Some("2026-01-01T00:00:12Z".to_owned()),
                    status: CodexTurnStatus::Completed,
                    primary_model: None,
                    total_token_count: None,
                    steps: vec![],
                },
            ],
        }
    );

    Ok(())
}

#[test]
fn falls_back_to_non_boilerplate_response_item_user_messages() -> Result<()> {
    let rollout = parse_fixture(
        r##"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-fallback","cwd":"/tmp/repo","cli_version":"0.118.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /tmp/repo"}]}}
{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>/tmp/repo</cwd>\n</environment_context>"}]}}
{"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Summarize the build output"}]}}
{"timestamp":"2026-01-01T00:00:04Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Build passed."}]}}
"##,
    )?;

    assert_eq!(rollout.turns.len(), 1);
    assert_eq!(rollout.turns[0].user_message, "Summarize the build output");
    assert_eq!(rollout.turns[0].status, CodexTurnStatus::Completed);
    assert_eq!(
        rollout.turns[0].final_answer,
        Some(CodexTurnMessage {
            timestamp: "2026-01-01T00:00:04Z".to_owned(),
            text: "Build passed.".to_owned(),
        })
    );

    Ok(())
}

#[test]
fn uses_task_complete_when_no_final_answer_message_exists() -> Result<()> {
    let rollout = parse_fixture(
        r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-complete","cwd":"/tmp/repo","cli_version":"0.118.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Run the checks"}}
{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Running checks."}]}}
{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Checks passed."}}
"#,
    )?;

    assert_eq!(rollout.turns.len(), 1);
    assert_eq!(rollout.turns[0].status, CodexTurnStatus::Completed);
    assert!(matches!(
        rollout.turns[0].final_answer.as_ref(),
        Some(CodexTurnMessage { text, .. }) if text == "Checks passed."
    ));

    Ok(())
}

#[test]
fn marks_aborted_turns() -> Result<()> {
    let rollout = parse_fixture(
        r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-aborted","cwd":"/tmp/repo","cli_version":"0.118.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Inspect the repo"}}
{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Reading files."}]}}
{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"turn_aborted","turn_id":"turn-1","reason":"interrupted"}}
"#,
    )?;

    assert_eq!(rollout.turns.len(), 1);
    assert_eq!(rollout.turns[0].status, CodexTurnStatus::Aborted);
    assert_eq!(
        rollout.turns[0].completed_at.as_deref(),
        Some("2026-01-01T00:00:03Z")
    );
    assert!(rollout.turns[0].final_answer.is_none());

    Ok(())
}

#[test]
fn treats_legacy_unphased_assistant_messages_as_final_answers() -> Result<()> {
    let rollout = parse_fixture(
        r##"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-legacy-final","cwd":"/tmp/repo","cli_version":"0.118.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /tmp/repo"}]}}
{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"Legacy prompt"}}
{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"agent_message","message":"Legacy final reply"}}
{"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Legacy final reply"}]}}
"##,
    )?;

    assert_eq!(rollout.turns.len(), 1);
    assert_eq!(rollout.turns[0].status, CodexTurnStatus::Completed);
    assert_eq!(
        rollout.turns[0].final_answer,
        Some(CodexTurnMessage {
            timestamp: "2026-01-01T00:00:03Z".to_owned(),
            text: "Legacy final reply".to_owned(),
        })
    );

    Ok(())
}

#[test]
fn parses_structured_tool_payloads_and_custom_tool_items() -> Result<()> {
    let rollout = parse_fixture(
        r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-structured-tools","cwd":"/tmp/repo","cli_version":"0.118.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Inspect the rollout"}}
{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"screenshot","arguments":"{\"pageno\":0,\"mode\":\"page\"}"}}
{"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":[{"type":"input_image","image_url":"data:image/png;base64,abc"}]}}
{"timestamp":"2026-01-01T00:00:04Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"call-2","name":"apply_patch","input":"*** Begin Patch\n*** End Patch\n"}}
{"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-2","output":"{\"output\":\"Success\",\"metadata\":{\"exit_code\":0}}"}}
{"timestamp":"2026-01-01T00:00:06Z","type":"response_item","payload":{"type":"web_search_call","status":"completed","action":{"type":"open_page","url":"https://example.com"}}}
{"timestamp":"2026-01-01T00:00:07Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Parsed."}]}}
"#,
    )?;

    assert_eq!(rollout.turns.len(), 1);
    assert_eq!(rollout.turns[0].steps.len(), 5);

    let CodexTurnStep::ToolCall { arguments, .. } = &rollout.turns[0].steps[0] else {
        panic!("expected structured function_call step");
    };
    let arguments: Value = serde_json::from_str(arguments)?;
    assert_eq!(arguments["mode"], "page");
    assert_eq!(arguments["pageno"], 0);

    let CodexTurnStep::ToolCallOutput { output, .. } = &rollout.turns[0].steps[1] else {
        panic!("expected structured function_call_output step");
    };
    let output: Value = serde_json::from_str(output)?;
    assert_eq!(output[0]["type"], "input_image");

    let CodexTurnStep::ToolCall {
        name, arguments, ..
    } = &rollout.turns[0].steps[2]
    else {
        panic!("expected custom tool call step");
    };
    assert_eq!(name, "apply_patch");
    assert_eq!(arguments, "*** Begin Patch\n*** End Patch");

    let CodexTurnStep::ToolCallOutput { output, .. } = &rollout.turns[0].steps[3] else {
        panic!("expected custom tool output step");
    };
    let output: Value = serde_json::from_str(output)?;
    assert_eq!(output["output"], "Success");
    assert_eq!(output["metadata"]["exit_code"], 0);

    let CodexTurnStep::ProviderResponseItem {
        item_type,
        payload_json,
        ..
    } = &rollout.turns[0].steps[4]
    else {
        panic!("expected preserved provider response item");
    };
    assert_eq!(item_type, "web_search_call");
    let payload: Value = serde_json::from_str(payload_json)?;
    assert_eq!(payload["action"]["url"], "https://example.com");

    Ok(())
}

fn parse_fixture(input: &str) -> Result<CodexRollout> {
    let dir = unique_test_dir("parse-fixture");
    let path = dir.join("rollout.jsonl");
    fs::create_dir_all(&dir)?;
    write_file(&path, input)?;
    let rollout = parse_codex_rollout(&path);
    fs::remove_dir_all(&dir)?;
    rollout
}

/// Builds a unique temporary directory for one parse test fixture.
fn unique_test_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    let counter = UNIQUE_TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!(
        "test-{prefix}-{}-{nanos}-{counter}",
        std::process::id()
    ))
}

/// Writes one text file while creating any missing parent directories.
fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

/// Sets one file's modified timestamp to a fixed value for snapshot-based parse tests.
fn touch_file_timestamp(path: &Path, timestamp: &str) -> Result<()> {
    let status = Command::new("touch")
        .arg("-t")
        .arg(timestamp)
        .arg(path)
        .status()
        .with_context(|| format!("failed to run touch for {}", path.display()))?;
    if !status.success() {
        anyhow::bail!("touch -t {timestamp} failed for {}", path.display());
    }
    Ok(())
}

/// Creates the fixed test sessions root used by the indexing tests.
fn write_parse_config(_root: &Path, _project_root: &Path, sessions_root: &Path) -> Result<String> {
    fs::create_dir_all(sessions_root)?;
    Ok(TEST_PROJECT_ID.to_owned())
}

/// Counts the indexed Codex sessions currently stored for one project.
fn indexed_codex_session_count(connection: &Connection, project_id: &str) -> Result<i64> {
    connection
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE project_id = ?1 AND provider = 'codex'",
            [project_id],
            |row| row.get(0),
        )
        .context("failed to count indexed Codex sessions in normalized table")
}

/// Counts the indexed Codex turns currently stored for one project.
fn indexed_codex_turn_count(connection: &Connection, project_id: &str) -> Result<i64> {
    connection
        .query_row(
            "SELECT COUNT(*) FROM turns WHERE project_id = ?1 AND provider = 'codex'",
            [project_id],
            |row| row.get(0),
        )
        .context("failed to count indexed Codex turns in normalized table")
}

#[test]
fn index_project_indexes_codex_turns_into_sqlite() -> Result<()> {
    let darc_root = unique_test_dir("parse-index");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    let index_db_path = darc_root.join(INDEX_DB_FILE_NAME);
    let rollout_path =
        codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    write_file(
        &rollout_path,
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"First task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"First reply\"}}]}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:04Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-2\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:05Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Second task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:06Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"commentary\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Checking\"}}]}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:07Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Second reply\"}}]}}}}\n"
            ),
            project_root.display()
        ),
    )?;
    let (source_size, source_mtime_ms) = file_snapshot(&rollout_path)?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;

    assert_eq!(report.project_name, "repo");
    assert_eq!(report.project_root, fs::canonicalize(&project_root)?);
    assert_eq!(report.sessions_discovered, 1);
    assert_eq!(report.sessions_currently_indexed, 1);
    assert_eq!(report.sessions_skipped_this_run, 0);
    assert_eq!(report.turns_currently_indexed, 2);
    assert_eq!(report.index_db_path, index_db_path);
    assert!(report.skipped_rollouts.is_empty());

    let connection = Connection::open(&report.index_db_path)?;
    let indexed_sessions = indexed_codex_session_count(&connection, "repo-abc123")?;
    let indexed_turns = indexed_codex_turn_count(&connection, "repo-abc123")?;
    let second_turn: (String, String) = connection.query_row(
        "
        SELECT user_message, final_answer_text
        FROM turns
        WHERE project_id = ?1 AND provider = 'codex' AND session_id = ?2 AND turn_ordinal = 1
        ",
        ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let session_metadata: (String, String, String, i64, i64) = connection.query_row(
        "
        SELECT cli_version, schema_id, determinism, source_size, source_mtime_ms
        FROM sessions
        WHERE project_id = ?1 AND provider = 'codex' AND session_id = ?2
        ",
        ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    assert_eq!(indexed_sessions, 1);
    assert_eq!(indexed_turns, 2);
    assert_eq!(second_turn.0, "Second task");
    assert_eq!(second_turn.1, "Second reply");
    assert_eq!(session_metadata.0, "0.118.0");
    assert_eq!(session_metadata.1, "codex.turn_lifecycle");
    assert_eq!(session_metadata.2, "exact");
    assert_eq!(u64::try_from(session_metadata.3)?, source_size);
    assert_eq!(u64::try_from(session_metadata.4)?, source_mtime_ms);

    Ok(())
}

#[test]
fn index_project_materializes_tool_calls_and_file_accesses() -> Result<()> {
    let darc_root = unique_test_dir("parse-derived-analytics");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    let rollout_path =
        codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    write_file(
        &rollout_path,
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Inspect files\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"Read\",\"arguments\":\"{{\\\"file_path\\\":\\\"README.md\\\"}}\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"function_call_output\",\"call_id\":\"call-1\",\"output\":\"# README\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:04Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"function_call\",\"call_id\":\"call-2\",\"name\":\"Edit\",\"arguments\":\"{{\\\"path\\\":\\\"src/main.rs\\\"}}\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:05Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Done\"}}]}}}}\n"
            ),
            project_root.display()
        ),
    )?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;
    let connection = Connection::open(&report.index_db_path)?;
    let tool_rows = connection
        .prepare(
            "
            SELECT call_ordinal, call_id, tool_name, output_text
            FROM tool_calls
            WHERE project_id = ?1 AND provider = 'codex' AND session_id = ?2
            ORDER BY call_ordinal ASC
            ",
        )?
        .query_map(
            ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let file_rows = connection
        .prepare(
            "
            SELECT tool_name, access_type, path
            FROM file_accesses
            WHERE project_id = ?1 AND provider = 'codex' AND session_id = ?2
            ORDER BY call_ordinal ASC, path ASC
            ",
        )?
        .query_map(
            ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    assert_eq!(
        tool_rows,
        vec![
            (
                0,
                "call-1".to_owned(),
                Some("Read".to_owned()),
                Some("# README".to_owned())
            ),
            (1, "call-2".to_owned(), Some("Edit".to_owned()), None),
        ]
    );
    assert_eq!(
        file_rows,
        vec![
            ("Read".to_owned(), "read".to_owned(), "README.md".to_owned()),
            (
                "Edit".to_owned(),
                "edit".to_owned(),
                "src/main.rs".to_owned()
            ),
        ]
    );

    Ok(())
}

#[test]
fn index_project_materializes_shell_and_patch_file_accesses() -> Result<()> {
    let darc_root = unique_test_dir("parse-shell-analytics");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    let rollout_path =
        codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    write_file(
        &rollout_path,
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Inspect files\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"exec_command\",\"arguments\":\"{{\\\"cmd\\\":\\\"sed -n '1,200p' README.md && cat > notes.txt <<'EOF'\\\\nhello\\\\nEOF\\\",\\\"workdir\\\":\\\"{}\\\"}}\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"custom_tool_call\",\"call_id\":\"call-2\",\"name\":\"apply_patch\",\"input\":\"*** Begin Patch\\n*** Update File: src/main.rs\\n@@\\n-old\\n+new\\n*** Add File: src/new.rs\\n+fn main() {{}}\\n*** End Patch\\n\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:04Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Done\"}}]}}}}\n"
            ),
            project_root.display(),
            project_root.display()
        ),
    )?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;
    let connection = Connection::open(&report.index_db_path)?;
    let file_rows = connection
        .prepare(
            "
            SELECT tool_name, access_type, path
            FROM file_accesses
            WHERE project_id = ?1 AND provider = 'codex' AND session_id = ?2
            ORDER BY call_ordinal ASC, access_type ASC, path ASC
            ",
        )?
        .query_map(
            ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    assert_eq!(
        file_rows,
        vec![
            (
                "exec_command".to_owned(),
                "read".to_owned(),
                "README.md".to_owned()
            ),
            (
                "exec_command".to_owned(),
                "write".to_owned(),
                "notes.txt".to_owned()
            ),
            (
                "apply_patch".to_owned(),
                "edit".to_owned(),
                "src/main.rs".to_owned()
            ),
            (
                "apply_patch".to_owned(),
                "write".to_owned(),
                "src/new.rs".to_owned()
            ),
        ]
    );

    Ok(())
}

#[test]
fn index_project_indexes_codex_and_claude_rollouts_together() -> Result<()> {
    let darc_root = unique_test_dir("parse-multi-provider");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    let claude_root = sessions_root.join("claude");
    let claude_session_id = "885a05b8-f731-4fde-bfdb-a24ce28dc9c3";
    let claude_parent = claude_root
        .join(claude_session_id)
        .join(format!("{claude_session_id}.jsonl"));
    let claude_subagent = claude_root
        .join(claude_session_id)
        .join("subagents/agent-a487e2adbf00a7a09.jsonl");
    let codex_rollout =
        codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    write_file(
        &codex_rollout,
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Codex task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Codex reply\"}}]}}}}\n"
            ),
            project_root.display()
        ),
    )?;
    write_file(
        &claude_parent,
        &format!(
            concat!(
                "{{\"type\":\"queue-operation\",\"operation\":\"enqueue\",\"timestamp\":\"2026-04-01T11:00:00Z\",\"sessionId\":\"{}\",\"content\":\"Inspect parse.rs\"}}\n",
                "{{\"parentUuid\":null,\"isSidechain\":false,\"promptId\":\"prompt-1\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"Inspect parse.rs\"}},\"uuid\":\"user-1\",\"timestamp\":\"2026-04-01T11:00:01Z\",\"userType\":\"external\",\"entrypoint\":\"claude-desktop\",\"cwd\":\"{}\",\"sessionId\":\"{}\",\"version\":\"2.1.87\",\"gitBranch\":\"main\"}}\n",
                "{{\"parentUuid\":\"user-1\",\"isSidechain\":false,\"message\":{{\"model\":\"claude-sonnet-4-6\",\"id\":\"assistant-1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"Claude reply\"}}],\"stop_reason\":\"end_turn\",\"stop_sequence\":null}},\"requestId\":\"req-1\",\"type\":\"assistant\",\"uuid\":\"assistant-1\",\"timestamp\":\"2026-04-01T11:00:02Z\",\"userType\":\"external\",\"entrypoint\":\"claude-desktop\",\"cwd\":\"{}\",\"sessionId\":\"{}\",\"version\":\"2.1.87\",\"gitBranch\":\"main\"}}\n"
            ),
            claude_session_id,
            project_root.display(),
            claude_session_id,
            project_root.display(),
            claude_session_id
        ),
    )?;
    write_file(
        &claude_subagent,
        &format!(
            concat!(
                "{{\"parentUuid\":null,\"isSidechain\":true,\"promptId\":\"prompt-1\",\"agentId\":\"agent-a487e2adbf00a7a09\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"Explore the codebase\"}},\"uuid\":\"sub-user-1\",\"timestamp\":\"2026-04-01T11:01:01Z\",\"userType\":\"external\",\"entrypoint\":\"claude-desktop\",\"cwd\":\"{}\",\"sessionId\":\"{}\",\"version\":\"2.1.87\",\"gitBranch\":\"main\"}}\n",
                "{{\"parentUuid\":\"sub-user-1\",\"isSidechain\":true,\"agentId\":\"agent-a487e2adbf00a7a09\",\"message\":{{\"model\":\"claude-haiku-4-5-20251001\",\"id\":\"sub-assistant-1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"Subagent reply\"}}],\"stop_reason\":\"end_turn\",\"stop_sequence\":null}},\"requestId\":\"req-sub-1\",\"type\":\"assistant\",\"uuid\":\"sub-assistant-1\",\"timestamp\":\"2026-04-01T11:01:02Z\",\"userType\":\"external\",\"entrypoint\":\"claude-desktop\",\"cwd\":\"{}\",\"sessionId\":\"{}\",\"version\":\"2.1.87\",\"gitBranch\":\"main\"}}\n"
            ),
            project_root.display(),
            claude_session_id,
            project_root.display(),
            claude_session_id
        ),
    )?;

    let report = index_project_sessions_from(
        &project_root,
        darc_root.clone(),
        &[SourceKind::Claude, SourceKind::Codex],
    )?;

    assert_eq!(
        report.providers,
        vec![SourceKind::Claude, SourceKind::Codex]
    );
    assert_eq!(report.sessions_discovered, 3);
    assert_eq!(report.sessions_skipped_this_run, 0);
    assert_eq!(report.sessions_currently_indexed, 3);
    assert_eq!(report.turns_currently_indexed, 3);
    assert!(report.skipped_rollouts.is_empty());

    let connection = Connection::open(darc_root.join(INDEX_DB_FILE_NAME))?;
    let indexed_claude_sessions: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sessions WHERE project_id = ?1 AND provider = 'claude'",
        ["repo-abc123"],
        |row| row.get(0),
    )?;
    let indexed_codex_sessions = indexed_codex_session_count(&connection, "repo-abc123")?;
    let claude_parent_answer: String = connection.query_row(
        "
        SELECT final_answer_text
        FROM turns
        WHERE project_id = ?1 AND provider = 'claude' AND session_id = ?2 AND turn_ordinal = 0
        ",
        ["repo-abc123", claude_session_id],
        |row| row.get(0),
    )?;
    let claude_subagent_row: (String, String, String) = connection.query_row(
        "
        SELECT parent_session_id, session_kind, schema_id
        FROM sessions
        WHERE project_id = ?1 AND provider = 'claude' AND session_id = ?2
        ",
        [
            "repo-abc123",
            "885a05b8-f731-4fde-bfdb-a24ce28dc9c3/subagents/agent-a487e2adbf00a7a09",
        ],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;

    assert_eq!(indexed_claude_sessions, 2);
    assert_eq!(indexed_codex_sessions, 1);
    assert_eq!(claude_parent_answer, "Claude reply");
    assert_eq!(claude_subagent_row.0, claude_session_id);
    assert_eq!(claude_subagent_row.1, "subagent");
    assert_eq!(
        claude_subagent_row.2,
        "claude.subagent_transcript.2_1_84_to_2_1_89"
    );

    Ok(())
}

#[test]
fn index_project_rewrites_existing_indexed_turns() -> Result<()> {
    let darc_root = unique_test_dir("parse-rewrite");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    let rollout_path =
        codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    write_file(
        &rollout_path,
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Original task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Original reply\"}}]}}}}\n"
            ),
            project_root.display()
        ),
    )?;
    index_project_codex_turns_from(&project_root, darc_root.clone())?;

    write_file(
        &rollout_path,
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Updated task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Updated reply\"}}]}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:04Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-2\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:05Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Second task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:06Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Second reply\"}}]}}}}\n"
            ),
            project_root.display()
        ),
    )?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;
    let connection = Connection::open(darc_root.join(INDEX_DB_FILE_NAME))?;
    let indexed_turns = indexed_codex_turn_count(&connection, "repo-abc123")?;
    let first_turn: (String, String) = connection.query_row(
        "
        SELECT user_message, final_answer_text
        FROM turns
        WHERE project_id = ?1 AND provider = 'codex' AND session_id = ?2 AND turn_ordinal = 0
        ",
        ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    assert_eq!(report.turns_currently_indexed, 2);
    assert_eq!(indexed_turns, 2);
    assert_eq!(first_turn.0, "Updated task");
    assert_eq!(first_turn.1, "Updated reply");
    assert!(report.skipped_rollouts.is_empty());

    Ok(())
}

#[test]
fn index_project_skips_unchanged_sessions_when_snapshot_matches() -> Result<()> {
    let darc_root = unique_test_dir("parse-skip-unchanged");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    let rollout_path =
        codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    let original = format!(
        concat!(
            "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
            "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Original task\"}}}}\n",
            "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Original reply\"}}]}}}}\n"
        ),
        project_root.display()
    );
    write_file(&rollout_path, &original)?;
    touch_file_timestamp(&rollout_path, "202604011000.00")?;
    index_project_codex_turns_from(&project_root, darc_root.clone())?;

    write_file(&rollout_path, &"{".repeat(original.len()))?;
    touch_file_timestamp(&rollout_path, "202604011000.00")?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;
    let connection = Connection::open(darc_root.join(INDEX_DB_FILE_NAME))?;
    let indexed_turn: (String, String) = connection.query_row(
        "
        SELECT user_message, final_answer_text
        FROM turns
        WHERE project_id = ?1 AND provider = 'codex' AND session_id = ?2 AND turn_ordinal = 0
        ",
        ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    assert_eq!(report.sessions_discovered, 1);
    assert_eq!(report.sessions_currently_indexed, 1);
    assert_eq!(report.sessions_skipped_this_run, 0);
    assert_eq!(report.turns_currently_indexed, 1);
    assert_eq!(indexed_turn.0, "Original task");
    assert_eq!(indexed_turn.1, "Original reply");
    assert!(report.skipped_rollouts.is_empty());

    Ok(())
}

#[test]
fn index_project_deduplicates_archived_rollouts_with_same_session_id() -> Result<()> {
    let darc_root = unique_test_dir("parse-deduplicate");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    write_file(
        &codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Stale task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Stale reply\"}}]}}}}\n"
            ),
            project_root.display()
        ),
    )?;
    std::thread::sleep(std::time::Duration::from_millis(5));
    write_file(
        &codex_root.join("rollout-2026-04-01T11-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T11:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T11:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T11:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Fresh task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T11:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"commentary\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Checking\"}}]}}}}\n",
                "{{\"timestamp\":\"2026-04-01T11:00:04Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Fresh reply\"}}]}}}}\n"
            ),
            project_root.display()
        ),
    )?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;
    let connection = Connection::open(darc_root.join(INDEX_DB_FILE_NAME))?;
    let indexed_sessions = indexed_codex_session_count(&connection, "repo-abc123")?;
    let indexed_turns = indexed_codex_turn_count(&connection, "repo-abc123")?;
    let indexed_row: (String, String) = connection.query_row(
        "
        SELECT archive_path, user_message
        FROM sessions s
        JOIN turns t
          ON t.project_id = s.project_id
         AND t.provider = s.provider
         AND t.session_id = s.session_id
         AND t.turn_ordinal = 0
        WHERE s.project_id = ?1 AND s.provider = 'codex'
        ",
        ["repo-abc123"],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    assert_eq!(report.sessions_discovered, 1);
    assert_eq!(report.sessions_currently_indexed, 1);
    assert_eq!(report.sessions_skipped_this_run, 0);
    assert_eq!(report.turns_currently_indexed, 1);
    assert_eq!(indexed_sessions, 1);
    assert_eq!(indexed_turns, 1);
    assert_eq!(
        indexed_row.0,
        "codex/rollout-2026-04-01T11-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"
    );
    assert_eq!(indexed_row.1, "Fresh task");
    assert!(report.skipped_rollouts.is_empty());

    Ok(())
}

#[test]
fn index_project_skips_mismatched_filename_and_payload_session_ids() -> Result<()> {
    let darc_root = unique_test_dir("parse-id-mismatch");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    write_file(
        &codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e40\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Reply\"}}]}}}}\n"
            ),
            project_root.display()
        ),
    )?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;
    let connection = Connection::open(darc_root.join(INDEX_DB_FILE_NAME))?;
    let indexed_sessions = indexed_codex_session_count(&connection, "repo-abc123")?;

    assert_eq!(report.sessions_discovered, 1);
    assert_eq!(report.sessions_currently_indexed, 0);
    assert_eq!(report.sessions_skipped_this_run, 1);
    assert_eq!(report.turns_currently_indexed, 0);
    assert_eq!(indexed_sessions, 0);
    assert_eq!(report.skipped_rollouts.len(), 1);
    assert_eq!(
        report.skipped_rollouts[0].logical_session_id.as_deref(),
        Some("019d3415-0b9c-7dc3-88e0-e9cb7a789e3f")
    );
    assert_eq!(
        report.skipped_rollouts[0].cli_version.as_deref(),
        Some("0.118.0")
    );
    assert!(
        report.skipped_rollouts[0]
            .reason
            .contains("mismatched Codex session ids")
    );

    Ok(())
}

#[test]
fn index_project_ignores_corrupt_losing_duplicate() -> Result<()> {
    let darc_root = unique_test_dir("parse-corrupt-loser");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    write_file(
        &codex_root.join("rollout-2026-04-01T09-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        "{not-json\n",
    )?;
    std::thread::sleep(std::time::Duration::from_millis(5));
    write_file(
        &codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Fresh task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Fresh reply\"}}]}}}}\n"
            ),
            project_root.display()
        ),
    )?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;
    let connection = Connection::open(darc_root.join(INDEX_DB_FILE_NAME))?;
    let indexed_turn: (String, String) = connection.query_row(
        "
        SELECT user_message, final_answer_text
        FROM turns
        WHERE project_id = ?1 AND provider = 'codex' AND session_id = ?2 AND turn_ordinal = 0
        ",
        ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    assert_eq!(report.sessions_discovered, 1);
    assert_eq!(report.sessions_currently_indexed, 1);
    assert_eq!(report.sessions_skipped_this_run, 0);
    assert_eq!(report.turns_currently_indexed, 1);
    assert_eq!(indexed_turn.0, "Fresh task");
    assert_eq!(indexed_turn.1, "Fresh reply");
    assert!(report.skipped_rollouts.is_empty());

    Ok(())
}

#[test]
fn index_project_falls_back_when_selected_duplicate_is_corrupt() -> Result<()> {
    let darc_root = unique_test_dir("parse-corrupt-winner");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    write_file(
        &codex_root.join("rollout-2026-04-01T09-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T09:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T09:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Stale task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T09:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Stale reply\"}}]}}}}\n"
            ),
            project_root.display()
        ),
    )?;
    std::thread::sleep(std::time::Duration::from_millis(5));
    write_file(
        &codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        &format!("{{not-json\n{}\n", "x".repeat(4096)),
    )?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;
    let connection = Connection::open(darc_root.join(INDEX_DB_FILE_NAME))?;
    let indexed_row: (String, String, String) = connection.query_row(
        "
        SELECT s.archive_path, t.user_message, t.final_answer_text
        FROM sessions s
        JOIN turns t
          ON t.project_id = s.project_id
         AND t.provider = s.provider
         AND t.session_id = s.session_id
         AND t.turn_ordinal = 0
        WHERE s.project_id = ?1 AND s.provider = 'codex' AND s.session_id = ?2
        ",
        ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;

    assert_eq!(report.sessions_discovered, 1);
    assert_eq!(report.sessions_currently_indexed, 1);
    assert_eq!(report.sessions_skipped_this_run, 0);
    assert_eq!(report.turns_currently_indexed, 1);
    assert_eq!(
        indexed_row.0,
        "codex/rollout-2026-04-01T09-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"
    );
    assert_eq!(indexed_row.1, "Stale task");
    assert_eq!(indexed_row.2, "Stale reply");
    assert_eq!(report.skipped_rollouts.len(), 1);
    assert_eq!(
        report.skipped_rollouts[0].logical_session_id.as_deref(),
        Some("019d3415-0b9c-7dc3-88e0-e9cb7a789e3f")
    );
    assert!(
        report.skipped_rollouts[0]
            .reason
            .contains("failed to parse")
    );

    Ok(())
}

#[test]
fn index_project_skips_session_when_all_duplicate_candidates_are_corrupt() -> Result<()> {
    let darc_root = unique_test_dir("parse-all-corrupt-duplicates");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    write_file(
        &codex_root.join("rollout-2026-04-01T09-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        "{not-json\n",
    )?;
    std::thread::sleep(std::time::Duration::from_millis(5));
    write_file(
        &codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        &format!("{{not-json\n{}\n", "x".repeat(4096)),
    )?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;
    let connection = Connection::open(darc_root.join(INDEX_DB_FILE_NAME))?;
    let indexed_sessions = indexed_codex_session_count(&connection, "repo-abc123")?;

    assert_eq!(report.sessions_discovered, 1);
    assert_eq!(report.sessions_currently_indexed, 0);
    assert_eq!(report.sessions_skipped_this_run, 1);
    assert_eq!(report.turns_currently_indexed, 0);
    assert_eq!(indexed_sessions, 0);
    assert_eq!(report.skipped_rollouts.len(), 2);
    assert!(report.skipped_rollouts.iter().all(|skipped| {
        skipped.logical_session_id.as_deref() == Some("019d3415-0b9c-7dc3-88e0-e9cb7a789e3f")
    }));

    Ok(())
}

#[test]
fn index_project_preserves_previous_index_when_replacement_rollout_fails() -> Result<()> {
    let darc_root = unique_test_dir("parse-preserve-index-on-failure");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    let rollout_path =
        codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    let original = format!(
        concat!(
            "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
            "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Original task\"}}}}\n",
            "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Original reply\"}}]}}}}\n"
        ),
        project_root.display()
    );
    write_file(&rollout_path, &original)?;
    index_project_codex_turns_from(&project_root, darc_root.clone())?;

    write_file(&rollout_path, "{not-json\n")?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;
    let connection = Connection::open(darc_root.join(INDEX_DB_FILE_NAME))?;
    let indexed_turn: (String, String) = connection.query_row(
        "
        SELECT user_message, final_answer_text
        FROM turns
        WHERE project_id = ?1 AND provider = 'codex' AND session_id = ?2 AND turn_ordinal = 0
        ",
        ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    assert_eq!(report.sessions_discovered, 1);
    assert_eq!(report.sessions_currently_indexed, 1);
    assert_eq!(report.sessions_skipped_this_run, 1);
    assert_eq!(report.turns_currently_indexed, 1);
    assert_eq!(indexed_turn.0, "Original task");
    assert_eq!(indexed_turn.1, "Original reply");
    assert_eq!(report.skipped_rollouts.len(), 1);
    assert!(
        report.skipped_rollouts[0]
            .reason
            .contains("failed to parse")
    );

    Ok(())
}

#[test]
fn index_project_preserves_previous_index_when_replacement_header_mismatches() -> Result<()> {
    let darc_root = unique_test_dir("parse-preserve-index-on-mismatch");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    let rollout_path =
        codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    let original = format!(
        concat!(
            "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
            "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Original task\"}}}}\n",
            "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Original reply\"}}]}}}}\n"
        ),
        project_root.display()
    );
    write_file(&rollout_path, &original)?;
    index_project_codex_turns_from(&project_root, darc_root.clone())?;

    let mismatched = format!(
        concat!(
            "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"different-session-id\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
            "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Broken task\"}}}}\n"
        ),
        project_root.display()
    );
    write_file(&rollout_path, &mismatched)?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;
    let connection = Connection::open(darc_root.join(INDEX_DB_FILE_NAME))?;
    let indexed_turn: (String, String) = connection.query_row(
        "
        SELECT user_message, final_answer_text
        FROM turns
        WHERE project_id = ?1 AND provider = 'codex' AND session_id = ?2 AND turn_ordinal = 0
        ",
        ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    assert_eq!(report.sessions_discovered, 1);
    assert_eq!(report.sessions_currently_indexed, 1);
    assert_eq!(report.sessions_skipped_this_run, 1);
    assert_eq!(report.turns_currently_indexed, 1);
    assert_eq!(indexed_turn.0, "Original task");
    assert_eq!(indexed_turn.1, "Original reply");
    assert_eq!(report.skipped_rollouts.len(), 1);
    assert_eq!(
        report.skipped_rollouts[0].logical_session_id.as_deref(),
        Some("019d3415-0b9c-7dc3-88e0-e9cb7a789e3f")
    );
    assert!(
        report.skipped_rollouts[0]
            .reason
            .contains("mismatched Codex session ids")
    );

    Ok(())
}

#[test]
fn index_project_skips_unchanged_fallback_candidate_after_corrupt_higher_duplicate() -> Result<()> {
    let darc_root = unique_test_dir("parse-skip-fallback-duplicate");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    let fallback_path =
        codex_root.join("rollout-2026-04-01T09-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    let original = format!(
        concat!(
            "{{\"timestamp\":\"2026-04-01T09:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
            "{{\"timestamp\":\"2026-04-01T09:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Original task\"}}}}\n",
            "{{\"timestamp\":\"2026-04-01T09:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Original reply\"}}]}}}}\n"
        ),
        project_root.display()
    );
    write_file(&fallback_path, &original)?;
    touch_file_timestamp(&fallback_path, "202604010900.00")?;
    write_file(
        &codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        &format!("{{not-json\n{}\n", "x".repeat(4096)),
    )?;
    index_project_codex_turns_from(&project_root, darc_root.clone())?;

    write_file(&fallback_path, &"{".repeat(original.len()))?;
    touch_file_timestamp(&fallback_path, "202604010900.00")?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;
    let connection = Connection::open(darc_root.join(INDEX_DB_FILE_NAME))?;
    let indexed_turn: (String, String) = connection.query_row(
        "
        SELECT user_message, final_answer_text
        FROM turns
        WHERE project_id = ?1 AND provider = 'codex' AND session_id = ?2 AND turn_ordinal = 0
        ",
        ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    assert_eq!(report.sessions_discovered, 1);
    assert_eq!(report.sessions_currently_indexed, 1);
    assert_eq!(report.sessions_skipped_this_run, 0);
    assert_eq!(report.turns_currently_indexed, 1);
    assert_eq!(indexed_turn.0, "Original task");
    assert_eq!(indexed_turn.1, "Original reply");
    assert_eq!(report.skipped_rollouts.len(), 1);

    Ok(())
}

#[test]
fn index_project_skips_unknown_schema_session_while_indexing_other_sessions() -> Result<()> {
    let darc_root = unique_test_dir("parse-skip-unknown-schema");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    write_file(
        &codex_root.join("rollout-2026-04-01T09-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T09:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.32.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T09:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Too new\"}}}}\n"
            ),
            project_root.display()
        ),
    )?;
    write_file(
        &codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e40.jsonl"),
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e40\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Good task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Good reply\"}}]}}}}\n"
            ),
            project_root.display()
        ),
    )?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;
    let connection = Connection::open(darc_root.join(INDEX_DB_FILE_NAME))?;
    let indexed_sessions = indexed_codex_session_count(&connection, "repo-abc123")?;
    let indexed_turn: (String, String) = connection.query_row(
        "
        SELECT user_message, final_answer_text
        FROM turns
        WHERE project_id = ?1 AND provider = 'codex' AND session_id = ?2 AND turn_ordinal = 0
        ",
        ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e40"],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    assert_eq!(report.sessions_discovered, 2);
    assert_eq!(report.sessions_currently_indexed, 1);
    assert_eq!(report.sessions_skipped_this_run, 1);
    assert_eq!(report.turns_currently_indexed, 1);
    assert_eq!(indexed_sessions, 1);
    assert_eq!(indexed_turn.0, "Good task");
    assert_eq!(indexed_turn.1, "Good reply");
    assert_eq!(report.skipped_rollouts.len(), 1);
    assert_eq!(
        report.skipped_rollouts[0].logical_session_id.as_deref(),
        Some("019d3415-0b9c-7dc3-88e0-e9cb7a789e3f")
    );
    assert_eq!(
        report.skipped_rollouts[0].cli_version.as_deref(),
        Some("0.32.0")
    );
    assert!(
        report.skipped_rollouts[0]
            .reason
            .contains("unsupported Codex rollout schema")
    );

    Ok(())
}

#[test]
fn index_project_skips_bad_duplicate_group_and_continues_other_sessions() -> Result<()> {
    let darc_root = unique_test_dir("parse-skip-bad-group");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    write_file(
        &codex_root.join("rollout-2026-04-01T09-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        "{not-json\n",
    )?;
    std::thread::sleep(std::time::Duration::from_millis(5));
    write_file(
        &codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        &format!("{{not-json\n{}\n", "x".repeat(4096)),
    )?;
    write_file(
        &codex_root.join("rollout-2026-04-01T11-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e40.jsonl"),
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T11:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e40\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T11:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Healthy task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T11:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Healthy reply\"}}]}}}}\n"
            ),
            project_root.display()
        ),
    )?;

    let report = index_project_codex_turns_from(&project_root, darc_root.clone())?;
    let connection = Connection::open(darc_root.join(INDEX_DB_FILE_NAME))?;
    let indexed_sessions = indexed_codex_session_count(&connection, "repo-abc123")?;
    let indexed_turn: (String, String) = connection.query_row(
        "
        SELECT user_message, final_answer_text
        FROM turns
        WHERE project_id = ?1 AND provider = 'codex' AND session_id = ?2 AND turn_ordinal = 0
        ",
        ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e40"],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    assert_eq!(report.sessions_discovered, 2);
    assert_eq!(report.sessions_currently_indexed, 1);
    assert_eq!(report.sessions_skipped_this_run, 1);
    assert_eq!(report.turns_currently_indexed, 1);
    assert_eq!(indexed_sessions, 1);
    assert_eq!(indexed_turn.0, "Healthy task");
    assert_eq!(indexed_turn.1, "Healthy reply");
    assert_eq!(report.skipped_rollouts.len(), 2);
    assert!(report.skipped_rollouts.iter().all(|skipped| {
        skipped.logical_session_id.as_deref() == Some("019d3415-0b9c-7dc3-88e0-e9cb7a789e3f")
    }));

    Ok(())
}

#[cfg(unix)]
#[test]
fn index_project_still_fails_on_claude_rollout_file_read_errors() -> Result<()> {
    let darc_root = unique_test_dir("parse-hard-claude-file-read-error");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let claude_session_id = "885a05b8-f731-4fde-bfdb-a24ce28dc9c3";
    let rollout_path = sessions_root
        .join("claude")
        .join(claude_session_id)
        .join(format!("{claude_session_id}.jsonl"));
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    write_file(
        &rollout_path,
        &format!(
            concat!(
                "{{\"type\":\"queue-operation\",\"operation\":\"enqueue\",\"timestamp\":\"2026-04-01T11:00:00Z\",\"sessionId\":\"{}\",\"content\":\"Inspect parse.rs\"}}\n",
                "{{\"parentUuid\":null,\"isSidechain\":false,\"promptId\":\"prompt-1\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"Inspect parse.rs\"}},\"uuid\":\"user-1\",\"timestamp\":\"2026-04-01T11:00:01Z\",\"userType\":\"external\",\"entrypoint\":\"claude-desktop\",\"cwd\":\"{}\",\"sessionId\":\"{}\",\"version\":\"2.1.87\",\"gitBranch\":\"main\"}}\n",
                "{{\"parentUuid\":\"user-1\",\"isSidechain\":false,\"message\":{{\"model\":\"claude-sonnet-4-6\",\"id\":\"assistant-1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"Claude reply\"}}],\"stop_reason\":\"end_turn\",\"stop_sequence\":null}},\"requestId\":\"req-1\",\"type\":\"assistant\",\"uuid\":\"assistant-1\",\"timestamp\":\"2026-04-01T11:00:02Z\",\"userType\":\"external\",\"entrypoint\":\"claude-desktop\",\"cwd\":\"{}\",\"sessionId\":\"{}\",\"version\":\"2.1.87\",\"gitBranch\":\"main\"}}\n"
            ),
            claude_session_id,
            project_root.display(),
            claude_session_id,
            project_root.display(),
            claude_session_id
        ),
    )?;
    fs::set_permissions(&rollout_path, fs::Permissions::from_mode(0o000))?;

    let error = index_project_sessions_from(&project_root, darc_root, &[SourceKind::Claude])
        .expect_err("hard read error");

    assert!(
        error.to_string().contains("failed") || error.to_string().contains("Permission denied")
    );

    Ok(())
}

#[cfg(unix)]
#[test]
fn index_project_still_fails_on_rollout_file_read_errors() -> Result<()> {
    let darc_root = unique_test_dir("parse-hard-file-read-error");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let codex_root = sessions_root.join("codex");
    let rollout_path =
        codex_root.join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
    fs::create_dir_all(&project_root)?;
    write_parse_config(&darc_root, &project_root, &sessions_root)?;

    write_file(
        &rollout_path,
        &format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Reply\"}}]}}}}\n"
            ),
            project_root.display()
        ),
    )?;
    fs::set_permissions(&rollout_path, fs::Permissions::from_mode(0o000))?;

    let error =
        index_project_codex_turns_from(&project_root, darc_root).expect_err("hard read error");

    assert!(
        error.to_string().contains("failed") || error.to_string().contains("Permission denied")
    );

    Ok(())
}
