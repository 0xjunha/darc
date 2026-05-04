use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use darc_index::{
    evidence::EvidenceField,
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
use serde_json::to_value;

use crate::query::{
    DEFAULT_MATCHED_PATH_LIMIT, DEFAULT_SEARCH_MATCH_LIMIT, DEFAULT_TURN_STEP_LIMIT,
    DEFAULT_WORKSPACE_RECENT_SESSION_LIMIT, FilesQueryMode, FilesQueryRequest, LocalDate,
    ProjectInsights, SearchMode, SearchTurnsRequest, SessionBundleQueryRequest, SessionBundleView,
    SessionFilesQueryRequest, SessionKind, SessionsQueryRequest, SessionsView, TurnDetailOptions,
    TurnInsights, TurnsQueryRequest, TurnsView, build_project_insights, build_turn_insights,
    build_workspace_insights, open_existing_index_database, parse_session_kind,
    query_project_files, query_project_session_bundle, query_project_session_files,
    query_project_sessions, query_project_turns, query_search_turns, query_session_turn_details,
    query_turn_detail, query_turn_exists, smoke_test_sql,
};

/// Builds one temporary SQLite index path for query tests.
fn test_index_path(prefix: &str) -> PathBuf {
    unique_test_dir(prefix).join("index.sqlite")
}

/// Stores synthetic evidence-row bulk insert inputs for query tests.
struct SyntheticEvidenceRows<'a> {
    project_id: &'a str,
    provider: SourceKind,
    session_id: &'a str,
    turn_ordinal: i64,
    first_evidence_ordinal: usize,
    row_count: usize,
    field: &'a str,
    text: &'a str,
}

/// Inserts many synthetic evidence rows for one indexed turn inside one transaction.
fn insert_turn_evidence_rows(
    connection: &mut rusqlite::Connection,
    fixture: SyntheticEvidenceRows<'_>,
) -> Result<()> {
    let evidence_ordinal_end = fixture
        .first_evidence_ordinal
        .checked_add(fixture.row_count)
        .context("test evidence ordinal range should fit in usize")?;
    let transaction = connection.transaction()?;
    {
        let mut statement = transaction.prepare(
            "
            INSERT INTO turn_evidence (
                project_id,
                provider,
                session_id,
                turn_ordinal,
                evidence_ordinal,
                field,
                text
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ",
        )?;
        for evidence_ordinal in fixture.first_evidence_ordinal..evidence_ordinal_end {
            statement.execute(rusqlite::params![
                fixture.project_id,
                fixture.provider.directory_name(),
                fixture.session_id,
                fixture.turn_ordinal,
                i64::try_from(evidence_ordinal)
                    .context("test evidence ordinal should fit in SQLite INTEGER")?,
                fixture.field,
                fixture.text,
            ])?;
        }
    }
    transaction.commit()?;
    Ok(())
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
    assert!(!file_accesses.iter().any(|record| {
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
fn derives_file_accesses_skip_fd_redirections() {
    let steps = vec![NormalizedTurnStep::ToolCall {
        timestamp: "2026-04-06T10:00:01Z".to_owned(),
        call_id: "call-1".to_owned(),
        name: "Bash".to_owned(),
        arguments: r#"{"command":"ls README.md 2>&1 && cargo +nightly fmt -- crates/core/src/sync.rs 2>&1 && ls README.md 2>/dev/null && grep foo src/lib.rs 2> errors.log && cargo test > target/test.log 2>&1 && cat < input.txt > output.txt","description":"exercise redirections"}"#.to_owned(),
    }];

    let tool_calls = extract_tool_call_records("repo-a", SourceKind::Codex, "session-1", 0, &steps);
    let file_accesses = derive_file_access_records(&tool_calls);

    assert!(!file_accesses.iter().any(|record| {
        matches!(
            record.path.as_str(),
            "2>&1" | "2>/dev/null" | "&1" | "/dev/null"
        )
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "README.md" && matches!(record.access_type, ToolAccessKind::List)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "crates/core/src/sync.rs"
            && matches!(record.access_type, ToolAccessKind::Edit)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "src/lib.rs" && matches!(record.access_type, ToolAccessKind::Read)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "errors.log" && matches!(record.access_type, ToolAccessKind::Write)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "target/test.log" && matches!(record.access_type, ToolAccessKind::Write)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "input.txt" && matches!(record.access_type, ToolAccessKind::Read)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "output.txt" && matches!(record.access_type, ToolAccessKind::Write)
    }));
}

#[test]
fn derives_file_accesses_skip_shell_metadata_and_dynamic_paths() {
    let steps = vec![NormalizedTurnStep::ToolCall {
        timestamp: "2026-04-06T10:00:01Z".to_owned(),
        call_id: "call-1".to_owned(),
        name: "exec_command".to_owned(),
        arguments: r#"{"cmd":"chmod +x scripts/run.sh scripts/check.sh && chmod 755 scripts/install.sh && chmod --reference scripts/run.sh scripts/ref-mode-target.sh && chown junha:staff scripts/run.sh && chown --reference scripts/run.sh scripts/ref-owner-target.sh && test \"$actual\" = \"$expected\" && [ -x scripts/run.sh ] && cat > $tmp/Cargo.toml && touch -t 202604011200 docs/release.md && lsof -p 597 && awk -v iter=\"$i\" '/real/ { print iter, $2 }' benches/out.log && jq --arg key \"$key\" '.[$key]' data.json && jq --rawfile fixture fixtures/raw.txt --from-file filters/release.jq data.json","workdir":"/tmp/repo"}"#.to_owned(),
    }];

    let tool_calls = extract_tool_call_records("repo-a", SourceKind::Codex, "session-1", 0, &steps);
    let file_accesses = derive_file_access_records(&tool_calls);
    let paths = file_accesses
        .iter()
        .map(|record| record.path.as_str())
        .collect::<BTreeSet<_>>();

    for pseudo_path in [
        "+x",
        "755",
        "junha:staff",
        "=",
        "$expected",
        "$tmp/Cargo.toml",
        "202604011200",
        "597",
        "/real/ { print iter, $2 }",
    ] {
        assert!(
            !paths.contains(pseudo_path),
            "unexpected path {pseudo_path}"
        );
    }
    assert!(paths.contains("scripts/run.sh"));
    assert!(paths.contains("scripts/check.sh"));
    assert!(paths.contains("scripts/install.sh"));
    assert!(paths.contains("scripts/ref-mode-target.sh"));
    assert!(paths.contains("scripts/ref-owner-target.sh"));
    assert!(paths.contains("docs/release.md"));
    assert!(paths.contains("benches/out.log"));
    assert!(paths.contains("fixtures/raw.txt"));
    assert!(paths.contains("filters/release.jq"));
    assert!(paths.contains("data.json"));
}

#[test]
fn derives_file_accesses_skip_heredoc_bodies_and_process_substitutions() {
    let steps = vec![NormalizedTurnStep::ToolCall {
        timestamp: "2026-04-06T10:00:01Z".to_owned(),
        call_id: "call-1".to_owned(),
        name: "exec_command".to_owned(),
        arguments: r#"{"cmd":"cat <<'EOF' > README.md\ncargo fmt -- --check\nRUST_LOG=debug cargo nextest run <test_name>\nEOF\ncmp -s <(target/debug/darc list --color never projects | jq 'del(.generated_at)') <(target/debug/darc show --color never workspace | jq 'del(.generated_at)')\ntarget/debug/darc list files --session $(target/debug/darc list --color never sessions --limit 1 | jq -r '.data.sessions[0].session_id') --color never 2>&1 | sed -n '1,80p'\ncargo +nightly fmt -- --check\nstat -f '%Sm %N' -t '%Y-%m-%d' Cargo.toml\nrustfmt --config imports_granularity=Crate --print-config current /dev/null 2>&1\nxxd -l 32 traces/input.bin","workdir":"/tmp/repo"}"#.to_owned(),
    }];

    let tool_calls = extract_tool_call_records("repo-a", SourceKind::Codex, "session-1", 0, &steps);
    let file_accesses = derive_file_access_records(&tool_calls);
    let paths = file_accesses
        .iter()
        .map(|record| record.path.as_str())
        .collect::<BTreeSet<_>>();

    for pseudo_path in [
        "--check",
        "never",
        "workspace",
        "%Sm %N",
        "%Y-%m-%d",
        "current",
        "32",
        "test_name>",
    ] {
        assert!(
            !paths.contains(pseudo_path),
            "unexpected path {pseudo_path}"
        );
    }
    assert!(paths.contains("README.md"));
    assert!(paths.contains("Cargo.toml"));
    assert!(paths.contains("traces/input.bin"));
}

#[test]
fn derives_file_accesses_preserve_quoted_search_patterns() {
    let steps = vec![NormalizedTurnStep::ToolCall {
        timestamp: "2026-04-06T10:00:01Z".to_owned(),
        call_id: "call-1".to_owned(),
        name: "Bash".to_owned(),
        arguments: r#"{"command":"rg '<div>' src/main.rs > rg.log && grep '>' src/lib.rs && rg -e '<tag>' src/opt.rs && echo hi >| out.log","description":"exercise quoted search patterns"}"#.to_owned(),
    }];

    let tool_calls = extract_tool_call_records("repo-a", SourceKind::Codex, "session-1", 0, &steps);
    let file_accesses = derive_file_access_records(&tool_calls);

    assert!(
        !file_accesses
            .iter()
            .any(|record| matches!(record.path.as_str(), "div>" | "tag>" | ">"))
    );
    assert!(!file_accesses.iter().any(|record| {
        record.path == "src/lib.rs" && matches!(record.access_type, ToolAccessKind::Write)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "src/main.rs" && matches!(record.access_type, ToolAccessKind::Read)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "src/lib.rs" && matches!(record.access_type, ToolAccessKind::Read)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "src/opt.rs" && matches!(record.access_type, ToolAccessKind::Read)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "rg.log" && matches!(record.access_type, ToolAccessKind::Write)
    }));
    assert!(file_accesses.iter().any(|record| {
        record.path == "out.log" && matches!(record.access_type, ToolAccessKind::Write)
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

    let insights =
        build_workspace_insights(&connection, 7, DEFAULT_WORKSPACE_RECENT_SESSION_LIMIT, 0)?;

    assert_eq!(
        insights.window_end,
        sqlite_local_date(&connection, "2026-04-06T08:00:00Z")?
    );
    assert_eq!(
        insights.recent_session_limit,
        DEFAULT_WORKSPACE_RECENT_SESSION_LIMIT as u64
    );
    assert_eq!(insights.recent_session_offset, 0);
    assert!(!insights.recent_sessions_has_more);
    assert_eq!(insights.active_session_count, 1);
    assert_eq!(insights.included_turn_count, 1);
    assert_eq!(insights.excluded_turn_count, 2);
    assert_eq!(insights.total_time_ms, 3_000);
    assert_eq!(insights.recent_sessions.len(), 1);
    assert_eq!(insights.recent_sessions[0].project_id, "repo-a");

    let empty_page = build_workspace_insights(&connection, 7, 0, 0)?;
    assert_eq!(empty_page.active_session_count, 1);
    assert_eq!(empty_page.recent_session_limit, 0);
    assert!(empty_page.recent_sessions_has_more);
    assert!(empty_page.recent_sessions.is_empty());

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

    let sessions = query_project_sessions(
        &index_path,
        SessionsQueryRequest {
            project_id: "repo-a",
            project_root: None,
            provider: None,
            since: None,
            until: None,
            touched_path: None,
            view: SessionsView::Full,
            limit: 50,
            offset: 0,
        },
    )?;

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
fn session_summaries_compact_view_caps_prompt_and_final_message() -> Result<()> {
    let index_path = test_index_path("session-compact-view");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    let prompt = "a".repeat(600);
    let final_message = "b".repeat(600);
    let steps_json = format!(
        "[{}]",
        (0..12)
            .map(|index| {
                format!(
                    r#"{{"type":"tool_call","timestamp":"2026-04-05T12:00:{index:02}Z","call_id":"call-{index}","name":"Edit","arguments":"{{\"file_path\":\"src/file-{index:02}.rs\"}}"}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    );
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: &prompt,
            final_answer_text: Some(&final_message),
            step_count: 12,
            tool_call_count: 12,
            has_final_answer: true,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-05T12:00:00Z",
                "completed",
                &steps_json,
            )
        },
    )?;

    let compact = query_project_sessions(
        &index_path,
        SessionsQueryRequest {
            project_id: "repo-a",
            project_root: None,
            provider: None,
            since: None,
            until: None,
            touched_path: None,
            view: SessionsView::Compact,
            limit: 50,
            offset: 0,
        },
    )?;
    let full = query_project_sessions(
        &index_path,
        SessionsQueryRequest {
            project_id: "repo-a",
            project_root: None,
            provider: None,
            since: None,
            until: None,
            touched_path: None,
            view: SessionsView::Full,
            limit: 50,
            offset: 0,
        },
    )?;

    assert_eq!(compact.view, SessionsView::Compact);
    assert_eq!(
        compact.sessions[0]
            .first_user_prompt
            .as_ref()
            .expect("prompt should exist")
            .chars()
            .count(),
        500
    );
    assert!(compact.sessions[0].first_user_prompt_truncated);
    assert_eq!(compact.sessions[0].first_user_prompt_chars, Some(500));
    assert_eq!(compact.sessions[0].first_user_prompt_total_chars, Some(600));
    assert_eq!(
        compact.sessions[0]
            .final_agent_message
            .as_ref()
            .expect("final message should exist")
            .chars()
            .count(),
        500
    );
    assert!(compact.sessions[0].final_agent_message_truncated);
    assert_eq!(compact.sessions[0].final_agent_message_chars, Some(500));
    assert_eq!(
        compact.sessions[0].final_agent_message_total_chars,
        Some(600)
    );
    assert_eq!(compact.sessions[0].edited_files.len(), 12);
    assert_eq!(full.view, SessionsView::Full);
    assert_eq!(
        full.sessions[0]
            .first_user_prompt
            .as_ref()
            .expect("prompt should exist")
            .chars()
            .count(),
        600
    );
    assert!(!full.sessions[0].first_user_prompt_truncated);
    assert_eq!(full.sessions[0].first_user_prompt_chars, Some(600));
    assert_eq!(full.sessions[0].first_user_prompt_total_chars, Some(600));
    assert_eq!(
        full.sessions[0]
            .final_agent_message
            .as_ref()
            .expect("final message should exist")
            .chars()
            .count(),
        600
    );
    assert!(!full.sessions[0].final_agent_message_truncated);
    assert_eq!(full.sessions[0].final_agent_message_chars, Some(600));
    assert_eq!(full.sessions[0].final_agent_message_total_chars, Some(600));
    assert_eq!(full.sessions[0].edited_files.len(), 12);

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn session_summaries_deduplicate_absolute_and_relative_edited_files() -> Result<()> {
    let index_path = test_index_path("session-edited-files-dedupe");
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
                "2026-04-05T12:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-05T12:00:01Z","call_id":"call-1","name":"Edit","arguments":"{\"path\":\"src/lib.rs\"}"},{"type":"tool_call","timestamp":"2026-04-05T12:00:02Z","call_id":"call-2","name":"Edit","arguments":"{\"path\":\"/tmp/repo-a/src/lib.rs\"}"}]"##,
            )
        },
    )?;

    let sessions = query_project_sessions(
        &index_path,
        SessionsQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            since: None,
            until: None,
            touched_path: None,
            view: SessionsView::Compact,
            limit: 50,
            offset: 0,
        },
    )?;

    assert_eq!(sessions.sessions[0].edited_files, vec!["src/lib.rs"]);

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

    let all_sessions = query_project_sessions(
        &index_path,
        SessionsQueryRequest {
            project_id: "repo-a",
            project_root: None,
            provider: None,
            since: None,
            until: None,
            touched_path: None,
            view: SessionsView::Full,
            limit: 50,
            offset: 0,
        },
    )?;
    let since_sessions = query_project_sessions(
        &index_path,
        SessionsQueryRequest {
            project_id: "repo-a",
            project_root: None,
            provider: None,
            since: Some("2026-04-06T00:00:00Z"),
            until: None,
            touched_path: None,
            view: SessionsView::Full,
            limit: 50,
            offset: 0,
        },
    )?;
    let until_sessions = query_project_sessions(
        &index_path,
        SessionsQueryRequest {
            project_id: "repo-a",
            project_root: None,
            provider: None,
            since: None,
            until: Some("2026-04-06T00:00:00Z"),
            touched_path: None,
            view: SessionsView::Full,
            limit: 50,
            offset: 0,
        },
    )?;
    let bounded_sessions = query_project_sessions(
        &index_path,
        SessionsQueryRequest {
            project_id: "repo-a",
            project_root: None,
            provider: None,
            since: Some("2026-04-05T12:00:00Z"),
            until: Some("2026-04-06T12:00:00Z"),
            touched_path: None,
            view: SessionsView::Full,
            limit: 50,
            offset: 0,
        },
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
fn session_summaries_filter_by_provider() -> Result<()> {
    let index_path = test_index_path("session-provider-filter");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-codex", "/tmp/repo-a"),
    )?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new(
            "repo-a",
            SourceKind::Claude,
            "session-claude",
            "/tmp/repo-a",
        ),
    )?;
    for (provider, session_id) in [
        (SourceKind::Codex, "session-codex"),
        (SourceKind::Claude, "session-claude"),
    ] {
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture::new(
                "repo-a",
                provider,
                session_id,
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                "[]",
            ),
        )?;
    }

    let sessions = query_project_sessions(
        &index_path,
        SessionsQueryRequest {
            project_id: "repo-a",
            project_root: None,
            provider: Some(SourceKind::Codex),
            since: None,
            until: None,
            touched_path: None,
            view: SessionsView::Full,
            limit: 50,
            offset: 0,
        },
    )?;

    assert_eq!(sessions.provider, Some(SourceKind::Codex));
    assert_eq!(sessions.sessions.len(), 1);
    assert_eq!(sessions.sessions[0].provider, SourceKind::Codex);
    assert_eq!(sessions.sessions[0].session_id, "session-codex");

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
        IndexedSessionFixture::new(
            "repo-a",
            SourceKind::Codex,
            "session-components",
            "/tmp/repo-a",
        ),
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
                "session-components",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"path\":\"/tmp/repo-a/src/components/planner.rs\"}"}]"##,
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
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new(
            "repo-a",
            SourceKind::Codex,
            "session-list-only",
            "/tmp/repo-a",
        ),
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
                "session-list-only",
                0,
                "2026-04-06T12:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T12:00:01Z","call_id":"call-3","name":"ListFiles","arguments":"{\"path\":\"src/components/planner.rs\"}"}]"##,
            )
        },
    )?;

    let sessions = query_project_sessions(
        &index_path,
        SessionsQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            since: None,
            until: None,
            touched_path: Some("src/components/**"),
            view: SessionsView::Full,
            limit: 50,
            offset: 0,
        },
    )?;

    assert_eq!(
        sessions
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["session-list-only", "session-components"]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn session_summaries_touched_path_uses_latest_turn_time_bounds() -> Result<()> {
    let index_path = test_index_path("session-touched-path-latest-turn-bounds");
    let connection = open_index_database(&index_path)?;
    for session_id in [
        "session-recent-touch",
        "session-old-touch",
        "session-recent-only",
    ] {
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new("repo-a", SourceKind::Codex, session_id, "/tmp/repo-a"),
        )?;
    }
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 1,
            tool_call_count: 1,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-recent-touch",
                0,
                "2026-04-01T10:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-01T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"path\":\"src/components/planner.rs\"}"}]"##,
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture::new(
            "repo-a",
            SourceKind::Codex,
            "session-recent-touch",
            1,
            "2026-04-08T10:00:00Z",
            "completed",
            "[]",
        ),
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
                "session-old-touch",
                0,
                "2026-04-01T11:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-01T11:00:01Z","call_id":"call-2","name":"Read","arguments":"{\"path\":\"src/components/context.rs\"}"}]"##,
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture::new(
            "repo-a",
            SourceKind::Codex,
            "session-recent-only",
            0,
            "2026-04-09T10:00:00Z",
            "completed",
            "[]",
        ),
    )?;

    let sessions = query_project_sessions(
        &index_path,
        SessionsQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            since: Some("2026-04-07T00:00:00Z"),
            until: None,
            touched_path: Some("src/components/**"),
            view: SessionsView::Full,
            limit: 50,
            offset: 0,
        },
    )?;

    assert!(!sessions.has_more);
    assert_eq!(
        sessions
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["session-recent-touch"]
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
        SessionsQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            since: None,
            until: None,
            touched_path: Some("/tmp/repo-a/README.md"),
            view: SessionsView::Full,
            limit: 50,
            offset: 0,
        },
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
fn session_summaries_paginate_after_touched_path_filter() -> Result<()> {
    let index_path = test_index_path("session-touched-path-pagination");
    let connection = open_index_database(&index_path)?;
    for (session_id, started_at, file_path) in [
        ("session-docs", "2026-04-06T12:00:00Z", "docs/guide.md"),
        (
            "session-new",
            "2026-04-06T11:00:00Z",
            "src/components/new.rs",
        ),
        (
            "session-mid",
            "2026-04-06T10:00:00Z",
            "src/components/mid.rs",
        ),
        (
            "session-old",
            "2026-04-06T09:00:00Z",
            "src/components/old.rs",
        ),
    ] {
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new("repo-a", SourceKind::Codex, session_id, "/tmp/repo-a"),
        )?;
        let steps_json = format!(
            r#"[{{"type":"tool_call","timestamp":"{started_at}","call_id":"call-{session_id}","name":"Read","arguments":"{{\"file_path\":\"{file_path}\"}}"}}]"#
        );
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture {
                step_count: 1,
                tool_call_count: 1,
                duration_ms: 3_000,
                ..IndexedTurnFixture::new(
                    "repo-a",
                    SourceKind::Codex,
                    session_id,
                    0,
                    started_at,
                    "completed",
                    &steps_json,
                )
            },
        )?;
    }

    let first_page = query_project_sessions(
        &index_path,
        SessionsQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            since: None,
            until: None,
            touched_path: Some("src/**/*.rs"),
            view: SessionsView::Full,
            limit: 2,
            offset: 0,
        },
    )?;
    let second_page = query_project_sessions(
        &index_path,
        SessionsQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            since: None,
            until: None,
            touched_path: Some("src/**/*.rs"),
            view: SessionsView::Full,
            limit: 2,
            offset: 2,
        },
    )?;

    assert_eq!(first_page.limit, 2);
    assert_eq!(first_page.offset, 0);
    assert!(first_page.has_more);
    assert_eq!(
        first_page
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["session-new", "session-mid"]
    );
    assert_eq!(second_page.limit, 2);
    assert_eq!(second_page.offset, 2);
    assert!(!second_page.has_more);
    assert_eq!(
        second_page
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["session-old"]
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
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"src/components/planner.rs\"}"}]"##,
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
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:05:01Z","call_id":"call-2","name":"Edit","arguments":"{\"path\":\"/tmp/repo-a/src/components/context.rs\"}"}]"##,
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
                r##"[{"type":"tool_call","timestamp":"2026-04-06T09:00:01Z","call_id":"call-3","name":"Read","arguments":"{\"file_path\":\"src/components/planner.rs\"}"}]"##,
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
                r##"[{"type":"tool_call","timestamp":"2026-04-04T10:00:01Z","call_id":"call-4","name":"Read","arguments":"{\"file_path\":\"src/components/planner.rs\"}"}]"##,
            )
        },
    )?;

    let exact = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            path: Some("./src/components/planner.rs"),
            co_touched_with: None,
            since: None,
            until: None,
            limit: 50,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
        },
    )?;
    let glob = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            path: Some("/tmp/repo-a/src/components/**/*.rs"),
            co_touched_with: None,
            since: Some("2026-04-05T00:00:00Z"),
            until: Some("2026-04-07T00:00:00Z"),
            limit: 50,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
        },
    )?;
    let codex_exact = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: Some(SourceKind::Codex),
            path: Some("./src/components/planner.rs"),
            co_touched_with: None,
            since: None,
            until: None,
            limit: 50,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
        },
    )?;
    let capped_glob = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            path: Some("/tmp/repo-a/src/components/**/*.rs"),
            co_touched_with: None,
            since: Some("2026-04-05T00:00:00Z"),
            until: Some("2026-04-07T00:00:00Z"),
            limit: 50,
            offset: 0,
            matched_path_limit: Some(1),
        },
    )?;
    let top = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            path: None,
            co_touched_with: None,
            since: None,
            until: None,
            limit: 50,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
        },
    )?;
    let top_limited = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            path: None,
            co_touched_with: None,
            since: None,
            until: None,
            limit: 1,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
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
    assert_eq!(exact.limit, 50);
    assert_eq!(exact.offset, 0);
    assert!(!exact.has_more);
    assert_eq!(codex_exact.provider, Some(SourceKind::Codex));
    assert_eq!(top.mode, FilesQueryMode::Top);
    assert_eq!(top.path, None);
    assert_eq!(top.co_touched_with, None);
    assert_eq!(
        top.files
            .iter()
            .map(|file| (
                file.path.as_str(),
                file.touch_count,
                file.session_count,
                file.read_count,
                file.write_count,
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "src/components/planner.rs",
                Some(3),
                Some(3),
                Some(3),
                Some(0),
            ),
            (
                "src/components/context.rs",
                Some(1),
                Some(1),
                Some(0),
                Some(1),
            ),
        ]
    );
    assert_eq!(
        top.files[0].last_touched_at.as_deref(),
        Some("2026-04-06T10:00:00Z")
    );
    assert_eq!(top_limited.files.len(), 1);
    assert_eq!(top_limited.files[0].path, "src/components/planner.rs");
    assert!(top_limited.has_more);
    assert_eq!(
        codex_exact
            .sessions
            .iter()
            .map(|session| (session.provider, session.session_id.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (SourceKind::Codex, "session-1"),
            (SourceKind::Codex, "session-old"),
        ]
    );
    let paged = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            path: Some("./src/components/planner.rs"),
            co_touched_with: None,
            since: None,
            until: None,
            limit: 1,
            offset: 1,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
        },
    )?;
    assert_eq!(paged.limit, 1);
    assert_eq!(paged.offset, 1);
    assert!(paged.has_more);
    assert_eq!(
        paged
            .sessions
            .iter()
            .map(|session| (
                session.provider,
                session.session_id.as_str(),
                session.touch_count
            ))
            .collect::<Vec<_>>(),
        vec![(SourceKind::Claude, "session-2", 1)]
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
            "src/components/context.rs".to_owned(),
            "src/components/planner.rs".to_owned()
        ]
    );
    assert!(!glob.sessions[0].matched_paths_truncated);
    assert_eq!(capped_glob.matched_path_limit, Some(1));
    assert_eq!(
        capped_glob.sessions[0].matched_paths,
        vec!["src/components/context.rs".to_owned()]
    );
    assert!(capped_glob.sessions[0].matched_paths_truncated);
    assert_eq!(glob.limit, 50);
    assert_eq!(glob.offset, 0);
    assert!(!glob.has_more);

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_files_rejects_explicit_empty_selectors() -> Result<()> {
    let index_path = test_index_path("query-files-empty-selector");
    let _connection = open_index_database(&index_path)?;

    let empty_path = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            path: Some(" "),
            co_touched_with: None,
            since: None,
            until: None,
            limit: 50,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
        },
    )
    .expect_err("empty path selector should fail");
    assert!(
        empty_path
            .to_string()
            .contains("PATH/--path cannot be empty")
    );

    let empty_co_touched = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            path: None,
            co_touched_with: Some(""),
            since: None,
            until: None,
            limit: 50,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
        },
    )
    .expect_err("empty co-touch selector should fail");
    assert!(
        empty_co_touched
            .to_string()
            .contains("--co-touched-with cannot be empty")
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
            r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file\":[\"src/components/planner.rs\",\"src/components/context.rs\",\"src/components/api.rs\"]}"}]"##,
        ),
        (
            SourceKind::Claude,
            "session-2",
            "2026-04-06T11:00:00Z",
            r##"[{"type":"tool_call","timestamp":"2026-04-06T11:00:01Z","call_id":"call-2","name":"Read","arguments":"{\"file\":[\"src/components/planner.rs\",\"src/components/context.rs\"]}"}]"##,
        ),
        (
            SourceKind::Codex,
            "session-3",
            "2026-04-06T12:00:00Z",
            r##"[{"type":"tool_call","timestamp":"2026-04-06T12:00:01Z","call_id":"call-3","name":"Read","arguments":"{\"file\":[\"src/components/planner.rs\",\"src/components/alpha.rs\"]}"}]"##,
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
            provider: None,
            path: None,
            co_touched_with: Some("/tmp/repo-a/src/components/planner.rs"),
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
        },
    )?;

    assert_eq!(
        result
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.co_touch_count))
            .collect::<Vec<_>>(),
        vec![
            ("src/components/context.rs", Some(2)),
            ("src/components/alpha.rs", Some(1)),
            ("src/components/api.rs", Some(1)),
        ]
    );
    assert_eq!(result.limit, 10);
    assert_eq!(result.offset, 0);
    assert!(!result.has_more);
    let paged = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            path: None,
            co_touched_with: Some("/tmp/repo-a/src/components/planner.rs"),
            since: None,
            until: None,
            limit: 1,
            offset: 1,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
        },
    )?;
    assert_eq!(paged.limit, 1);
    assert_eq!(paged.offset, 1);
    assert!(paged.has_more);
    assert_eq!(
        paged
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.co_touch_count))
            .collect::<Vec<_>>(),
        vec![("src/components/alpha.rs", Some(1))]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_files_co_touched_mode_applies_time_bounds() -> Result<()> {
    let index_path = test_index_path("query-files-co-touch-time");
    let connection = open_index_database(&index_path)?;

    for session_id in ["session-1", "session-2"] {
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new("repo-a", SourceKind::Codex, session_id, "/tmp/repo-a"),
        )?;
    }

    for (session_id, turn_ordinal, started_at, steps_json) in [
        (
            "session-1",
            0,
            "2026-04-06T10:00:00Z",
            r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file\":[\"src/components/planner.rs\",\"src/components/outside-before.rs\"]}"}]"##,
        ),
        (
            "session-1",
            1,
            "2026-04-06T12:00:00Z",
            r##"[{"type":"tool_call","timestamp":"2026-04-06T12:00:01Z","call_id":"call-2","name":"Read","arguments":"{\"file\":[\"src/components/planner.rs\",\"src/components/inside.rs\"]}"}]"##,
        ),
        (
            "session-1",
            2,
            "2026-04-06T14:00:00Z",
            r##"[{"type":"tool_call","timestamp":"2026-04-06T14:00:01Z","call_id":"call-3","name":"Read","arguments":"{\"file\":[\"src/components/outside-after.rs\"]}"}]"##,
        ),
        (
            "session-2",
            0,
            "2026-04-06T10:30:00Z",
            r##"[{"type":"tool_call","timestamp":"2026-04-06T10:30:01Z","call_id":"call-4","name":"Read","arguments":"{\"file\":[\"src/components/planner.rs\",\"src/components/outside-session.rs\"]}"}]"##,
        ),
    ] {
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture {
                step_count: 1,
                tool_call_count: 1,
                duration_ms: 3_000,
                ..IndexedTurnFixture::new(
                    "repo-a",
                    SourceKind::Codex,
                    session_id,
                    turn_ordinal,
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
            provider: None,
            path: None,
            co_touched_with: Some("/tmp/repo-a/src/components/planner.rs"),
            since: Some("2026-04-06T11:00:00Z"),
            until: Some("2026-04-06T13:00:00Z"),
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
        },
    )?;

    assert_eq!(result.since.as_deref(), Some("2026-04-06T11:00:00Z"));
    assert_eq!(result.until.as_deref(), Some("2026-04-06T13:00:00Z"));
    assert_eq!(
        result
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.co_touch_count))
            .collect::<Vec<_>>(),
        vec![("src/components/inside.rs", Some(1))]
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
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"src/components/planner.rs\"}"}]"##,
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
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:05:01Z","call_id":"call-2","name":"Edit","arguments":"{\"path\":\"/tmp/repo-a/src/components/planner.rs\"}"}]"##,
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
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:06:01Z","call_id":"call-3","name":"Read","arguments":"{\"file_path\":\"src/components/context.rs\"}"}]"##,
            )
        },
    )?;

    let result = query_project_session_files(
        &index_path,
        SessionFilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: SourceKind::Codex,
            session_id: "session-1",
            limit: 50,
            offset: 0,
        },
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
            ("src/components/planner.rs", 1, 1, 0, 2),
            ("src/components/context.rs", 1, 0, 3, 3),
        ]
    );
    let limited = query_project_session_files(
        &index_path,
        SessionFilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: SourceKind::Codex,
            session_id: "session-1",
            limit: 1,
            offset: 0,
        },
    )?;
    assert_eq!(limited.file_count, 2);
    assert_eq!(limited.limit, 1);
    assert_eq!(limited.offset, 0);
    assert!(limited.has_more);
    assert_eq!(limited.files[0].path, "src/components/planner.rs");

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
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"./src/components/planner.rs\"}"}]"##,
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
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:01:01Z","call_id":"call-2","name":"Edit","arguments":"{\"file_path\":\"src/components/planner.rs\"}"}]"##,
            )
        },
    )?;

    let result = query_project_session_files(
        &index_path,
        SessionFilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: SourceKind::Codex,
            session_id: "session-1",
            limit: 50,
            offset: 0,
        },
    )?;

    assert_eq!(
        result
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.read_count, file.write_count))
            .collect::<Vec<_>>(),
        vec![("src/components/planner.rs", 1, 1)]
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
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"path\":\"/tmp/secret.txt\"}"},{"type":"tool_call","timestamp":"2026-04-06T10:00:02Z","call_id":"call-2","name":"ListFiles","arguments":"{\"path\":\"src/components\"}"}]"##,
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
        SessionFilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: SourceKind::Codex,
            session_id: "session-1",
            limit: 50,
            offset: 0,
        },
    )?;
    let co_touched = query_project_files(
        &index_path,
        FilesQueryRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            provider: None,
            path: None,
            co_touched_with: Some("README.md"),
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
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
fn query_session_bundle_reuses_session_and_file_shapes_with_narrative_turns() -> Result<()> {
    let index_path = test_index_path("query-session-bundle");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    let long_prompt = "inspect README and source ".repeat(40);
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: &long_prompt,
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
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"},{"type":"tool_call","timestamp":"2026-04-06T10:00:02Z","call_id":"call-2","name":"List","arguments":"{\"path\":\"src\"}"}]"##,
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Update lib",
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
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:05:01Z","call_id":"call-2","name":"Edit","arguments":"{\"path\":\"src/lib.rs\"}"}]"##,
            )
        },
    )?;

    let result = query_project_session_bundle(
        &index_path,
        SessionBundleQueryRequest {
            project_id: "repo-a",
            provider: SourceKind::Codex,
            session_id: "session-1",
            project_root: Some(Path::new("/tmp/repo-a")),
            session_view: SessionsView::Compact,
            view: SessionBundleView::Narrative,
            turn_limit: 50,
            turn_offset: 0,
            step_limit: DEFAULT_TURN_STEP_LIMIT,
            step_offset: 0,
        },
    )?;

    assert_eq!(result.project_id, "repo-a");
    assert_eq!(result.provider, SourceKind::Codex);
    assert_eq!(result.session_id, "session-1");
    assert_eq!(result.session_view, SessionsView::Compact);
    assert_eq!(result.view, SessionBundleView::Narrative);
    assert_eq!(result.turn_limit, 50);
    assert_eq!(result.turn_offset, 0);
    assert!(!result.turns_has_more);
    assert_eq!(result.step_limit, DEFAULT_TURN_STEP_LIMIT as u64);
    assert_eq!(result.step_offset, 0);
    assert_eq!(result.session_file_limit, 100);
    assert!(!result.session_files_has_more);
    assert_eq!(result.session.session_id, "session-1");
    assert_eq!(result.session.turn_count, 2);
    assert!(result.session.first_user_prompt_truncated);
    assert_eq!(
        result
            .session
            .first_user_prompt
            .as_deref()
            .expect("missing first prompt")
            .chars()
            .count(),
        500
    );
    assert_eq!(result.turns.len(), 2);
    assert_eq!(result.turns[0].step_limit, DEFAULT_TURN_STEP_LIMIT as u64);
    assert_eq!(result.turns[0].step_offset, 0);
    assert!(!result.turns[0].steps_has_more);
    assert_eq!(result.turns[0].steps.len(), 2);
    assert_eq!(result.turns[0].turn_ordinal, 0);
    assert_eq!(result.turns[1].turn_ordinal, 1);
    assert!(matches!(
        &result.turns[0].steps[0],
        NormalizedTurnStep::ToolCall { arguments, .. } if arguments.is_empty()
    ));
    assert_eq!(
        result
            .session_files
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
        vec![("src/lib.rs", 0, 1, 1, 1), ("README.md", 1, 0, 0, 0)]
    );

    let page = query_project_session_bundle(
        &index_path,
        SessionBundleQueryRequest {
            project_id: "repo-a",
            provider: SourceKind::Codex,
            session_id: "session-1",
            project_root: Some(Path::new("/tmp/repo-a")),
            session_view: SessionsView::Compact,
            view: SessionBundleView::Narrative,
            turn_limit: 1,
            turn_offset: 0,
            step_limit: 1,
            step_offset: 0,
        },
    )?;

    assert_eq!(page.turn_limit, 1);
    assert_eq!(page.turn_offset, 0);
    assert!(page.turns_has_more);
    assert_eq!(page.step_limit, 1);
    assert_eq!(page.step_offset, 0);
    assert_eq!(page.session.turn_count, 2);
    assert!(page.turns[0].steps_has_more);
    assert_eq!(page.turns[0].steps.len(), 1);
    assert_eq!(
        page.turns
            .iter()
            .map(|turn| turn.turn_ordinal)
            .collect::<Vec<_>>(),
        vec![0]
    );

    let full_session = query_project_session_bundle(
        &index_path,
        SessionBundleQueryRequest {
            project_id: "repo-a",
            provider: SourceKind::Codex,
            session_id: "session-1",
            project_root: Some(Path::new("/tmp/repo-a")),
            session_view: SessionsView::Full,
            view: SessionBundleView::Narrative,
            turn_limit: 1,
            turn_offset: 0,
            step_limit: DEFAULT_TURN_STEP_LIMIT,
            step_offset: 0,
        },
    )?;
    assert_eq!(full_session.session_view, SessionsView::Full);
    assert!(!full_session.session.first_user_prompt_truncated);
    assert_eq!(
        full_session.session.first_user_prompt.as_deref(),
        Some(long_prompt.as_str())
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_session_bundle_caps_embedded_session_files() -> Result<()> {
    let index_path = test_index_path("query-session-bundle-file-cap");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    let steps_json = format!(
        "[{}]",
        (0..101)
            .map(|index| {
                format!(
                    r#"{{"type":"tool_call","timestamp":"2026-04-06T10:00:00Z","call_id":"call-{index}","name":"Edit","arguments":"{{\"path\":\"src/file-{index:03}.rs\"}}"}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    );
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            step_count: 101,
            tool_call_count: 101,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                &steps_json,
            )
        },
    )?;

    let result = query_project_session_bundle(
        &index_path,
        SessionBundleQueryRequest {
            project_id: "repo-a",
            provider: SourceKind::Codex,
            session_id: "session-1",
            project_root: Some(Path::new("/tmp/repo-a")),
            session_view: SessionsView::Compact,
            view: SessionBundleView::Narrative,
            turn_limit: 50,
            turn_offset: 0,
            step_limit: DEFAULT_TURN_STEP_LIMIT,
            step_offset: 0,
        },
    )?;

    assert_eq!(result.session_file_limit, 100);
    assert_eq!(result.session_file_count, 101);
    assert_eq!(result.session_files.file_count, 101);
    assert!(result.session_files_has_more);
    assert_eq!(result.session_files.files.len(), 100);

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_session_bundle_ignores_unrelated_invalid_session_rows() -> Result<()> {
    let index_path = test_index_path("query-session-bundle-targeted-summary");
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
            "[]",
        ),
    )?;
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
        ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, NULL, NULL, NULL, NULL, NULL)
        ",
        (
            "repo-a",
            "bogus",
            "broken-session",
            "primary",
            "/tmp/repo-a/.darc/broken-session.jsonl",
            "/tmp/repo-a",
        ),
    )?;

    let result = query_project_session_bundle(
        &index_path,
        SessionBundleQueryRequest {
            project_id: "repo-a",
            provider: SourceKind::Codex,
            session_id: "session-1",
            project_root: Some(Path::new("/tmp/repo-a")),
            session_view: SessionsView::Compact,
            view: SessionBundleView::Full,
            turn_limit: 50,
            turn_offset: 0,
            step_limit: DEFAULT_TURN_STEP_LIMIT,
            step_offset: 0,
        },
    )?;

    assert_eq!(result.session.session_id, "session-1");
    assert_eq!(result.turns.len(), 1);

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
        None,
        SourceKind::Codex,
        "session-1",
        0,
        TurnDetailOptions {
            include_raw: false,
            include_insights: false,
            narrative: true,
            step_limit: DEFAULT_TURN_STEP_LIMIT,
            step_offset: 0,
        },
    )?;

    assert_eq!(detail.step_limit, DEFAULT_TURN_STEP_LIMIT as u64);
    assert_eq!(detail.step_offset, 0);
    assert!(!detail.steps_has_more);
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

    let page = query_turn_detail(
        &index_path,
        "repo-a",
        None,
        SourceKind::Codex,
        "session-1",
        0,
        TurnDetailOptions {
            include_raw: false,
            include_insights: false,
            narrative: true,
            step_limit: 3,
            step_offset: 2,
        },
    )?;

    assert_eq!(page.step_count, 8);
    assert_eq!(page.step_limit, 3);
    assert_eq!(page.step_offset, 2);
    assert!(page.steps_has_more);
    assert_eq!(page.steps.len(), 3);
    assert!(matches!(
        &page.steps[0],
        NormalizedTurnStep::ToolCall { arguments, .. } if arguments.is_empty()
    ));

    let error = query_turn_detail(
        &index_path,
        "repo-a",
        None,
        SourceKind::Codex,
        "session-1",
        0,
        TurnDetailOptions {
            include_raw: true,
            include_insights: false,
            narrative: true,
            step_limit: DEFAULT_TURN_STEP_LIMIT,
            step_offset: 0,
        },
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("raw turn payloads require full turn detail view")
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn query_project_turns_support_since_until_and_tool_call_counts() -> Result<()> {
    let index_path = test_index_path("query-project-turns-bounds");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    for (turn_ordinal, started_at, tool_call_count) in [
        (0, "2026-04-06T09:59:00Z", 1),
        (1, "2026-04-06T10:00:00Z", 2),
        (2, "2026-04-06T10:01:00Z", 3),
    ] {
        let user_message = format!("Turn {turn_ordinal}");
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture {
                user_message: user_message.as_str(),
                step_count: tool_call_count + 1,
                tool_call_count,
                duration_ms: 3_000,
                ..IndexedTurnFixture::new(
                    "repo-a",
                    SourceKind::Codex,
                    "session-1",
                    turn_ordinal,
                    started_at,
                    "completed",
                    "[]",
                )
            },
        )?;
    }

    let result = query_project_turns(
        &index_path,
        TurnsQueryRequest {
            project_id: "repo-a",
            provider: SourceKind::Codex,
            session_id: "session-1",
            since: Some("2026-04-06T10:00:00Z"),
            until: Some("2026-04-06T10:01:00Z"),
            view: TurnsView::Oneline,
            limit: 50,
            offset: 0,
        },
    )?;

    assert_eq!(result.view, TurnsView::Oneline);
    assert_eq!(result.since.as_deref(), Some("2026-04-06T10:00:00Z"));
    assert_eq!(result.until.as_deref(), Some("2026-04-06T10:01:00Z"));
    assert_eq!(result.limit, 50);
    assert_eq!(result.offset, 0);
    assert!(!result.has_more);
    assert_eq!(result.turns.len(), 1);
    assert_eq!(result.turns[0].turn_ordinal, 1);
    assert_eq!(result.turns[0].tool_call_count, 2);

    let page = query_project_turns(
        &index_path,
        TurnsQueryRequest {
            project_id: "repo-a",
            provider: SourceKind::Codex,
            session_id: "session-1",
            since: None,
            until: None,
            view: TurnsView::Full,
            limit: 1,
            offset: 1,
        },
    )?;

    assert_eq!(page.limit, 1);
    assert_eq!(page.offset, 1);
    assert!(page.has_more);
    assert_eq!(
        page.turns
            .iter()
            .map(|turn| turn.turn_ordinal)
            .collect::<Vec<_>>(),
        vec![1]
    );

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
        None,
        SourceKind::Codex,
        "session-1",
        TurnDetailOptions {
            include_raw: false,
            include_insights: false,
            narrative: true,
            step_limit: DEFAULT_TURN_STEP_LIMIT,
            step_offset: 0,
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
fn query_turn_exists_checks_project_scoped_index_presence() -> Result<()> {
    let index_path = test_index_path("turn-exists");
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
            "[]",
        ),
    )?;

    assert!(query_turn_exists(
        &index_path,
        "repo-a",
        SourceKind::Codex,
        "session-1",
        0,
    )?);
    assert!(!query_turn_exists(
        &index_path,
        "repo-a",
        SourceKind::Codex,
        "session-1",
        1,
    )?);
    assert!(!query_turn_exists(
        &index_path,
        "repo-b",
        SourceKind::Codex,
        "session-1",
        0,
    )?);

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

    let insights: ProjectInsights =
        build_project_insights(&connection, "repo-a", None, None, 1000)?;

    assert_eq!(insights.provider, None);
    assert_eq!(insights.turn_limit, 1000);
    assert_eq!(insights.inspected_turn_count, 2);
    assert!(!insights.turns_has_more);
    assert_eq!(insights.failure_count, 1);
    assert_eq!(insights.total_time_ms, 5_000);
    assert_eq!(insights.most_common_tools[0].name, "Edit");
    assert!(
        insights
            .most_read_files
            .iter()
            .any(|stat| { stat.path == "README.md" && stat.read_count == 1 })
    );
    assert!(
        insights
            .most_written_files
            .iter()
            .any(|stat| { stat.path == "src/main.rs" && stat.write_count == 1 })
    );
    let limited_insights: ProjectInsights =
        build_project_insights(&connection, "repo-a", None, None, 1)?;
    assert_eq!(limited_insights.turn_limit, 1);
    assert_eq!(limited_insights.inspected_turn_count, 1);
    assert!(limited_insights.turns_has_more);

    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Claude, "session-2", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            duration_ms: 7_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Claude,
                "session-2",
                0,
                "2026-04-06T11:00:00Z",
                "completed",
                r#"[{"type":"tool_call","timestamp":"2026-04-06T11:00:01Z","call_id":"call-3","name":"Write","arguments":"{\"path\":\"CLAUDE.md\"}"}]"#,
            )
        },
    )?;
    let codex_insights: ProjectInsights =
        build_project_insights(&connection, "repo-a", None, Some(SourceKind::Codex), 1000)?;
    let claude_insights: ProjectInsights =
        build_project_insights(&connection, "repo-a", None, Some(SourceKind::Claude), 1000)?;
    assert_eq!(codex_insights.provider, Some(SourceKind::Codex));
    assert_eq!(codex_insights.total_time_ms, 5_000);
    assert_eq!(claude_insights.provider, Some(SourceKind::Claude));
    assert_eq!(claude_insights.total_time_ms, 7_000);
    assert_eq!(claude_insights.most_common_tools[0].name, "Write");

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn project_insights_display_project_root_relative_paths() -> Result<()> {
    let index_path = test_index_path("project-insights-project-root-paths");
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
            r#"[{"type":"tool_call","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"/tmp/repo-a/src/lib.rs\"}"},{"type":"tool_call","timestamp":"2026-04-06T10:00:02Z","call_id":"call-2","name":"Read","arguments":"{\"file_path\":\"src/lib.rs\"}"},{"type":"tool_call","timestamp":"2026-04-06T10:00:03Z","call_id":"call-3","name":"Write","arguments":"{\"path\":\"/tmp/repo-a/src/main.rs\"}"}]"#,
        ),
    )?;

    let insights = build_project_insights(
        &connection,
        "repo-a",
        Some(Path::new("/tmp/repo-a")),
        None,
        1000,
    )?;

    assert!(
        insights
            .most_read_files
            .iter()
            .any(|stat| stat.path == "src/lib.rs" && stat.read_count == 2)
    );
    assert!(
        insights
            .most_written_files
            .iter()
            .any(|stat| stat.path == "src/main.rs" && stat.write_count == 1)
    );
    assert!(
        insights
            .most_read_files
            .iter()
            .chain(insights.most_written_files.iter())
            .all(|stat| !stat.path.starts_with("/tmp/repo-a/"))
    );

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

    let insights: TurnInsights = build_turn_insights(
        &connection,
        "repo-a",
        None,
        SourceKind::Codex,
        "session-1",
        0,
    )?;

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
            .map(|stat| { (stat.path.as_str(), stat.read_count, stat.write_count,) })
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
fn turn_insights_keep_absolute_paths() -> Result<()> {
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

    let insights: TurnInsights = build_turn_insights(
        &connection,
        "repo-a",
        None,
        SourceKind::Codex,
        "session-1",
        0,
    )?;

    assert_eq!(insights.files.len(), 1);
    assert_eq!(insights.files[0].path, "/tmp/repo-a/README.md");
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
fn turn_insights_display_project_root_relative_paths() -> Result<()> {
    let index_path = test_index_path("turn-insights-project-root-paths");
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
            r#"[{"type":"tool_call","timestamp":"2026-04-06T11:10:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"/tmp/repo-a/README.md\"}"},{"type":"tool_call","timestamp":"2026-04-06T11:10:02Z","call_id":"call-2","name":"Read","arguments":"{\"file_path\":\"README.md\"}"}]"#,
        ),
    )?;

    let insights = build_turn_insights(
        &connection,
        "repo-a",
        Some(Path::new("/tmp/repo-a")),
        SourceKind::Codex,
        "session-1",
        0,
    )?;

    assert_eq!(
        insights
            .files
            .iter()
            .map(|stat| (stat.path.as_str(), stat.read_count, stat.write_count))
            .collect::<Vec<_>>(),
        vec![("README.md", 2, 0)]
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn turn_insights_omit_non_concrete_stale_file_rows() -> Result<()> {
    let index_path = test_index_path("turn-insights-stale-pseudo-paths");
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
            r#"[{"type":"tool_call","timestamp":"2026-04-06T11:10:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"}]"#,
        ),
    )?;
    for (path, repo_relative_path) in [
        ("+x", Some("+x")),
        ("$tmp/Cargo.toml", Some("$tmp/Cargo.toml")),
    ] {
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
                repo_relative_path,
                file_name
            )
            VALUES ('repo-a', 'codex', 'session-1', 0, 0, 'call-1', '2026-04-06T11:10:01Z', 'exec_command', 'edit', ?1, ?2, ?3)
            ",
            rusqlite::params![path, repo_relative_path, path],
        )?;
    }

    let insights = build_turn_insights(
        &connection,
        "repo-a",
        Some(Path::new("/tmp/repo-a")),
        SourceKind::Codex,
        "session-1",
        0,
    )?;

    assert_eq!(
        insights
            .files
            .iter()
            .map(|stat| stat.path.as_str())
            .collect::<Vec<_>>(),
        vec!["README.md"]
    );

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

    let insights = build_turn_insights(
        &connection,
        "repo-a",
        None,
        SourceKind::Codex,
        "session-1",
        0,
    )?;

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

    let insights = build_turn_insights(
        &connection,
        "repo-a",
        None,
        SourceKind::Codex,
        "session-1",
        0,
    )?;

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

    let error = build_turn_insights(
        &connection,
        "repo-a",
        None,
        SourceKind::Codex,
        "session-1",
        9,
    )
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
            final_answer_text: Some("The inspection is complete."),
            step_count: 2,
            tool_call_count: 1,
            tool_output_count: 1,
            has_final_answer: true,
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
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Run a command with a shared exact marker",
            step_count: 2,
            tool_call_count: 1,
            tool_output_count: 1,
            duration_ms: 5_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                2,
                "2026-04-06T10:10:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T10:10:01Z","call_id":"call-3","name":"exec_command","arguments":"{\"cmd\":\"echo SHARED_EXACT_MARKER\"}"},{"type":"tool_call_output","timestamp":"2026-04-06T10:10:02Z","call_id":"call-3","output":"SHARED_EXACT_MARKER"}]"##,
            )
        },
    )?;

    let result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Keyword,
            query: "Inspect",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;
    let secret_result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Keyword,
            query: "SECRET_TOKEN",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
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
    assert_eq!(
        result.hits[0].agent_answer_preview.as_deref(),
        Some("The inspection is complete.")
    );
    assert_eq!(
        result.hits[0].agent_answer_preview_chars,
        result.hits[0].agent_answer_total_chars
    );
    assert!(secret_result.hits.is_empty());

    let literal_result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Literal,
            query: "SECRET_TOKEN=top-secret",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;
    let regex_result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Regex,
            query: "SECRET_[A-Z]+=top-secret",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;
    let literal_with_output = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Literal,
            query: "SECRET_TOKEN=top-secret",
            include_tool_output: true,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;
    let regex_with_output = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Regex,
            query: "SECRET_[A-Z]+=top-secret",
            include_tool_output: true,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;
    let shared_literal_result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Literal,
            query: "SHARED_EXACT_MARKER",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;
    let shared_regex_result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Regex,
            query: "SHARED_EXACT_[A-Z]+",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;
    let content_only_literal_result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Literal,
            query: "SHARED_EXACT_MARKER",
            include_tool_output: false,
            fields: &[EvidenceField::UserMessage, EvidenceField::FinalAnswer],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;
    let excluded_tool_arguments_result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Literal,
            query: "SHARED_EXACT_MARKER",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[EvidenceField::ToolArguments],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;
    let tool_output_field_without_opt_in = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Literal,
            query: "SECRET_TOKEN=top-secret",
            include_tool_output: false,
            fields: &[EvidenceField::ToolOutput],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    );

    assert!(!literal_result.include_tool_output);
    assert!(literal_result.hits.is_empty());
    assert!(!regex_result.include_tool_output);
    assert!(regex_result.hits.is_empty());
    assert!(literal_with_output.include_tool_output);
    assert_eq!(literal_with_output.hits.len(), 1);
    assert_eq!(literal_with_output.hits[0].turn_ordinal, 1);
    assert_eq!(literal_with_output.hits[0].matches[0].field, "tool_output");
    assert_eq!(
        literal_with_output.hits[0].matches[0].snippet,
        "SECRET_TOKEN=top-secret"
    );
    assert!(regex_with_output.include_tool_output);
    assert_eq!(regex_with_output.hits.len(), 1);
    assert_eq!(regex_with_output.hits[0].matches[0].field, "tool_output");
    assert_eq!(shared_literal_result.hits.len(), 1);
    assert_eq!(
        shared_literal_result.hits[0].matches[0].field,
        "tool_arguments"
    );
    assert_eq!(shared_regex_result.hits.len(), 1);
    assert_eq!(
        shared_regex_result.hits[0].matches[0].field,
        "tool_arguments"
    );
    assert_eq!(
        content_only_literal_result.fields,
        vec!["user_message".to_owned(), "final_answer".to_owned()]
    );
    assert!(content_only_literal_result.hits.is_empty());
    assert_eq!(
        excluded_tool_arguments_result.excluded_fields,
        vec!["tool_arguments".to_owned()]
    );
    assert!(excluded_tool_arguments_result.hits.is_empty());
    assert!(
        tool_output_field_without_opt_in
            .unwrap_err()
            .to_string()
            .contains("--field tool-output requires --include-tool-output")
    );

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn search_turns_exact_modes_match_extended_evidence_fields() -> Result<()> {
    let index_path = test_index_path("search-extended-evidence");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    let steps_json = serde_json::to_string(&vec![
        NormalizedTurnStep::Reasoning {
            timestamp: "2026-04-06T10:00:01Z".to_owned(),
            summary: vec!["Plaintext DARC_REASONING_BIN summary".to_owned()],
            encrypted: false,
        },
        NormalizedTurnStep::Commentary {
            timestamp: "2026-04-06T10:00:02Z".to_owned(),
            text: "Commentary marker DARC_COMMENTARY_BIN".to_owned(),
        },
        NormalizedTurnStep::Attachment {
            timestamp: "2026-04-06T10:00:03Z".to_owned(),
            attachment_type: "deferred_tools_delta".to_owned(),
            payload_json: "{\"added\":[\"Read\"]}".to_owned(),
        },
        NormalizedTurnStep::Delegation {
            timestamp: "2026-04-06T10:00:04Z".to_owned(),
            call_id: Some("call-del".to_owned()),
            task_id: Some("task-alpha".to_owned()),
            event: "completed".to_owned(),
            agent_id: Some("agent-1".to_owned()),
            agent_type: Some("general-purpose".to_owned()),
            status: Some("completed".to_owned()),
            summary: Some("Delegation summary PLANNER_MARKER".to_owned()),
            payload_json: "{\"totalDurationMs\":12}".to_owned(),
        },
        NormalizedTurnStep::HookSummary {
            timestamp: "2026-04-06T10:00:05Z".to_owned(),
            call_id: Some("call-hook".to_owned()),
            hook_count: 2,
            prevented_continuation: false,
            has_output: true,
            level: Some("suggestion".to_owned()),
            payload_json: "{\"command\":\"callback\"}".to_owned(),
        },
        NormalizedTurnStep::ProviderResponseItem {
            timestamp: "2026-04-06T10:00:06Z".to_owned(),
            item_type: "web_search_call".to_owned(),
            payload_json: "{\"status\":\"completed\",\"action\":{\"type\":\"open_page\"}}"
                .to_owned(),
        },
    ])?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Check extended evidence",
            step_count: 6,
            attachment_count: 1,
            delegation_count: 1,
            hook_summary_count: 1,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                &steps_json,
            )
        },
    )?;

    for (mode, query, expected_field) in [
        (
            SearchMode::Regex,
            "DARC_REASONING_[A-Z]+",
            EvidenceField::ReasoningSummary.as_str(),
        ),
        (
            SearchMode::Literal,
            "DARC_COMMENTARY_BIN",
            EvidenceField::Commentary.as_str(),
        ),
        (
            SearchMode::Literal,
            "deferred_tools_delta",
            EvidenceField::AttachmentMetadata.as_str(),
        ),
        (
            SearchMode::Literal,
            "PLANNER_MARKER",
            EvidenceField::DelegationSummary.as_str(),
        ),
        (
            SearchMode::Literal,
            "general-purpose",
            EvidenceField::DelegationMetadata.as_str(),
        ),
        (
            SearchMode::Literal,
            "suggestion",
            EvidenceField::HookSummary.as_str(),
        ),
        (
            SearchMode::Literal,
            "open_page",
            EvidenceField::ProviderResponseItemMetadata.as_str(),
        ),
    ] {
        let result = query_search_turns(
            &index_path,
            SearchTurnsRequest {
                project_id: "repo-a",
                project_root: None,
                mode,
                query,
                include_tool_output: false,
                fields: &[],
                excluded_fields: &[],
                provider: None,
                session_id: None,
                since: None,
                until: None,
                limit: 10,
                offset: 0,
                matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
                match_limit: None,
            },
        )?;

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].session_id, "session-1");
        assert_eq!(result.hits[0].turn_ordinal, 0);
        assert_eq!(result.hits[0].matches[0].field, expected_field);
    }

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn search_turns_exact_modes_preserve_outer_whitespace() -> Result<()> {
    let index_path = test_index_path("search-exact-whitespace");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "plain error",
            step_count: 1,
            tool_output_count: 1,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T10:00:00Z",
                "completed",
                r#"[{"type":"tool_call_output","timestamp":"2026-04-06T10:00:01Z","call_id":"call-1","output":"error"}]"#,
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "spaced marker",
            step_count: 1,
            tool_output_count: 1,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-06T10:01:00Z",
                "completed",
                r#"[{"type":"tool_call_output","timestamp":"2026-04-06T10:01:01Z","call_id":"call-2","output":" error "}]"#,
            )
        },
    )?;

    for mode in [SearchMode::Literal, SearchMode::Regex] {
        let result = query_search_turns(
            &index_path,
            SearchTurnsRequest {
                project_id: "repo-a",
                project_root: None,
                mode,
                query: " error ",
                include_tool_output: true,
                fields: &[],
                excluded_fields: &[],
                provider: None,
                session_id: None,
                since: None,
                until: None,
                limit: 10,
                offset: 0,
                matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
                match_limit: None,
            },
        )?;

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].turn_ordinal, 1);
        assert!(result.hits[0].matches[0].evidence_ordinal > 0);
        assert_eq!(result.hits[0].matches[0].snippet, " error ");
    }

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn search_turns_exact_modes_cap_nested_matches() -> Result<()> {
    const MATCHING_EVIDENCE_ROWS: usize = 21;

    let index_path = test_index_path("search-exact-match-cap");
    let mut connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "many matching evidence rows",
            step_count: 0,
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
    insert_turn_evidence_rows(
        &mut connection,
        SyntheticEvidenceRows {
            project_id: "repo-a",
            provider: SourceKind::Codex,
            session_id: "session-1",
            turn_ordinal: 0,
            first_evidence_ordinal: 1,
            row_count: MATCHING_EVIDENCE_ROWS,
            field: EvidenceField::Commentary.as_str(),
            text: "repeated-marker evidence",
        },
    )?;

    let result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Literal,
            query: "repeated-marker",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 1,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;

    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.match_limit, Some(DEFAULT_SEARCH_MATCH_LIMIT as u64));
    assert_eq!(result.hits[0].matches.len(), DEFAULT_SEARCH_MATCH_LIMIT);
    assert_eq!(
        result.hits[0].matches_count,
        DEFAULT_SEARCH_MATCH_LIMIT as u64
    );
    assert!(result.hits[0].matches_truncated);

    let custom_limit = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Literal,
            query: "repeated-marker",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 1,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: Some(3),
        },
    )?;

    assert_eq!(custom_limit.match_limit, Some(3));
    assert_eq!(custom_limit.hits[0].matches.len(), 3);
    assert_eq!(custom_limit.hits[0].matches_count, 3);
    assert!(custom_limit.hits[0].matches_truncated);

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn search_turns_literal_filters_evidence_before_preview_cap() -> Result<()> {
    const NON_MATCHING_EVIDENCE_ROWS: usize = 50;

    let index_path = test_index_path("search-literal-filtered-evidence");
    let mut connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "turn with one late literal evidence match",
            step_count: 0,
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
    insert_turn_evidence_rows(
        &mut connection,
        SyntheticEvidenceRows {
            project_id: "repo-a",
            provider: SourceKind::Codex,
            session_id: "session-1",
            turn_ordinal: 0,
            first_evidence_ordinal: 1,
            row_count: NON_MATCHING_EVIDENCE_ROWS,
            field: EvidenceField::Commentary.as_str(),
            text: "nonmatching evidence",
        },
    )?;
    insert_turn_evidence_rows(
        &mut connection,
        SyntheticEvidenceRows {
            project_id: "repo-a",
            provider: SourceKind::Codex,
            session_id: "session-1",
            turn_ordinal: 0,
            first_evidence_ordinal: NON_MATCHING_EVIDENCE_ROWS + 1,
            row_count: 1,
            field: EvidenceField::Commentary.as_str(),
            text: "late-literal-marker evidence",
        },
    )?;

    let result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Literal,
            query: "late-literal-marker",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 1,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;

    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].matches.len(), 1);
    assert_eq!(
        result.hits[0].matches[0].field,
        EvidenceField::Commentary.as_str()
    );
    assert!(!result.hits[0].matches_truncated);

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn search_turns_literal_streams_past_legacy_candidate_cap() -> Result<()> {
    const NON_MATCHING_EVIDENCE_ROWS: usize = 50_001;

    let index_path = test_index_path("search-literal-streaming");
    let mut connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "recent nonmatching turn",
            step_count: 0,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T11:00:00Z",
                "completed",
                "[]",
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "older rare-literal-needle turn",
            step_count: 0,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-06T10:00:00Z",
                "completed",
                "[]",
            )
        },
    )?;

    insert_turn_evidence_rows(
        &mut connection,
        SyntheticEvidenceRows {
            project_id: "repo-a",
            provider: SourceKind::Codex,
            session_id: "session-1",
            turn_ordinal: 0,
            first_evidence_ordinal: 1,
            row_count: NON_MATCHING_EVIDENCE_ROWS,
            field: EvidenceField::Commentary.as_str(),
            text: "nonmatching evidence",
        },
    )?;

    let result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Literal,
            query: "rare-literal-needle",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;

    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].turn_ordinal, 1);

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn search_turns_regex_streams_past_legacy_candidate_cap() -> Result<()> {
    const NON_MATCHING_EVIDENCE_ROWS: usize = 50_001;

    let index_path = test_index_path("search-regex-streaming");
    let mut connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "recent nonmatching turn",
            step_count: 0,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T11:00:00Z",
                "completed",
                "[]",
            )
        },
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "older rare-regex-needle turn",
            step_count: 0,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                1,
                "2026-04-06T10:00:00Z",
                "completed",
                "[]",
            )
        },
    )?;
    insert_turn_evidence_rows(
        &mut connection,
        SyntheticEvidenceRows {
            project_id: "repo-a",
            provider: SourceKind::Codex,
            session_id: "session-1",
            turn_ordinal: 0,
            first_evidence_ordinal: 1,
            row_count: NON_MATCHING_EVIDENCE_ROWS,
            field: EvidenceField::Commentary.as_str(),
            text: "nonmatching evidence",
        },
    )?;

    let result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::Regex,
            query: "rare-regex-[a-z]+",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;

    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].turn_ordinal, 1);
    assert_eq!(result.hits[0].matches[0].field, "user_message");
    assert!(!result.has_more);

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
            step_count: 2,
            tool_call_count: 2,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T11:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T11:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"src/main,old.rs\"}"},{"type":"tool_call","timestamp":"2026-04-06T11:00:02Z","call_id":"call-2","name":"Edit","arguments":"{\"file_path\":\"/tmp/repo-a/src/main,old.rs\"}"}]"##,
            )
        },
    )?;

    let file_name_result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            mode: SearchMode::FileName,
            query: "main,old.rs",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;
    let file_path_result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            mode: SearchMode::FilePath,
            query: "src/main,old.rs",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;
    let glob_path_result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            mode: SearchMode::FilePath,
            query: "/tmp/repo-a/src/**",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;
    let path_fragment_result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            mode: SearchMode::PathFragment,
            query: "main,old",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;

    assert_eq!(file_name_result.hits.len(), 1);
    assert_eq!(file_path_result.hits.len(), 1);
    assert_eq!(glob_path_result.hits.len(), 1);
    assert_eq!(path_fragment_result.hits.len(), 1);
    assert_eq!(
        file_name_result.hits[0].matched_paths,
        vec!["src/main,old.rs"]
    );
    assert_eq!(file_name_result.hits[0].matched_paths_count, 1);
    assert_eq!(
        file_path_result.hits[0].matched_paths,
        vec!["src/main,old.rs"]
    );
    assert_eq!(file_path_result.hits[0].matched_paths_count, 1);
    assert_eq!(
        glob_path_result.hits[0].matched_paths,
        vec!["src/main,old.rs"]
    );
    assert_eq!(glob_path_result.hits[0].matched_paths_count, 1);
    assert_eq!(
        path_fragment_result.hits[0].matched_paths,
        vec!["src/main,old.rs"]
    );
    assert_eq!(path_fragment_result.hits[0].matched_paths_count, 1);

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn search_turns_file_modes_cap_matched_paths() -> Result<()> {
    let index_path = test_index_path("search-file-path-limit");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Inspect source files",
            step_count: 2,
            tool_call_count: 2,
            duration_ms: 3_000,
            ..IndexedTurnFixture::new(
                "repo-a",
                SourceKind::Codex,
                "session-1",
                0,
                "2026-04-06T11:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-06T11:00:01Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"src/a.rs\"}"},{"type":"tool_call","timestamp":"2026-04-06T11:00:02Z","call_id":"call-2","name":"Read","arguments":"{\"file_path\":\"src/b.rs\"}"}]"##,
            )
        },
    )?;

    let capped = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            mode: SearchMode::FilePath,
            query: "src/**",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: Some(1),
            match_limit: None,
        },
    )?;
    let uncapped = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: Some(Path::new("/tmp/repo-a")),
            mode: SearchMode::FilePath,
            query: "src/**",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 10,
            offset: 0,
            matched_path_limit: None,
            match_limit: None,
        },
    )?;

    assert_eq!(capped.matched_path_limit, Some(1));
    assert_eq!(capped.hits[0].matched_paths, vec!["src/a.rs"]);
    assert_eq!(capped.hits[0].matched_paths_count, 2);
    assert!(capped.hits[0].matched_paths_truncated);
    assert_eq!(uncapped.matched_path_limit, None);
    assert_eq!(uncapped.hits[0].matched_paths, vec!["src/a.rs", "src/b.rs"]);
    assert_eq!(uncapped.hits[0].matched_paths_count, 2);
    assert!(!uncapped.hits[0].matched_paths_truncated);

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn search_turns_file_modes_dedupe_before_pagination() -> Result<()> {
    let index_path = test_index_path("search-file-dedupe-pagination");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;

    for (turn_ordinal, started_at, file_path) in [
        (0, "2026-04-06T11:03:00Z", "src/foo"),
        (1, "2026-04-06T11:02:00Z", "src/foo"),
        (2, "2026-04-06T11:01:00Z", "src/foo-alpha.rs"),
        (3, "2026-04-06T11:00:00Z", "src/foo-beta.rs"),
    ] {
        let steps_json = format!(
            r#"[{{"type":"tool_call","timestamp":"{started_at}","call_id":"call-{turn_ordinal}","name":"Read","arguments":"{{\"file_path\":\"{file_path}\"}}"}}]"#
        );
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture {
                user_message: "Inspect a source file",
                step_count: 1,
                tool_call_count: 1,
                duration_ms: 3_000,
                ..IndexedTurnFixture::new(
                    "repo-a",
                    SourceKind::Codex,
                    "session-1",
                    turn_ordinal,
                    started_at,
                    "completed",
                    &steps_json,
                )
            },
        )?;
    }

    let result = query_search_turns(
        &index_path,
        SearchTurnsRequest {
            project_id: "repo-a",
            project_root: None,
            mode: SearchMode::FileName,
            query: "foo",
            include_tool_output: false,
            fields: &[],
            excluded_fields: &[],
            provider: None,
            session_id: None,
            since: None,
            until: None,
            limit: 3,
            offset: 0,
            matched_path_limit: Some(DEFAULT_MATCHED_PATH_LIMIT),
            match_limit: None,
        },
    )?;

    assert_eq!(result.hits.len(), 3);
    assert!(result.has_more);
    assert_eq!(result.hits[0].turn_ordinal, 0);
    assert_eq!(result.hits[1].turn_ordinal, 1);
    assert_eq!(result.hits[2].turn_ordinal, 2);

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}

#[test]
fn full_turn_payload_serialization_skips_oneline_helper_fields() -> Result<()> {
    let index_path = test_index_path("turn-payload-skip-oneline-helpers");
    let connection = open_index_database(&index_path)?;
    insert_indexed_session(
        &connection,
        IndexedSessionFixture::new("repo-a", SourceKind::Codex, "session-1", "/tmp/repo-a"),
    )?;
    insert_indexed_turn(
        &connection,
        IndexedTurnFixture {
            user_message: "Please use staged init here",
            final_answer_text: Some("Use staged init in the reply too."),
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

    let turns = query_project_turns(
        &index_path,
        TurnsQueryRequest {
            project_id: "repo-a",
            provider: SourceKind::Codex,
            session_id: "session-1",
            since: None,
            until: None,
            view: TurnsView::Full,
            limit: 50,
            offset: 0,
        },
    )?;
    let turns_value = to_value(&turns)?;
    let turns_row = turns_value["turns"][0]
        .as_object()
        .context("turn row should serialize as an object")?;
    assert!(!turns_row.contains_key("oneline_user_prompt_preview"));

    fs::remove_dir_all(
        index_path
            .parent()
            .expect("index path should have a parent"),
    )?;
    Ok(())
}
