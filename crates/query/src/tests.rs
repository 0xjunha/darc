use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use darc_index::{
    open_index_database,
    policy::{
        ToolAccessKind, active_time_policy, classify_tool_access, derive_file_access_records,
        extract_tool_call_records, extract_tool_path, extract_tool_paths,
        should_include_turn_in_active_time,
    },
};
use darc_paths::SourceKind;
use darc_rollout::model::NormalizedTurnStep;
use darc_test_utils::{
    IndexedSessionFixture, IndexedTurnFixture, insert_indexed_session, insert_indexed_turn,
    unique_test_dir,
};

use crate::query::{
    HardDebuggingTurn, LocalDate, ProjectInsights, SessionKind, TurnInsights,
    build_project_insights, build_turn_insights, build_workspace_insights,
    open_existing_index_database, parse_session_kind,
};

/// Builds one temporary SQLite index path for query tests.
fn test_index_path(prefix: &str) -> PathBuf {
    unique_test_dir(prefix).join("index.sqlite")
}

/// Resolves one UTC timestamp into the host-local civil day used by SQLite localtime.
fn sqlite_local_date(connection: &rusqlite::Connection, timestamp: &str) -> Result<String> {
    connection
        .query_row("SELECT DATE(?1, 'localtime')", [timestamp], |row| {
            row.get(0)
        })
        .context("failed to derive SQLite local date")
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
    assert!(matches!(classify_tool_access("Grep"), ToolAccessKind::Read));
    assert!(matches!(
        classify_tool_access("ListFiles"),
        ToolAccessKind::List
    ));
    assert!(matches!(classify_tool_access("Glob"), ToolAccessKind::List));
    assert!(matches!(classify_tool_access("Edit"), ToolAccessKind::Edit));
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
    assert_eq!(
        extract_tool_paths(r#"{"file":["README.md","src/main.rs"]}"#),
        vec!["README.md".to_owned(), "src/main.rs".to_owned()]
    );
    assert!(extract_tool_path("*** Begin Patch").is_none());
}

#[test]
fn matches_tool_call_outputs_and_keeps_unmatched_rows() {
    let steps = vec![
        NormalizedTurnStep::ToolCall {
            timestamp: "2026-04-06T10:00:01Z".to_owned(),
            call_id: "call-1".to_owned(),
            name: "Read".to_owned(),
            arguments: r#"{"file_path":"README.md"}"#.to_owned(),
        },
        NormalizedTurnStep::ToolCallOutput {
            timestamp: "2026-04-06T10:00:02Z".to_owned(),
            call_id: "call-1".to_owned(),
            output: "# README".to_owned(),
        },
        NormalizedTurnStep::ToolCallOutput {
            timestamp: "2026-04-06T10:00:03Z".to_owned(),
            call_id: "call-2".to_owned(),
            output: r#"{"status":"error","error":"boom"}"#.to_owned(),
        },
        NormalizedTurnStep::ToolCall {
            timestamp: "2026-04-06T10:00:04Z".to_owned(),
            call_id: "call-3".to_owned(),
            name: "Edit".to_owned(),
            arguments: r#"{"path":"src/main.rs"}"#.to_owned(),
        },
    ];

    let records = extract_tool_call_records("repo-a", SourceKind::Codex, "session-1", 7, &steps);

    assert_eq!(records.len(), 3);
    assert_eq!(records[0].tool_name.as_deref(), Some("Read"));
    assert_eq!(records[0].output_text.as_deref(), Some("# README"));
    assert_eq!(records[1].tool_name, None);
    assert_eq!(records[1].status.as_deref(), Some("error"));
    assert!(records[1].is_error);
    assert_eq!(records[2].tool_name.as_deref(), Some("Edit"));
    assert_eq!(records[2].output_text, None);
}

#[test]
fn derives_file_accesses_from_normalized_tool_calls() {
    let steps = vec![
        NormalizedTurnStep::ToolCall {
            timestamp: "2026-04-06T10:00:01Z".to_owned(),
            call_id: "call-1".to_owned(),
            name: "ListFiles".to_owned(),
            arguments: r#"{"file":["README.md","src/main.rs"]}"#.to_owned(),
        },
        NormalizedTurnStep::ToolCall {
            timestamp: "2026-04-06T10:00:02Z".to_owned(),
            call_id: "call-2".to_owned(),
            name: "Edit".to_owned(),
            arguments: r#"{"path":"src/lib.rs"}"#.to_owned(),
        },
    ];

    let tool_calls = extract_tool_call_records("repo-a", SourceKind::Codex, "session-1", 0, &steps);
    let file_accesses = derive_file_access_records(&tool_calls);

    assert_eq!(file_accesses.len(), 3);
    assert!(file_accesses.iter().any(|record| {
        record.path == "README.md"
            && matches!(record.access_type, ToolAccessKind::List)
            && record.repo_relative_path.as_deref() == Some("README.md")
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "src/lib.rs" && matches!(record.access_type, ToolAccessKind::Edit)
    }));
}

#[test]
fn derives_file_accesses_from_shell_commands_and_patches() {
    let steps = vec![
        NormalizedTurnStep::ToolCall {
            timestamp: "2026-04-06T10:00:01Z".to_owned(),
            call_id: "call-1".to_owned(),
            name: "exec_command".to_owned(),
            arguments: r#"{"cmd":"sed -n '1,200p' README.md && rg -n \"fn main\" src/main.rs && cat > notes.txt <<'EOF'\nhello\nEOF","workdir":"/tmp/repo"}"#.to_owned(),
        },
        NormalizedTurnStep::ToolCall {
            timestamp: "2026-04-06T10:00:02Z".to_owned(),
            call_id: "call-2".to_owned(),
            name: "shell".to_owned(),
            arguments: r#"{"command":["bash","-lc","cp src/main.rs src/main.rs.bak && mv old.rs new.rs && ls src"],"workdir":"/tmp/repo"}"#.to_owned(),
        },
        NormalizedTurnStep::ToolCall {
            timestamp: "2026-04-06T10:00:03Z".to_owned(),
            call_id: "call-3".to_owned(),
            name: "apply_patch".to_owned(),
            arguments: "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n*** Add File: src/new.rs\n+fn main() {}\n*** End Patch\n".to_owned(),
        },
    ];

    let tool_calls = extract_tool_call_records("repo-a", SourceKind::Codex, "session-1", 0, &steps);
    let file_accesses = derive_file_access_records(&tool_calls);

    assert!(file_accesses.iter().any(|record| {
        record.path == "README.md" && matches!(record.access_type, ToolAccessKind::Read)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "src/main.rs" && matches!(record.access_type, ToolAccessKind::Read)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "notes.txt" && matches!(record.access_type, ToolAccessKind::Write)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "src/main.rs.bak" && matches!(record.access_type, ToolAccessKind::Write)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "old.rs" && matches!(record.access_type, ToolAccessKind::Edit)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "new.rs" && matches!(record.access_type, ToolAccessKind::Write)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "src" && matches!(record.access_type, ToolAccessKind::List)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "src/lib.rs" && matches!(record.access_type, ToolAccessKind::Edit)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "src/new.rs" && matches!(record.access_type, ToolAccessKind::Write)
    }));
}

#[test]
fn derives_file_accesses_from_script_runners_and_output_flags() {
    let steps = vec![
        NormalizedTurnStep::ToolCall {
            timestamp: "2026-04-06T10:00:01Z".to_owned(),
            call_id: "call-1".to_owned(),
            name: "exec_command".to_owned(),
            arguments: r#"{"cmd":"bash ./scripts/check.sh && cargo fmt -- crates/core/src/sync.rs && cargo test --manifest-path Cargo.toml && curl -o /tmp/out.txt https://example.com","workdir":"/tmp/repo"}"#.to_owned(),
        },
        NormalizedTurnStep::ToolCall {
            timestamp: "2026-04-06T10:00:02Z".to_owned(),
            call_id: "call-2".to_owned(),
            name: "Bash".to_owned(),
            arguments: r#"{"command":"rustfmt src/shared/types.rs && node scripts/build.js","description":"run toolchain"}"#.to_owned(),
        },
        NormalizedTurnStep::ToolCall {
            timestamp: "2026-04-06T10:00:03Z".to_owned(),
            call_id: "call-3".to_owned(),
            name: "exec_command".to_owned(),
            arguments: r#"{"cmd":"python3 - <<'PY'\nprint('hi')\nPY","workdir":"/tmp/repo"}"#.to_owned(),
        },
    ];

    let tool_calls = extract_tool_call_records("repo-a", SourceKind::Codex, "session-1", 0, &steps);
    let file_accesses = derive_file_access_records(&tool_calls);

    assert!(file_accesses.iter().any(|record| {
        record.path == "./scripts/check.sh" && matches!(record.access_type, ToolAccessKind::Read)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "crates/core/src/sync.rs"
            && matches!(record.access_type, ToolAccessKind::Edit)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "Cargo.toml" && matches!(record.access_type, ToolAccessKind::Read)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "/tmp/out.txt" && matches!(record.access_type, ToolAccessKind::Write)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "src/shared/types.rs" && matches!(record.access_type, ToolAccessKind::Read)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "scripts/build.js" && matches!(record.access_type, ToolAccessKind::Read)
    }));
    assert!(!file_accesses.iter().any(|record| record.path == "<<PY"));
}

#[test]
fn active_time_policy_requires_completed_turns_and_two_seconds() {
    let policy = active_time_policy();

    assert_eq!(policy.min_duration_ms, 2_000);
    assert!(should_include_turn_in_active_time(
        darc_rollout::model::NormalizedTurnStatus::Completed,
        2_000,
    ));
    assert!(should_include_turn_in_active_time(
        darc_rollout::model::NormalizedTurnStatus::Completed,
        7_200_000,
    ));
    assert!(!should_include_turn_in_active_time(
        darc_rollout::model::NormalizedTurnStatus::Completed,
        1_999,
    ));
    assert!(!should_include_turn_in_active_time(
        darc_rollout::model::NormalizedTurnStatus::Incomplete,
        7_200_000,
    ));
}

#[test]
fn workspace_insights_filter_short_and_failed_turns() -> Result<()> {
    let index_path = test_index_path("workspace-insights");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-b", SourceKind::Claude, "session-2", "/tmp/repo-b"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 3,
            tool_call_count: 0,
            tool_output_count: 0,
            attachment_count: 0,
            delegation_count: 0,
            hook_summary_count: 0,
            has_final_answer: true,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-05T12:00:00Z",
                "completed",
                "[]",
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 0,
            tool_output_count: 0,
            attachment_count: 0,
            delegation_count: 0,
            hook_summary_count: 0,
            has_final_answer: true,
            duration_ms: 1_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-05T13:00:00Z",
                "completed",
                "[]",
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 10,
            tool_call_count: 0,
            tool_output_count: 0,
            attachment_count: 0,
            delegation_count: 0,
            hook_summary_count: 0,
            has_final_answer: false,
            duration_ms: 9_000,
            ..IndexedTurnFixture::new(
                "repo-b",
                SourceKind::Claude,
                "session-2",
                0,
                "2026-04-06T08:00:00Z",
                "aborted",
                "[]",
            )
        },
    )?;

    let insights = build_workspace_insights(&connection, 7)?;

    assert_eq!(
        insights.window_end,
        sqlite_local_date(&connection, "2026-04-06T08:00:00Z")?
    );
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
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 2,
            tool_call_count: 2,
            tool_output_count: 0,
            attachment_count: 0,
            delegation_count: 0,
            hook_summary_count: 0,
            has_final_answer: true,
            duration_ms: 5_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                r#"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"},{"type":"tool_call","timestamp":"2026-04-06T10:00:02Z","call_id":"call-2","name":"Edit","arguments":"{\"path\":\"src/main.rs\"}"}]"#,
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 55,
            tool_call_count: 0,
            tool_output_count: 0,
            attachment_count: 0,
            delegation_count: 0,
            hook_summary_count: 0,
            has_final_answer: false,
            duration_ms: 4_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-06T10:10:00Z",
                "incomplete",
                "[]",
            )
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
fn turn_insights_collect_turn_scoped_stats_and_ordering() -> Result<()> {
    let index_path = test_index_path("turn-insights");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 9,
            tool_call_count: 4,
            tool_output_count: 2,
            attachment_count: 1,
            delegation_count: 1,
            hook_summary_count: 1,
            has_final_answer: true,
            duration_ms: 12_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T11:00:00Z",
                "completed",
                r#"[{"type":"tool_call","timestamp":"2026-04-06T11:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"},{"type":"tool_call","timestamp":"2026-04-06T11:00:02Z","call_id":"call-2","name":"Edit","arguments":"{\"path\":\"src/main.rs\"}"},{"type":"tool_call","timestamp":"2026-04-06T11:00:03Z","call_id":"call-3","name":"Read","arguments":"{\"file_path\":\"README.md\"}"},{"type":"tool_call","timestamp":"2026-04-06T11:00:04Z","call_id":"call-4","name":"Edit","arguments":"{\"path\":\"src/main.rs\"}"}]"#,
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            tool_output_count: 0,
            attachment_count: 0,
            delegation_count: 0,
            hook_summary_count: 0,
            has_final_answer: true,
            duration_ms: 1_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-06T11:05:00Z",
                "completed",
                r#"[{"type":"tool_call","timestamp":"2026-04-06T11:05:01Z","call_id":"call-9","name":"Write","arguments":"{\"path\":\"ignored.txt\"}"}]"#,
            )
        },
    )?;

    let insights: TurnInsights =
        build_turn_insights(&connection, "repo-a", SourceKind::Codex, "session-1", 0)?;

    assert_eq!(insights.project_id, "repo-a");
    assert_eq!(insights.provider, SourceKind::Codex);
    assert_eq!(insights.session_id, "session-1");
    assert_eq!(insights.turn_ordinal, 0);
    assert_eq!(
        insights.status,
        darc_rollout::model::NormalizedTurnStatus::Completed
    );
    assert_eq!(insights.duration_ms, 12_000);
    assert_eq!(insights.step_count, 9);
    assert_eq!(insights.tool_call_count, 4);
    assert_eq!(insights.tool_output_count, 2);
    assert_eq!(insights.attachment_count, 1);
    assert_eq!(insights.delegation_count, 1);
    assert_eq!(insights.hook_summary_count, 1);
    assert!(insights.has_final_answer);
    assert_eq!(
        insights
            .tools
            .iter()
            .map(|stat| (stat.name.as_str(), stat.count))
            .collect::<Vec<_>>(),
        vec![("Edit", 2), ("Read", 2)]
    );
    assert_eq!(
        insights
            .files
            .iter()
            .map(|stat| (stat.path.as_str(), stat.read_count, stat.write_count))
            .collect::<Vec<_>>(),
        vec![("src/main.rs", 0, 2), ("README.md", 2, 0)]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn turn_insights_return_empty_tool_and_file_lists() -> Result<()> {
    let index_path = test_index_path("turn-insights-empty");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 0,
            tool_call_count: 0,
            tool_output_count: 0,
            attachment_count: 0,
            delegation_count: 0,
            hook_summary_count: 0,
            has_final_answer: false,
            duration_ms: 0,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T12:00:00Z",
                "completed",
                "[]",
            )
        },
    )?;

    let insights = build_turn_insights(&connection, "repo-a", SourceKind::Codex, "session-1", 0)?;

    assert!(insights.tools.is_empty());
    assert!(insights.files.is_empty());
    assert_eq!(insights.tool_call_count, 0);
    assert_eq!(insights.tool_output_count, 0);

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn turn_insights_report_missing_turn_errors() -> Result<()> {
    let index_path = test_index_path("turn-insights-missing");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;

    let error = build_turn_insights(&connection, "repo-a", SourceKind::Codex, "session-1", 9)
        .expect_err("missing turns should error");

    assert!(
        error
            .to_string()
            .contains("turn 9 was not found in session session-1 for provider codex")
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn local_date_add_days_round_trips() -> Result<()> {
    let date = LocalDate::parse("2026-04-06").context("fixture date should parse")?;
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
