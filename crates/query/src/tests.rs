use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use darc_index::{
    open_index_database,
    policy::{
        ToolAccessKind, active_time_policy, classify_tool_access, derive_file_access_records,
        extract_shell_command, extract_tool_call_records, extract_tool_path, extract_tool_paths,
        should_include_turn_in_active_time,
    },
};
use darc_paths::SourceKind;
use darc_rollout::model::NormalizedTurnStep;
use darc_test_utils::{
    IndexedSessionFixture, IndexedTurnFixture, insert_indexed_session, insert_indexed_turn,
    seed_legacy_codex_index, unique_test_dir,
};

use crate::query::{
    FilesQueryRequest, HardDebuggingTurn, LocalDate, ProjectInsights, SearchMode,
    SearchTurnsRequest, SessionKind, TurnDetailOptions, TurnInsights, TurnMatchKind,
    TurnMatchesQueryRequest, TurnSearchRole, build_project_insights, build_turn_insights,
    build_workspace_insights, open_existing_index_database, parse_session_kind,
    query_project_files, query_project_session_files, query_project_sessions,
    query_project_turn_matches, query_search_turns, query_session_turn_details, query_turn_detail,
    smoke_test_sql,
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
fn prepares_all_query_sql_against_current_schema() -> Result<()> {
    let index_path = test_index_path("query-sql-smoke-current");
    let connection = open_index_database(&index_path)?;

    smoke_test_sql(&connection)?;

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn prepares_all_query_sql_after_legacy_codex_migration() -> Result<()> {
    let index_path = test_index_path("query-sql-smoke-legacy");
    fs::create_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    let connection = rusqlite::Connection::open(&index_path)?;
    seed_legacy_codex_index(&connection)?;
    drop(connection);

    let migrated = open_index_database(&index_path)?;
    smoke_test_sql(&migrated)?;

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
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
fn extracts_shell_commands_from_shell_like_tool_arguments() {
    let exec_command = extract_shell_command(
        "exec_command",
        r#"{"cmd":"rg -n \"tool_calls\" src -S","workdir":"/tmp/repo"}"#,
    )
    .expect("exec_command payload should parse");
    assert_eq!(exec_command.command_text, r#"rg -n "tool_calls" src -S"#);
    assert_eq!(exec_command.workdir.as_deref(), Some("/tmp/repo"));

    let shell_command = extract_shell_command(
        "shell",
        r#"{"command":["bash","-lc","cp src/main.rs src/main.rs.bak && ls src"],"workdir":"/tmp/repo"}"#,
    )
    .expect("shell payload should parse");
    assert_eq!(
        shell_command.command_text,
        "cp src/main.rs src/main.rs.bak && ls src"
    );
    assert_eq!(shell_command.workdir.as_deref(), Some("/tmp/repo"));

    assert!(extract_shell_command("Read", r#"{"file_path":"README.md"}"#).is_none());
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
fn derives_file_accesses_from_shell_heredoc_apply_patch() {
    let steps = vec![NormalizedTurnStep::ToolCall {
        timestamp: "2026-04-06T10:00:01Z".to_owned(),
        call_id: "call-1".to_owned(),
        name: "exec_command".to_owned(),
        arguments: r#"{"cmd":"apply_patch <<'PATCH'\n*** Begin Patch\n*** Update File: src/main.rs\n@@\n-old\n+new\n*** Add File: src/new.rs\n+fn main() {}\n*** End Patch\nPATCH","workdir":"/tmp/repo"}"#
            .to_owned(),
    }];

    let tool_calls = extract_tool_call_records("repo-a", SourceKind::Codex, "session-1", 0, &steps);
    let file_accesses = derive_file_access_records(&tool_calls);

    assert!(file_accesses.iter().any(|record| {
        record.path == "src/main.rs" && matches!(record.access_type, ToolAccessKind::Edit)
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
fn session_summaries_leave_partial_token_and_runtime_totals_null() -> Result<()> {
    let index_path = test_index_path("session-partial-totals");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            duration_ms: 1_000,
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
            duration_ms: 2_000,
            effective_agent_runtime_ms: Some(2_000),
            total_token_count: Some(321),
            input_uncached_token_count: Some(120),
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-05T12:05:00Z",
                "completed",
                "[]",
            )
        },
    )?;

    let sessions = query_project_sessions(&index_path, "repo-a", None, None, None, None)?;

    assert_eq!(sessions.sessions.len(), 1);
    assert_eq!(sessions.sessions[0].total_token_count, None);
    assert_eq!(sessions.sessions[0].token_usage, None);
    assert_eq!(sessions.sessions[0].effective_agent_runtime_ms, None);

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn session_summaries_filter_by_latest_turn_bounds() -> Result<()> {
    let index_path = test_index_path("session-time-bounds");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-early", "/tmp/repo-a"),
    )?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-late", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture::new(
            "repo-a",
            SourceKind::Codex,
            "session-early",
            0,
            "2026-04-05T10:00:00Z",
            "completed",
            "[]",
        ),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture::new(
            "repo-a",
            SourceKind::Codex,
            "session-late",
            0,
            "2026-04-06T10:00:00Z",
            "completed",
            "[]",
        ),
    )?;

    let all_sessions = query_project_sessions(&index_path, "repo-a", None, None, None, None)?;
    let since_sessions = query_project_sessions(
        &index_path,
        "repo-a",
        None,
        Some("2026-04-06T00:00:00Z"),
        None,
        None,
    )?;
    let until_sessions = query_project_sessions(
        &index_path,
        "repo-a",
        None,
        None,
        Some("2026-04-06T00:00:00Z"),
        None,
    )?;
    let bounded_sessions = query_project_sessions(
        &index_path,
        "repo-a",
        None,
        Some("2026-04-05T12:00:00Z"),
        Some("2026-04-06T12:00:00Z"),
        None,
    )?;

    assert_eq!(all_sessions.sessions.len(), 2);
    assert_eq!(
        since_sessions
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["session-late"]
    );
    assert_eq!(
        until_sessions
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["session-early"]
    );
    assert_eq!(
        bounded_sessions
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["session-late"]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn session_summaries_filter_by_touched_path_glob() -> Result<()> {
    let index_path = test_index_path("session-touched-path");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-wiki", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-wiki",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"path\":\"/tmp/repo-a/crates/wiki/src/proposal.rs\"}"}]"##,
            )
        },
    )?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-docs", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-docs",
                0,
                "2026-04-06T11:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T11:00:01Z","call_id":"call-2","name":"Read","arguments":"{\"file_path\":\"docs/query-protocol.md\"}"}]"##,
            )
        },
    )?;

    let sessions = query_project_sessions(
        &index_path,
        "repo-a",
        Some(Path::new("/tmp/repo-a")),
        None,
        None,
        Some("crates/wiki/**"),
    )?;

    assert_eq!(
        sessions
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["session-wiki"]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn session_summaries_accept_absolute_project_root_touched_paths() -> Result<()> {
    let index_path = test_index_path("session-touched-path-absolute");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"}]"##,
            )
        },
    )?;

    let sessions = query_project_sessions(
        &index_path,
        "repo-a",
        Some(Path::new("/tmp/repo-a")),
        None,
        None,
        Some("/tmp/repo-a/README.md"),
    )?;

    assert_eq!(
        sessions
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["session-1"]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_files_path_mode_ranks_sessions_and_respects_time_bounds() -> Result<()> {
    let index_path = test_index_path("query-files-path");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"crates/wiki/src/proposal.rs\"}"}]"##,
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-06T10:05:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:05:01Z","call_id":"call-2","name":"Edit","arguments":"{\"path\":\"/tmp/repo-a/crates/wiki/src/context.rs\"}"}]"##,
            )
        },
    )?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Claude, "session-2", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Claude,
                "session-2",
                0,
                "2026-04-06T09:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T09:00:01Z","call_id":"call-3","name":"Read","arguments":"{\"file_path\":\"crates/wiki/src/proposal.rs\"}"}]"##,
            )
        },
    )?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-old", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-old",
                0,
                "2026-04-04T10:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-04T10:00:01Z","call_id":"call-4","name":"Read","arguments":"{\"file_path\":\"crates/wiki/src/proposal.rs\"}"}]"##,
            )
        },
    )?;

    let exact = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            path: Some("./crates/wiki/src/proposal.rs"),
            co_touched_with: None,
            since: None,
            until: None,
            limit: None,
        },
    )?;
    let glob = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            path: Some("/tmp/repo-a/crates/wiki/**/*.rs"),
            co_touched_with: None,
            since: Some("2026-04-05T00:00:00Z"),
            until: Some("2026-04-07T00:00:00Z"),
            limit: None,
        },
    )?;

    assert_eq!(
        exact
            .sessions
            .iter()
            .map(|session| (
                session.provider,
                session.session_id.as_str(),
                session.touch_count
            ))
            .collect::<Vec<_>>(),
        vec![
            (SourceKind::Codex, "session-1", 1),
            (SourceKind::Claude, "session-2", 1),
            (SourceKind::Codex, "session-old", 1),
        ]
    );
    assert_eq!(
        glob.sessions
            .iter()
            .map(|session| (
                session.provider,
                session.session_id.as_str(),
                session.touch_count
            ))
            .collect::<Vec<_>>(),
        vec![
            (SourceKind::Codex, "session-1", 2),
            (SourceKind::Claude, "session-2", 1),
        ]
    );
    assert_eq!(
        glob.sessions[0].matched_paths,
        vec![
            "crates/wiki/src/context.rs".to_owned(),
            "crates/wiki/src/proposal.rs".to_owned()
        ]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_files_co_touched_mode_counts_sessions_and_sorts_ties() -> Result<()> {
    let index_path = test_index_path("query-files-co-touch");
    let connection = open_index_database(&index_path)?;
    for (provider, session_id, started_at, steps_json) in [
        (
            SourceKind::Codex,
            "session-1",
            "2026-04-06T10:00:00Z",
            r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file\":[\"crates/wiki/src/proposal.rs\",\"crates/wiki/src/context.rs\",\"crates/wiki/src/api.rs\"]}"}]"##,
        ),
        (
            SourceKind::Claude,
            "session-2",
            "2026-04-06T11:00:00Z",
            r##"[{"type":"tool_call","timestamp":"2026-04-06T11:00:01Z","call_id":"call-2","name":"Read","arguments":"{\"file\":[\"crates/wiki/src/proposal.rs\",\"crates/wiki/src/context.rs\"]}"}]"##,
        ),
        (
            SourceKind::Codex,
            "session-3",
            "2026-04-06T12:00:00Z",
            r##"[{"type":"tool_call","timestamp":"2026-04-06T12:00:01Z","call_id":"call-3","name":"Read","arguments":"{\"file\":[\"crates/wiki/src/proposal.rs\",\"crates/wiki/src/alpha.rs\"]}"}]"##,
        ),
    ] {
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new("repo-a", provider, session_id, "/tmp/repo-a"),
        )?;
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture {
                step_count: 1,
                tool_call_count: 1,
                duration_ms: 3_000,
                ..IndexedTurnFixture::new(
                    "repo-a",
                    provider,
                    session_id,
                    0,
                    started_at,
                    "completed",
                    steps_json,
                )
            },
        )?;
    }

    let result = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            path: None,
            co_touched_with: Some("/tmp/repo-a/crates/wiki/src/proposal.rs"),
            since: None,
            until: None,
            limit: Some(10),
        },
    )?;

    assert_eq!(
        result
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.co_touch_count))
            .collect::<Vec<_>>(),
        vec![
            ("crates/wiki/src/context.rs", 2),
            ("crates/wiki/src/alpha.rs", 1),
            ("crates/wiki/src/api.rs", 1),
        ]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_session_files_collapses_absolute_and_relative_paths() -> Result<()> {
    let index_path = test_index_path("query-session-files");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"crates/wiki/src/proposal.rs\"}"}]"##,
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                2,
                "2026-04-06T10:05:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:05:01Z","call_id":"call-2","name":"Edit","arguments":"{\"path\":\"/tmp/repo-a/crates/wiki/src/proposal.rs\"}"}]"##,
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                3,
                "2026-04-06T10:06:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:06:01Z","call_id":"call-3","name":"Read","arguments":"{\"file_path\":\"crates/wiki/src/context.rs\"}"}]"##,
            )
        },
    )?;

    let result = query_project_session_files(
        &index_path,
        "repo-a",
        SourceKind::Codex,
        "session-1",
        Some(Path::new("/tmp/repo-a")),
    )?;

    assert_eq!(
        result
            .files
            .iter()
            .map(|file| {
                (
                    file.path.as_str(),
                    file.read_count,
                    file.write_count,
                    file.first_turn_ordinal,
                    file.last_turn_ordinal,
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("crates/wiki/src/proposal.rs", 1, 1, 0, 2),
            ("crates/wiki/src/context.rs", 1, 0, 3, 3),
        ]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_session_files_normalize_dot_relative_paths() -> Result<()> {
    let index_path = test_index_path("query-session-files-dot");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"./crates/wiki/src/proposal.rs\"}"}]"##,
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-06T10:01:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:01:01Z","call_id":"call-2","name":"Edit","arguments":"{\"file_path\":\"crates/wiki/src/proposal.rs\"}"}]"##,
            )
        },
    )?;

    let result = query_project_session_files(
        &index_path,
        "repo-a",
        SourceKind::Codex,
        "session-1",
        Some(Path::new("/tmp/repo-a")),
    )?;

    assert_eq!(
        result
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.read_count, file.write_count))
            .collect::<Vec<_>>(),
        vec![("crates/wiki/src/proposal.rs", 1, 1)]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_session_files_exclude_out_of_project_and_list_only_paths() -> Result<()> {
    let index_path = test_index_path("query-session-files-scope");
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
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"path\":\"/tmp/secret.txt\"}"},{"type":"tool_call","timestamp":"2026-04-06T10:00:02Z","call_id":"call-2","name":"ListFiles","arguments":"{\"path\":\"crates/wiki/src\"}"}]"##,
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-06T10:01:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:01:01Z","call_id":"call-3","name":"Read","arguments":"{\"file_path\":\"README.md\"}"}]"##,
            )
        },
    )?;

    let session_files = query_project_session_files(
        &index_path,
        "repo-a",
        SourceKind::Codex,
        "session-1",
        Some(Path::new("/tmp/repo-a")),
    )?;
    let co_touched = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            path: None,
            co_touched_with: Some("README.md"),
            since: None,
            until: None,
            limit: Some(10),
        },
    )?;

    assert_eq!(
        session_files
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["README.md"]
    );
    assert!(co_touched.files.is_empty());

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn turn_detail_narrative_view_strips_bulky_step_fields() -> Result<()> {
    let index_path = test_index_path("turn-detail-narrative");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 8,
            tool_call_count: 1,
            tool_output_count: 1,
            attachment_count: 1,
            delegation_count: 1,
            hook_summary_count: 1,
            has_final_answer: true,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                r##"[{"type":"reasoning","timestamp":"2026-04-06T10:00:01Z","summary":["inspect"],"encrypted":true},{"type":"commentary","timestamp":"2026-04-06T10:00:02Z","text":"Checking files."},{"type":"tool_call","timestamp":"2026-04-06T10:00:03Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"},{"type":"tool_call_output","timestamp":"2026-04-06T10:00:04Z","call_id":"call-1","output":"# Repo"},{"type":"attachment","timestamp":"2026-04-06T10:00:05Z","attachment_type":"deferred_tools_delta","payload_json":"{\"added\":[\"Read\"]}"},{"type":"delegation","timestamp":"2026-04-06T10:00:06Z","call_id":"call-2","task_id":"task-1","event":"completed","agent_id":"agent-1","agent_type":"general-purpose","status":"completed","summary":"done","payload_json":"{\"totalDurationMs\":12}"},{"type":"hook_summary","timestamp":"2026-04-06T10:00:07Z","call_id":"call-3","hook_count":2,"prevented_continuation":false,"has_output":true,"level":"suggestion","payload_json":"{\"command\":\"callback\"}"},{"type":"provider_response_item","timestamp":"2026-04-06T10:00:08Z","item_type":"web_search_call","payload_json":"{\"status\":\"completed\"}"}]"##,
            )
        },
    )?;

    let detail = query_turn_detail(
        &index_path,
        "repo-a",
        SourceKind::Codex,
        "session-1",
        0,
        TurnDetailOptions {
            include_raw: true,
            include_insights: false,
            narrative: true,
        },
    )?;

    assert_eq!(detail.steps.len(), 8);
    assert_eq!(detail.raw_steps_json, None);
    assert!(matches!(
        &detail.steps[0],
        NormalizedTurnStep::Reasoning {
            summary,
            encrypted,
            ..
        } if summary == &vec!["inspect".to_owned()] && *encrypted
    ));
    assert!(matches!(
        &detail.steps[1],
        NormalizedTurnStep::Commentary { text, .. } if text == "Checking files."
    ));
    assert!(matches!(
        &detail.steps[2],
        NormalizedTurnStep::ToolCall { arguments, .. } if arguments.is_empty()
    ));
    assert!(matches!(
        &detail.steps[3],
        NormalizedTurnStep::ToolCallOutput { output, .. } if output.is_empty()
    ));
    assert!(matches!(
        &detail.steps[4],
        NormalizedTurnStep::Attachment { payload_json, .. } if payload_json.is_empty()
    ));
    assert!(matches!(
        &detail.steps[5],
        NormalizedTurnStep::Delegation {
            payload_json,
            summary,
            ..
        } if payload_json.is_empty() && summary.as_deref() == Some("done")
    ));
    assert!(matches!(
        &detail.steps[6],
        NormalizedTurnStep::HookSummary {
            payload_json,
            hook_count,
            ..
        } if payload_json.is_empty() && *hook_count == 2
    ));
    assert!(matches!(
        &detail.steps[7],
        NormalizedTurnStep::ProviderResponseItem {
            payload_json,
            item_type,
            ..
        } if payload_json.is_empty() && item_type == "web_search_call"
    ));

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn session_turn_details_reuse_one_session_query_shape() -> Result<()> {
    let index_path = test_index_path("session-turn-details");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture::new(
            "repo-a",
            SourceKind::Codex,
            "session-1",
            0,
            "2026-04-06T10:00:00Z",
            "completed",
            r#"[{"type":"commentary","timestamp":"2026-04-06T10:00:01Z","text":"First"}]"#,
        ),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture::new(
            "repo-a",
            SourceKind::Codex,
            "session-1",
            1,
            "2026-04-06T10:05:00Z",
            "completed",
            r#"[{"type":"tool_call","timestamp":"2026-04-06T10:05:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"}]"#,
        ),
    )?;

    let details = query_session_turn_details(
        &index_path,
        "repo-a",
        SourceKind::Codex,
        "session-1",
        TurnDetailOptions {
            include_raw: false,
            include_insights: false,
            narrative: true,
        },
    )?;

    assert_eq!(details.len(), 2);
    assert_eq!(details[0].turn_ordinal, 0);
    assert_eq!(details[1].turn_ordinal, 1);
    assert!(matches!(
        &details[1].steps[0],
        NormalizedTurnStep::ToolCall { arguments, .. } if arguments.is_empty()
    ));
    assert!(details.iter().all(|detail| detail.insights.is_none()));

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
    assert!(insights.most_read_files.iter().any(|stat| {
        stat.path == "README.md"
            && stat.repo_relative_path.as_deref() == Some("README.md")
            && stat.read_count == 1
    }));
    assert!(insights.most_written_files.iter().any(|stat| {
        stat.path == "src/main.rs"
            && stat.repo_relative_path.as_deref() == Some("src/main.rs")
            && stat.write_count == 1
    }));
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
            provider_total_token_count: Some(300),
            input_uncached_token_count: Some(120),
            cache_read_token_count: Some(80),
            output_token_count: Some(121),
            reasoning_token_count: Some(20),
            total_token_count: Some(321),
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
    assert_eq!(insights.total_token_count, Some(321));
    assert_eq!(
        insights
            .token_usage
            .and_then(|usage| usage.input_uncached_token_count),
        Some(120)
    );
    assert_eq!(
        insights
            .token_usage
            .and_then(|usage| usage.cache_read_token_count),
        Some(80)
    );
    assert_eq!(
        insights
            .token_usage
            .and_then(|usage| usage.cache_write_token_count),
        None
    );
    assert_eq!(
        insights
            .token_usage
            .and_then(|usage| usage.output_token_count),
        Some(121)
    );
    assert_eq!(
        insights
            .token_usage
            .and_then(|usage| usage.reasoning_token_count),
        Some(20)
    );
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
            .map(|stat| {
                (
                    stat.path.as_str(),
                    stat.repo_relative_path.as_deref(),
                    stat.read_count,
                    stat.write_count,
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("src/main.rs", Some("src/main.rs"), 0, 2),
            ("README.md", Some("README.md"), 2, 0),
        ]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn turn_insights_preserve_null_repo_relative_path_for_absolute_paths() -> Result<()> {
    let index_path = test_index_path("turn-insights-absolute-path");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture::new(
            "repo-a",
            SourceKind::Codex,
            "session-1",
            0,
            "2026-04-06T11:10:00Z",
            "completed",
            r#"[{"type":"tool_call","timestamp":"2026-04-06T11:10:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"/tmp/repo-a/README.md\"}"}]"#,
        ),
    )?;

    let insights: TurnInsights =
        build_turn_insights(&connection, "repo-a", SourceKind::Codex, "session-1", 0)?;

    assert_eq!(insights.files.len(), 1);
    assert_eq!(insights.files[0].path, "/tmp/repo-a/README.md");
    assert_eq!(insights.files[0].repo_relative_path, None);
    assert_eq!(insights.files[0].read_count, 1);
    assert_eq!(insights.files[0].write_count, 0);

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn turn_insights_collect_shell_commands() -> Result<()> {
    let index_path = test_index_path("turn-insights-shell-commands");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 3,
            tool_call_count: 3,
            tool_output_count: 0,
            attachment_count: 0,
            delegation_count: 0,
            hook_summary_count: 0,
            has_final_answer: true,
            duration_ms: 8_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T11:15:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T11:15:01Z","call_id":"call-1","name":"exec_command","arguments":"{\"cmd\":\"rg -n \\\"query_turn_insights\\\" crates/query/src/query.rs -S\",\"workdir\":\"/tmp/repo\"}"},{"type":"tool_call","timestamp":"2026-04-06T11:15:02Z","call_id":"call-2","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"cp src/main.rs src/main.rs.bak && ls src\"],\"workdir\":\"/tmp/repo\"}"},{"type":"tool_call","timestamp":"2026-04-06T11:15:03Z","call_id":"call-3","name":"Read","arguments":"{\"file_path\":\"README.md\"}"}]"##,
            )
        },
    )?;

    let insights = build_turn_insights(&connection, "repo-a", SourceKind::Codex, "session-1", 0)?;

    assert_eq!(
        insights
            .shell_commands
            .iter()
            .map(|command| {
                (
                    command.tool_name.as_str(),
                    command.command_text.as_str(),
                    command.workdir.as_deref(),
                )
            })
            .collect::<Vec<_>>(),
        vec![
            (
                "exec_command",
                r#"rg -n "query_turn_insights" crates/query/src/query.rs -S"#,
                Some("/tmp/repo"),
            ),
            (
                "shell",
                "cp src/main.rs src/main.rs.bak && ls src",
                Some("/tmp/repo"),
            ),
        ]
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
    assert!(insights.shell_commands.is_empty());
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

#[test]
fn search_turns_keyword_matches_indexed_turn_text() -> Result<()> {
    let index_path = test_index_path("search-keyword");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Inspect the repository heading",
            step_count: 2,
            tool_call_count: 1,
            tool_output_count: 1,
            duration_ms: 5_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"},{"type":"tool_call_output","timestamp":"2026-04-06T10:00:02Z","call_id":"call-1","output":"# Repo Heading"}]"##,
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Check the hidden tool output",
            step_count: 2,
            tool_call_count: 1,
            tool_output_count: 1,
            duration_ms: 5_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-06T10:05:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:05:01Z","call_id":"call-2","name":"Read","arguments":"{\"file_path\":\"secret.txt\"}"},{"type":"tool_call_output","timestamp":"2026-04-06T10:05:02Z","call_id":"call-2","output":"SECRET_TOKEN=top-secret"}]"##,
            )
        },
    )?;

    let result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            mode: SearchMode::Keyword,
            query: "Inspect",
            provider: None,
            session_id: None,
            limit: 10,
            offset: 0,
        },
    )?;
    let secret_result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            mode: SearchMode::Keyword,
            query: "SECRET_TOKEN",
            provider: None,
            session_id: None,
            limit: 10,
            offset: 0,
        },
    )?;

    assert_eq!(result.mode, SearchMode::Keyword);
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].session_id, "session-1");
    assert!(
        result.hits[0]
            .snippet
            .as_deref()
            .is_some_and(|snippet| snippet.contains("Inspect"))
    );
    assert!(secret_result.hits.is_empty());

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn search_turns_file_modes_match_derived_paths() -> Result<()> {
    let index_path = test_index_path("search-file");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Inspect the main source file",
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T11:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T11:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"src/main,old.rs\"}"}]"##,
            )
        },
    )?;

    let file_name_result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            mode: SearchMode::FileName,
            query: "main,old.rs",
            provider: None,
            session_id: None,
            limit: 10,
            offset: 0,
        },
    )?;
    let file_path_result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            mode: SearchMode::FilePath,
            query: "src/main,old.rs",
            provider: None,
            session_id: None,
            limit: 10,
            offset: 0,
        },
    )?;

    assert_eq!(file_name_result.hits.len(), 1);
    assert_eq!(file_path_result.hits.len(), 1);
    assert_eq!(
        file_name_result.hits[0].matched_paths,
        vec!["src/main,old.rs"]
    );
    assert_eq!(
        file_path_result.hits[0].matched_paths,
        vec!["src/main,old.rs"]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_turn_matches_user_role_scopes_phrase_to_user_text_only() -> Result<()> {
    let index_path = test_index_path("turn-matches-role-user");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "staged only",
            final_answer_text: Some("init only"),
            step_count: 1,
            has_final_answer: true,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                "[]",
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "please use staged init here",
            step_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-06T10:01:00Z",
                "completed",
                "[]",
            )
        },
    )?;

    let result = query_project_turn_matches(
        &index_path,
        TurnMatchesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            session_id: None,
            grep: "staged init",
            role: TurnSearchRole::User,
            context: 0,
            since: None,
            until: None,
            touched_path: None,
        },
    )?;

    assert_eq!(result.turns.len(), 1);
    assert_eq!(result.turns[0].turn_ordinal, 1);
    assert_eq!(result.turns[0].match_kind, Some(TurnMatchKind::Match));
    assert!(
        result.turns[0]
            .match_snippet
            .as_deref()
            .is_some_and(|snippet| snippet.contains("staged init"))
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_turn_matches_require_contiguous_phrase_order() -> Result<()> {
    let index_path = test_index_path("turn-matches-phrase");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    for (turn_ordinal, user_message) in [
        (0, "staged init"),
        (1, "staged later init"),
        (2, "init staged"),
    ] {
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture {
                user_message,
                step_count: 1,
                duration_ms: 3_000,
                ..IndexedTurnFixture::new(
                    "repo-a",
                    SourceKind::Codex,
                    "session-1",
                    turn_ordinal,
                    "2026-04-06T10:00:00Z",
                    "completed",
                    "[]",
                )
            },
        )?;
    }

    let result = query_project_turn_matches(
        &index_path,
        TurnMatchesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            session_id: None,
            grep: "staged init",
            role: TurnSearchRole::User,
            context: 0,
            since: None,
            until: None,
            touched_path: None,
        },
    )?;

    assert_eq!(
        result
            .turns
            .iter()
            .map(|turn| turn.turn_ordinal)
            .collect::<Vec<_>>(),
        vec![0]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_turn_matches_support_context_and_absolute_touched_paths() -> Result<()> {
    let index_path = test_index_path("turn-matches-context-path");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Review the surrounding context",
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T09:59:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T09:59:01Z","call_id":"call-0","name":"Read","arguments":"{\"file_path\":\"README.md\"}"}]"##,
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Please switch to staged init for the index setup",
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-06T10:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"path\":\"/tmp/repo-a/crates/index/src/index_db/schema.rs\"}"}]"##,
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Apply the follow-up change after that",
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                2,
                "2026-04-06T10:01:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:01:01Z","call_id":"call-2","name":"Read","arguments":"{\"file_path\":\"Cargo.toml\"}"}]"##,
            )
        },
    )?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-2", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Please switch to staged init for the docs too",
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-2",
                0,
                "2026-04-06T11:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T11:00:01Z","call_id":"call-3","name":"Read","arguments":"{\"file_path\":\"docs/query-protocol.md\"}"}]"##,
            )
        },
    )?;

    let result = query_project_turn_matches(
        &index_path,
        TurnMatchesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            session_id: None,
            grep: "staged init",
            role: TurnSearchRole::User,
            context: 1,
            since: Some("2026-04-06T00:00:00Z"),
            until: Some("2026-04-07T00:00:00Z"),
            touched_path: Some("/tmp/repo-a/crates/index/**"),
        },
    )?;

    assert_eq!(result.turns.len(), 3);
    assert_eq!(
        result
            .turns
            .iter()
            .map(|turn| turn.turn_ordinal)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(
        result
            .turns
            .iter()
            .map(|turn| turn.match_kind)
            .collect::<Vec<_>>(),
        vec![
            Some(TurnMatchKind::Context),
            Some(TurnMatchKind::Match),
            Some(TurnMatchKind::Context),
        ]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_turn_matches_assistant_role_only_matches_assistant_text() -> Result<()> {
    let index_path = test_index_path("turn-matches-assistant");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Review the migration options",
            final_answer_text: Some("Use staged init for the database bootstrap."),
            step_count: 1,
            has_final_answer: true,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                "[]",
            )
        },
    )?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-2", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Look at the migration helper",
            step_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-2",
                0,
                "2026-04-06T11:00:00Z",
                "completed",
                r##"[{"type":"commentary","timestamp":"2026-04-06T11:00:01Z","text":"Switching to staged init before the write step."}]"##,
            )
        },
    )?;

    let assistant_result = query_project_turn_matches(
        &index_path,
        TurnMatchesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            session_id: None,
            grep: "staged init",
            role: TurnSearchRole::Assistant,
            context: 0,
            since: None,
            until: None,
            touched_path: None,
        },
    )?;
    let user_result = query_project_turn_matches(
        &index_path,
        TurnMatchesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            session_id: None,
            grep: "staged init",
            role: TurnSearchRole::User,
            context: 0,
            since: None,
            until: None,
            touched_path: None,
        },
    )?;

    assert_eq!(assistant_result.turns.len(), 2);
    assert!(
        assistant_result
            .turns
            .iter()
            .all(|turn| turn.match_kind == Some(TurnMatchKind::Match))
    );
    assert!(assistant_result.turns.iter().all(|turn| {
        turn.match_snippet
            .as_deref()
            .is_some_and(|snippet| snippet.contains("staged init"))
    }));
    assert!(user_result.turns.is_empty());

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_turn_matches_reject_context_over_limit() {
    let index_path = test_index_path("turn-matches-context-limit");
    let _connection = open_index_database(&index_path).expect("index database should open");
    let error = query_project_turn_matches(
        &index_path,
        TurnMatchesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            session_id: None,
            grep: "staged init",
            role: TurnSearchRole::Both,
            context: 51,
            since: None,
            until: None,
            touched_path: None,
        },
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("--context must be at most 50 turns for grep mode")
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )
    .expect("temporary index directory should be removed");
}
