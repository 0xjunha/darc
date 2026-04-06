use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use darc_index::{
    INDEX_DB_FILE_NAME, ProjectIndexRequest, index_project_archived_sessions, open_index_database,
};
use darc_paths::SourceKind;
use darc_test_utils::{unique_test_dir, write_file};

use crate::{
    ProjectAnalyticsRequest,
    query::{
        HardDebuggingTurn, ProjectInsights, SessionKind, ToolAccessKind, UtcDate,
        build_project_insights, build_workspace_insights, classify_tool_access, extract_tool_path,
        open_existing_index_database, parse_session_kind,
    },
    report_project_claude_rollout_analytics,
};

/// Stores one normalized turn fixture used to seed query tests.
struct TurnFixture<'a> {
    project_id: &'a str,
    provider: &'a str,
    session_id: &'a str,
    turn_ordinal: i64,
    started_at: &'a str,
    status: &'a str,
    steps_json: &'a str,
    step_count: i64,
    tool_call_count: i64,
    has_final_answer: bool,
    duration_ms: i64,
}

/// Builds one temporary SQLite index path for query tests.
fn test_index_path(prefix: &str) -> PathBuf {
    unique_test_dir(prefix).join("index.sqlite")
}

/// Inserts one normalized session row for query tests.
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

/// Inserts one normalized turn row for query tests.
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
        ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, 'Inspect the repo', ?8, ?9, ?10, ?11, ?12, 0, 0, 0, 0, ?13, ?14)
        ",
        (
            fixture.project_id,
            fixture.provider,
            fixture.session_id,
            fixture.turn_ordinal,
            fixture.started_at,
            fixture.started_at,
            fixture.status,
            fixture.has_final_answer.then_some(fixture.started_at),
            fixture.has_final_answer.then_some("done"),
            fixture.steps_json,
            fixture.step_count,
            fixture.tool_call_count,
            i64::from(fixture.has_final_answer),
            fixture.duration_ms,
        ),
    )?;
    Ok(())
}

#[test]
fn reports_indexed_claude_rollout_analytics() -> Result<()> {
    let darc_root = unique_test_dir("claude-analytics");
    let project_root = darc_root.join("repo");
    let sessions_root = darc_root.join("projects/repo-abc123/sessions");
    let claude_root = sessions_root.join("claude");
    let session_id = "session-analytics";
    let session_path = claude_root
        .join(session_id)
        .join(format!("{session_id}.jsonl"));
    fs::create_dir_all(&project_root)?;

    write_file(
        &session_path,
        concat!(
            "{\"parentUuid\":null,\"isSidechain\":false,\"promptId\":\"prompt-1\",\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Delegate README.md\"},\"uuid\":\"user-1\",\"timestamp\":\"2026-04-01T00:00:01Z\",\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",\"sessionId\":\"session-analytics\",\"version\":\"2.1.90\",\"gitBranch\":\"HEAD\"}\n",
            "{\"parentUuid\":\"user-1\",\"isSidechain\":false,\"attachment\":{\"type\":\"deferred_tools_delta\",\"addedNames\":[\"Agent\"],\"addedLines\":[\"Agent\"],\"removedNames\":[]},\"type\":\"attachment\",\"uuid\":\"attachment-1\",\"timestamp\":\"2026-04-01T00:00:01Z\",\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",\"sessionId\":\"session-analytics\",\"version\":\"2.1.90\",\"gitBranch\":\"HEAD\"}\n",
            "{\"parentUuid\":\"attachment-1\",\"isSidechain\":false,\"message\":{\"model\":\"claude-sonnet-4-6\",\"id\":\"assistant-1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"tool-1\",\"name\":\"Agent\",\"input\":{\"description\":\"Read README heading\",\"prompt\":\"Read README.md and return the first heading.\",\"subagent_type\":\"general-purpose\"}}],\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"requestId\":\"req-1\",\"type\":\"assistant\",\"uuid\":\"assistant-1\",\"timestamp\":\"2026-04-01T00:00:02Z\",\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",\"sessionId\":\"session-analytics\",\"version\":\"2.1.90\",\"gitBranch\":\"HEAD\"}\n",
            "{\"parentUuid\":\"assistant-1\",\"isSidechain\":false,\"type\":\"system\",\"subtype\":\"task_started\",\"task_id\":\"task-1\",\"tool_use_id\":\"tool-1\",\"description\":\"Read README heading\",\"task_type\":\"local_agent\",\"prompt\":\"Read README.md and return the first heading.\",\"uuid\":\"system-1\",\"timestamp\":\"2026-04-01T00:00:03Z\",\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",\"sessionId\":\"session-analytics\",\"version\":\"2.1.90\",\"gitBranch\":\"HEAD\"}\n",
            "{\"parentUuid\":\"assistant-1\",\"isSidechain\":false,\"promptId\":\"prompt-1\",\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"tool_use_id\":\"tool-1\",\"type\":\"tool_result\",\"content\":[{\"type\":\"text\",\"text\":\"# Audit Fixture\"},{\"type\":\"text\",\"text\":\"agentId: agent-1\"}]}]},\"uuid\":\"user-2\",\"timestamp\":\"2026-04-01T00:00:04Z\",\"toolUseResult\":{\"status\":\"completed\",\"prompt\":\"Read README.md and return the first heading.\",\"agentId\":\"agent-1\",\"agentType\":\"general-purpose\",\"content\":[{\"type\":\"text\",\"text\":\"# Audit Fixture\"}],\"totalDurationMs\":12,\"totalTokens\":34,\"totalToolUseCount\":1},\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",\"sessionId\":\"session-analytics\",\"version\":\"2.1.90\",\"gitBranch\":\"HEAD\"}\n",
            "{\"parentUuid\":\"user-2\",\"isSidechain\":false,\"type\":\"system\",\"subtype\":\"stop_hook_summary\",\"hookCount\":2,\"hookInfos\":[{\"command\":\"callback\",\"durationMs\":12}],\"hookErrors\":[],\"preventedContinuation\":false,\"stopReason\":\"\",\"hasOutput\":true,\"level\":\"suggestion\",\"timestamp\":\"2026-04-01T00:00:05Z\",\"uuid\":\"system-2\",\"toolUseID\":\"tool-1\",\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",\"sessionId\":\"session-analytics\",\"version\":\"2.1.90\",\"gitBranch\":\"HEAD\"}\n",
            "{\"parentUuid\":\"user-2\",\"isSidechain\":false,\"message\":{\"model\":\"claude-sonnet-4-6\",\"id\":\"assistant-2\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"# Audit Fixture\"}],\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"requestId\":\"req-2\",\"type\":\"assistant\",\"uuid\":\"assistant-2\",\"timestamp\":\"2026-04-01T00:00:06Z\",\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",\"sessionId\":\"session-analytics\",\"version\":\"2.1.90\",\"gitBranch\":\"HEAD\"}\n"
        ),
    )?;

    let _report = index_project_archived_sessions(
        &ProjectIndexRequest {
            project_id: "repo-abc123".into(),
            project_name: "repo".into(),
            project_root: fs::canonicalize(&project_root)?,
            sessions_root: sessions_root.clone(),
            index_db_path: darc_root.join(INDEX_DB_FILE_NAME),
        },
        &[SourceKind::Claude],
    )?;
    let analytics = report_project_claude_rollout_analytics(&ProjectAnalyticsRequest {
        project_id: "repo-abc123".into(),
        project_name: "repo".into(),
        project_root: fs::canonicalize(&project_root)?,
        index_db_path: darc_root.join(INDEX_DB_FILE_NAME),
    })?;

    assert_eq!(analytics.project_name, "repo");
    assert_eq!(analytics.project_root, fs::canonicalize(&project_root)?);
    assert_eq!(analytics.index_db_path, darc_root.join(INDEX_DB_FILE_NAME));
    assert_eq!(analytics.sessions_total, 1);
    assert_eq!(analytics.primary_sessions, 1);
    assert_eq!(analytics.subagent_sessions, 0);
    assert_eq!(analytics.exact_sessions, 0);
    assert_eq!(analytics.best_effort_sessions, 1);
    assert_eq!(analytics.turns_total, 1);
    assert_eq!(analytics.completed_turns, 1);
    assert_eq!(analytics.incomplete_turns, 0);
    assert_eq!(analytics.aborted_turns, 0);
    assert_eq!(analytics.turns_with_final_answer, 1);
    assert_eq!(analytics.turns_with_attachments, 1);
    assert_eq!(analytics.turns_with_delegation, 1);
    assert_eq!(analytics.total_step_count, 6);
    assert_eq!(analytics.total_tool_calls, 1);
    assert_eq!(analytics.total_tool_outputs, 1);
    assert_eq!(analytics.total_attachments, 1);
    assert_eq!(analytics.total_delegation_events, 2);
    assert_eq!(analytics.total_hook_summaries, 1);
    assert_eq!(analytics.total_duration_ms, 5_000);
    assert_eq!(analytics.average_duration_ms, Some(5_000.0));
    assert_eq!(analytics.schemas.len(), 1);
    assert_eq!(
        analytics.schemas[0].schema_id,
        "claude.primary_transcript.2_1_90_to_latest"
    );
    assert_eq!(analytics.schemas[0].session_count, 1);
    assert_eq!(analytics.schemas[0].turn_count, 1);
    assert_eq!(analytics.schemas[0].exact_session_count, 0);
    assert_eq!(analytics.schemas[0].best_effort_session_count, 1);

    Ok(())
}

#[test]
fn parses_session_kinds() -> Result<()> {
    assert_eq!(parse_session_kind("primary")?, SessionKind::Primary);
    assert_eq!(parse_session_kind("subagent")?, SessionKind::Subagent);
    Ok(())
}

#[test]
fn rejects_missing_existing_index_database() {
    let error = open_existing_index_database(&test_index_path("missing")).unwrap_err();

    assert!(error.to_string().contains("index database not found"));
}

#[test]
fn classifies_tool_access_names() {
    assert!(matches!(classify_tool_access("Read"), ToolAccessKind::Read));
    assert!(matches!(
        classify_tool_access("WriteFile"),
        ToolAccessKind::Write
    ));
    assert!(matches!(
        classify_tool_access("exec_command"),
        ToolAccessKind::Other
    ));
}

#[test]
fn extracts_file_paths_from_tool_arguments() {
    assert_eq!(
        extract_tool_path(r#"{"file_path":"README.md"}"#).as_deref(),
        Some("README.md")
    );
    assert_eq!(
        extract_tool_path(r#"{"path":"/tmp/repo/src/main.rs"}"#).as_deref(),
        Some("/tmp/repo/src/main.rs")
    );
    assert!(extract_tool_path("*** Begin Patch").is_none());
}

#[test]
fn workspace_insights_filter_short_and_failed_turns() -> Result<()> {
    let index_path = test_index_path("workspace-insights");
    let connection = open_index_database(&index_path)?;
    insert_session(
        &connection,
        "repo-a",
        "codex",
        "session-1",
        None,
        "primary",
        "/tmp/repo-a",
    )?;
    insert_session(
        &connection,
        "repo-b",
        "claude",
        "session-2",
        None,
        "primary",
        "/tmp/repo-b",
    )?;
    insert_turn(
        &connection,
        TurnFixture {
            project_id: "repo-a",
            provider: "codex",
            session_id: "session-1",
            turn_ordinal: 0,
            started_at: "2026-04-05T12:00:00Z",
            status: "completed",
            steps_json: "[]",
            step_count: 3,
            tool_call_count: 0,
            has_final_answer: true,
            duration_ms: 3_000,
        },
    )?;
    insert_turn(
        &connection,
        TurnFixture {
            project_id: "repo-a",
            provider: "codex",
            session_id: "session-1",
            turn_ordinal: 1,
            started_at: "2026-04-05T13:00:00Z",
            status: "completed",
            steps_json: "[]",
            step_count: 1,
            tool_call_count: 0,
            has_final_answer: true,
            duration_ms: 1_000,
        },
    )?;
    insert_turn(
        &connection,
        TurnFixture {
            project_id: "repo-b",
            provider: "claude",
            session_id: "session-2",
            turn_ordinal: 0,
            started_at: "2026-04-06T08:00:00Z",
            status: "aborted",
            steps_json: "[]",
            step_count: 10,
            tool_call_count: 0,
            has_final_answer: false,
            duration_ms: 9_000,
        },
    )?;

    let insights = build_workspace_insights(&connection, 7)?;

    assert_eq!(insights.window_end, "2026-04-06");
    assert_eq!(insights.active_session_count, 1);
    assert_eq!(insights.included_turn_count, 1);
    assert_eq!(insights.excluded_turn_count, 2);
    assert_eq!(insights.total_time_ms, 3_000);
    assert_eq!(insights.recent_sessions.len(), 1);
    assert_eq!(insights.recent_sessions[0].project_id, "repo-a");

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn project_insights_collect_tool_and_file_stats() -> Result<()> {
    let index_path = test_index_path("project-insights");
    let connection = open_index_database(&index_path)?;
    insert_session(
        &connection,
        "repo-a",
        "codex",
        "session-1",
        None,
        "primary",
        "/tmp/repo-a",
    )?;
    insert_turn(
        &connection,
        TurnFixture {
            project_id: "repo-a",
            provider: "codex",
            session_id: "session-1",
            turn_ordinal: 0,
            started_at: "2026-04-06T10:00:00Z",
            status: "completed",
            steps_json: r#"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"},{"type":"tool_call","timestamp":"2026-04-06T10:00:02Z","call_id":"call-2","name":"Edit","arguments":"{\"path\":\"src/main.rs\"}"}]"#,
            step_count: 2,
            tool_call_count: 2,
            has_final_answer: true,
            duration_ms: 5_000,
        },
    )?;
    insert_turn(
        &connection,
        TurnFixture {
            project_id: "repo-a",
            provider: "codex",
            session_id: "session-1",
            turn_ordinal: 1,
            started_at: "2026-04-06T10:10:00Z",
            status: "incomplete",
            steps_json: "[]",
            step_count: 55,
            tool_call_count: 0,
            has_final_answer: false,
            duration_ms: 4_000,
        },
    )?;

    let insights: ProjectInsights = build_project_insights(&connection, "repo-a", 1000)?;

    assert_eq!(insights.failure_count, 1);
    assert_eq!(insights.total_time_ms, 5_000);
    assert_eq!(insights.most_common_tools[0].name, "Edit");
    assert!(
        insights
            .most_read_files
            .iter()
            .any(|stat| stat.path == "README.md" && stat.read_count == 1)
    );
    assert!(
        insights
            .most_written_files
            .iter()
            .any(|stat| stat.path == "src/main.rs" && stat.write_count == 1)
    );
    assert!(matches!(
        insights.hard_debuggings[0],
        HardDebuggingTurn { step_count: 55, .. }
    ));

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn utc_date_add_days_round_trips() -> Result<()> {
    let date = UtcDate::from_timestamp_prefix("2026-04-06T00:00:00Z")
        .context("fixture timestamp should parse")?;
    assert_eq!(
        date.add_days(-6)
            .context("date subtraction should work")?
            .to_string(),
        "2026-03-31"
    );
    assert_eq!(
        date.add_days(1)
            .context("date addition should work")?
            .to_string(),
        "2026-04-07"
    );
    Ok(())
}
