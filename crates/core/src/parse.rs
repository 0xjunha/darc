use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::Serialize;
use walkdir::WalkDir;

use crate::{
    SourceKind, active_project::load_active_project, constants::INDEX_DB_FILE_NAME,
    default_root_path, index_db::open_index_database, rollout::ParseDeterminism,
};

/// Parses one Codex rollout file into user-visible turns.
pub fn parse_codex_rollout(path: &Path) -> Result<CodexRollout> {
    crate::rollout::codex::parse_rollout_file(path)
}

/// Reports the results of indexing archived Codex turns for one project.
#[derive(Debug, Clone)]
pub struct ParseReport {
    pub project_name: String,
    pub project_root: PathBuf,
    pub codex_archive_root: PathBuf,
    pub index_db_path: PathBuf,
    pub sessions_indexed: usize,
    pub turns_indexed: usize,
}

/// Parses archived Codex rollouts for the active project into SQLite.
pub fn parse_project_codex_turns(root: Option<PathBuf>) -> Result<ParseReport> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    parse_project_codex_turns_from(&current_dir, root.unwrap_or_else(default_root_path))
}

/// Stores the parsed Codex dialogue for one rollout file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexRollout {
    pub session_id: String,
    pub cwd: PathBuf,
    pub cli_version: String,
    pub schema_id: String,
    pub determinism: ParseDeterminism,
    pub turns: Vec<CodexTurn>,
}

/// Stores one user turn and the assistant activity that followed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexTurn {
    pub turn_id: Option<String>,
    pub user_message: String,
    pub final_answer: Option<CodexTurnMessage>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub status: CodexTurnStatus,
    pub steps: Vec<CodexTurnStep>,
}

/// Stores one top-level assistant message attached to a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexTurnMessage {
    pub timestamp: String,
    pub text: String,
}

/// Tracks whether a parsed Codex turn finished normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexTurnStatus {
    Completed,
    Aborted,
    Incomplete,
}

impl CodexTurnStatus {
    /// Returns the stable SQLite string value for one turn status.
    fn as_sql_text(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Aborted => "aborted",
            Self::Incomplete => "incomplete",
        }
    }
}

/// Stores one ordered assistant-visible step inside a Codex turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CodexTurnStep {
    Reasoning {
        timestamp: String,
        summary: Vec<String>,
        encrypted: bool,
    },
    Commentary {
        timestamp: String,
        text: String,
    },
    ToolCall {
        timestamp: String,
        call_id: String,
        name: String,
        arguments: String,
    },
    ToolCallOutput {
        timestamp: String,
        call_id: String,
        output: String,
    },
    ProviderResponseItem {
        timestamp: String,
        item_type: String,
        payload_json: String,
    },
}

/// Stores one parsed archived rollout before SQLite insertion.
#[derive(Debug, Clone)]
struct IndexedCodexRollout {
    archive_path: String,
    rollout: CodexRollout,
    size: u64,
    mtime_ms: u64,
}

/// Parses archived Codex rollouts for one explicit current directory and memstack root.
fn parse_project_codex_turns_from(current_dir: &Path, root: PathBuf) -> Result<ParseReport> {
    let active_project = load_active_project(current_dir, &root)?;
    let codex_archive_root = active_project
        .project
        .sessions_root
        .join(SourceKind::Codex.directory_name());
    let rollout_paths = discover_archived_codex_rollouts(&codex_archive_root)?;
    let parsed_rollouts = rollout_paths
        .iter()
        .map(|path| parse_indexed_codex_rollout(path, &active_project.project.sessions_root))
        .collect::<Result<Vec<_>>>()?;
    let parsed_rollouts = deduplicate_indexed_codex_rollouts(parsed_rollouts);
    let turns_indexed = parsed_rollouts
        .iter()
        .map(|indexed| indexed.rollout.turns.len())
        .sum();
    let index_db_path = root.join(INDEX_DB_FILE_NAME);
    let mut connection = open_index_database(&index_db_path)?;

    rewrite_project_codex_turns(
        &mut connection,
        &active_project.project.id,
        &parsed_rollouts,
    )?;

    Ok(ParseReport {
        project_name: active_project.project.name,
        project_root: active_project.current_root,
        codex_archive_root,
        index_db_path,
        sessions_indexed: parsed_rollouts.len(),
        turns_indexed,
    })
}

/// Discovers archived Codex rollout files below one project archive root.
fn discover_archived_codex_rollouts(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut rollout_paths = Vec::new();
    for entry in WalkDir::new(root) {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        if !entry.file_type().is_file() || !is_archived_codex_rollout(entry.path()) {
            continue;
        }
        rollout_paths.push(entry.into_path());
    }
    rollout_paths.sort();
    Ok(rollout_paths)
}

/// Returns whether one path points at an archived Codex rollout file.
fn is_archived_codex_rollout(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-"))
}

/// Parses one archived rollout and records its project-relative archive path.
fn parse_indexed_codex_rollout(path: &Path, sessions_root: &Path) -> Result<IndexedCodexRollout> {
    let (size, mtime_ms) = file_snapshot(path)?;
    let archive_path = path
        .strip_prefix(sessions_root)
        .with_context(|| {
            format!(
                "failed to strip project sessions root {} from {}",
                sessions_root.display(),
                path.display()
            )
        })?
        .to_string_lossy()
        .into_owned();
    let rollout = parse_codex_rollout(path)?;

    Ok(IndexedCodexRollout {
        archive_path,
        rollout,
        size,
        mtime_ms,
    })
}

/// Keeps one archived rollout per logical Codex session id.
fn deduplicate_indexed_codex_rollouts(
    parsed_rollouts: Vec<IndexedCodexRollout>,
) -> Vec<IndexedCodexRollout> {
    let mut unique_rollouts = BTreeMap::<String, IndexedCodexRollout>::new();

    for indexed in parsed_rollouts {
        let session_id = indexed.rollout.session_id.clone();
        match unique_rollouts.get(&session_id) {
            Some(existing) if !prefer_indexed_codex_rollout(&indexed, existing) => {}
            _ => {
                unique_rollouts.insert(session_id, indexed);
            }
        }
    }

    unique_rollouts.into_values().collect()
}

/// Returns whether the left archived rollout should replace the right duplicate.
fn prefer_indexed_codex_rollout(left: &IndexedCodexRollout, right: &IndexedCodexRollout) -> bool {
    left.size
        .cmp(&right.size)
        .then_with(|| left.mtime_ms.cmp(&right.mtime_ms))
        .then_with(|| left.archive_path.cmp(&right.archive_path))
        .is_gt()
}

/// Reads stable comparison metadata from one archived rollout file.
fn file_snapshot(path: &Path) -> Result<(u64, u64)> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("failed to read modified time for {}", path.display()))?;
    let mtime_ms = modified
        .duration_since(std::time::UNIX_EPOCH)
        .with_context(|| format!("modified time predates Unix epoch for {}", path.display()))?
        .as_millis()
        .try_into()
        .context("modified time exceeds u64 milliseconds range")?;

    Ok((metadata.len(), mtime_ms))
}

/// Rewrites one project's indexed Codex sessions and turns inside SQLite.
fn rewrite_project_codex_turns(
    connection: &mut Connection,
    project_id: &str,
    parsed_rollouts: &[IndexedCodexRollout],
) -> Result<()> {
    let transaction = connection
        .transaction()
        .context("failed to begin SQLite transaction")?;
    transaction
        .execute(
            "DELETE FROM codex_sessions WHERE project_id = ?1",
            params![project_id],
        )
        .context("failed to clear existing indexed Codex sessions")?;

    {
        let mut insert_session = transaction
            .prepare(
                "
                INSERT INTO codex_sessions (
                    project_id,
                    session_id,
                    archive_path,
                    cwd
                ) VALUES (?1, ?2, ?3, ?4)
                ",
            )
            .context("failed to prepare Codex session insert")?;
        let mut insert_turn = transaction
            .prepare(
                "
                INSERT INTO codex_turns (
                    project_id,
                    session_id,
                    turn_ordinal,
                    turn_id,
                    started_at,
                    completed_at,
                    status,
                    user_message,
                    final_answer_at,
                    final_answer_text,
                    steps_json
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                ",
            )
            .context("failed to prepare Codex turn insert")?;

        for indexed in parsed_rollouts {
            insert_session
                .execute(params![
                    project_id,
                    indexed.rollout.session_id,
                    indexed.archive_path,
                    indexed.rollout.cwd.to_string_lossy(),
                ])
                .with_context(|| {
                    format!(
                        "failed to insert Codex session {}",
                        indexed.rollout.session_id
                    )
                })?;

            for (turn_ordinal, turn) in indexed.rollout.turns.iter().enumerate() {
                let steps_json = serde_json::to_string(&turn.steps)
                    .context("failed to serialize Codex turn steps")?;
                let final_answer_at = turn.final_answer.as_ref().map(|message| &message.timestamp);
                let final_answer_text = turn.final_answer.as_ref().map(|message| &message.text);

                insert_turn
                    .execute(params![
                        project_id,
                        indexed.rollout.session_id,
                        turn_ordinal as i64,
                        turn.turn_id,
                        turn.started_at,
                        turn.completed_at,
                        turn.status.as_sql_text(),
                        turn.user_message,
                        final_answer_at,
                        final_answer_text,
                        steps_json,
                    ])
                    .with_context(|| {
                        format!(
                            "failed to insert Codex turn {} for session {}",
                            turn_ordinal, indexed.rollout.session_id
                        )
                    })?;
            }
        }
    }

    transaction
        .commit()
        .context("failed to commit SQLite parse transaction")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        io::Cursor,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use anyhow::Result;
    use rusqlite::Connection;
    use serde_json::Value;

    use super::{
        CodexRollout, CodexTurnMessage, CodexTurnStatus, CodexTurnStep,
        parse_project_codex_turns_from,
    };
    use crate::config::{ProjectConfig, SharedConfig, SourcesConfig};
    use crate::constants::{CONFIG_FILE_NAME, INDEX_DB_FILE_NAME};
    use crate::rollout::{ParseDeterminism, codex::parse_rollout_reader};

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
                    super::CodexTurn {
                        turn_id: Some("turn-1".to_owned()),
                        user_message: "First task".to_owned(),
                        final_answer: Some(CodexTurnMessage {
                            timestamp: "2026-01-01T00:00:09Z".to_owned(),
                            text: "First reply".to_owned(),
                        }),
                        started_at: "2026-01-01T00:00:03Z".to_owned(),
                        completed_at: Some("2026-01-01T00:00:09Z".to_owned()),
                        status: CodexTurnStatus::Completed,
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
                    super::CodexTurn {
                        turn_id: Some("turn-2".to_owned()),
                        user_message: "Second task".to_owned(),
                        final_answer: Some(CodexTurnMessage {
                            timestamp: "2026-01-01T00:00:12Z".to_owned(),
                            text: "Second reply".to_owned(),
                        }),
                        started_at: "2026-01-01T00:00:11Z".to_owned(),
                        completed_at: Some("2026-01-01T00:00:12Z".to_owned()),
                        status: CodexTurnStatus::Completed,
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
        parse_rollout_reader(Cursor::new(input), Path::new("fixture.jsonl"))
    }

    /// Builds a unique temporary directory for one parse test fixture.
    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("test-{prefix}-{}-{nanos}", std::process::id()))
    }

    /// Writes one text file while creating any missing parent directories.
    fn write_file(path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
        Ok(())
    }

    /// Writes a minimal shared config for one parse indexing test.
    fn write_parse_config(
        root: &Path,
        project_root: &Path,
        sessions_root: &Path,
    ) -> Result<String> {
        let project_id = "repo-abc123".to_owned();
        let config = SharedConfig::new(
            root.to_path_buf(),
            vec![ProjectConfig {
                id: project_id.clone(),
                name: "repo".into(),
                local_path: project_root.to_path_buf(),
                git_upstream: None,
                sessions_root: sessions_root.to_path_buf(),
                known_paths: Vec::new(),
            }],
            SourcesConfig::default(),
        );
        write_file(
            &root.join(CONFIG_FILE_NAME),
            &toml::to_string_pretty(&config)?,
        )?;
        Ok(project_id)
    }

    #[test]
    fn parse_project_indexes_codex_turns_into_sqlite() -> Result<()> {
        let memstack_root = unique_test_dir("parse-index");
        let project_root = memstack_root.join("repo");
        let sessions_root = memstack_root.join("projects/repo-abc123/sessions");
        let codex_root = sessions_root.join("codex");
        let index_db_path = memstack_root.join(INDEX_DB_FILE_NAME);
        fs::create_dir_all(&project_root)?;
        write_parse_config(&memstack_root, &project_root, &sessions_root)?;

        write_file(
            &codex_root
                .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
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

        let report = parse_project_codex_turns_from(&project_root, memstack_root.clone())?;

        assert_eq!(report.project_name, "repo");
        assert_eq!(report.project_root, fs::canonicalize(&project_root)?);
        assert_eq!(report.sessions_indexed, 1);
        assert_eq!(report.turns_indexed, 2);
        assert_eq!(report.index_db_path, index_db_path);

        let connection = Connection::open(&report.index_db_path)?;
        let indexed_sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM codex_sessions WHERE project_id = ?1",
            ["repo-abc123"],
            |row| row.get(0),
        )?;
        let indexed_turns: i64 = connection.query_row(
            "SELECT COUNT(*) FROM codex_turns WHERE project_id = ?1",
            ["repo-abc123"],
            |row| row.get(0),
        )?;
        let second_turn: (String, String) = connection.query_row(
            "
            SELECT user_message, final_answer_text
            FROM codex_turns
            WHERE project_id = ?1 AND session_id = ?2 AND turn_ordinal = 1
            ",
            ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(indexed_sessions, 1);
        assert_eq!(indexed_turns, 2);
        assert_eq!(second_turn.0, "Second task");
        assert_eq!(second_turn.1, "Second reply");

        Ok(())
    }

    #[test]
    fn parse_project_rewrites_existing_indexed_turns() -> Result<()> {
        let memstack_root = unique_test_dir("parse-rewrite");
        let project_root = memstack_root.join("repo");
        let sessions_root = memstack_root.join("projects/repo-abc123/sessions");
        let codex_root = sessions_root.join("codex");
        let rollout_path = codex_root
            .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
        fs::create_dir_all(&project_root)?;
        write_parse_config(&memstack_root, &project_root, &sessions_root)?;

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
        parse_project_codex_turns_from(&project_root, memstack_root.clone())?;

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

        let report = parse_project_codex_turns_from(&project_root, memstack_root.clone())?;
        let connection = Connection::open(memstack_root.join(INDEX_DB_FILE_NAME))?;
        let indexed_turns: i64 = connection.query_row(
            "SELECT COUNT(*) FROM codex_turns WHERE project_id = ?1",
            ["repo-abc123"],
            |row| row.get(0),
        )?;
        let first_turn: (String, String) = connection.query_row(
            "
            SELECT user_message, final_answer_text
            FROM codex_turns
            WHERE project_id = ?1 AND session_id = ?2 AND turn_ordinal = 0
            ",
            ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        assert_eq!(report.turns_indexed, 2);
        assert_eq!(indexed_turns, 2);
        assert_eq!(first_turn.0, "Updated task");
        assert_eq!(first_turn.1, "Updated reply");

        Ok(())
    }

    #[test]
    fn parse_project_deduplicates_archived_rollouts_with_same_session_id() -> Result<()> {
        let memstack_root = unique_test_dir("parse-deduplicate");
        let project_root = memstack_root.join("repo");
        let sessions_root = memstack_root.join("projects/repo-abc123/sessions");
        let codex_root = sessions_root.join("codex");
        fs::create_dir_all(&project_root)?;
        write_parse_config(&memstack_root, &project_root, &sessions_root)?;

        write_file(
            &codex_root
                .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
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
            &codex_root
                .join("rollout-2026-04-01T11-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
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

        let report = parse_project_codex_turns_from(&project_root, memstack_root.clone())?;
        let connection = Connection::open(memstack_root.join(INDEX_DB_FILE_NAME))?;
        let indexed_sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM codex_sessions WHERE project_id = ?1",
            ["repo-abc123"],
            |row| row.get(0),
        )?;
        let indexed_turns: i64 = connection.query_row(
            "SELECT COUNT(*) FROM codex_turns WHERE project_id = ?1",
            ["repo-abc123"],
            |row| row.get(0),
        )?;
        let indexed_row: (String, String) = connection.query_row(
            "
            SELECT archive_path, user_message
            FROM codex_sessions s
            JOIN codex_turns t
              ON t.project_id = s.project_id
             AND t.session_id = s.session_id
             AND t.turn_ordinal = 0
            WHERE s.project_id = ?1
            ",
            ["repo-abc123"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        assert_eq!(report.sessions_indexed, 1);
        assert_eq!(report.turns_indexed, 1);
        assert_eq!(indexed_sessions, 1);
        assert_eq!(indexed_turns, 1);
        assert_eq!(
            indexed_row.0,
            "codex/rollout-2026-04-01T11-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"
        );
        assert_eq!(indexed_row.1, "Fresh task");

        Ok(())
    }
}
