use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::project_paths::normalize_project_path;

/// Parses one Codex rollout file into user-visible turns.
pub fn parse_codex_rollout(path: &Path) -> Result<CodexRollout> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    parse_codex_rollout_reader(reader, path)
}

/// Stores the parsed Codex dialogue for one rollout file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexRollout {
    pub session_id: String,
    pub cwd: PathBuf,
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
}

#[derive(Debug)]
struct NumberedRawLine {
    line_no: usize,
    line: RawLine,
}

#[derive(Debug, Deserialize)]
struct RawLine {
    timestamp: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Debug, Deserialize)]
struct RawSessionMetaPayload {
    id: String,
    cwd: String,
}

#[derive(Debug, Deserialize)]
struct RawEventPayload {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    last_agent_message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawResponseItemPayload {
    Message {
        role: String,
        #[serde(default)]
        phase: Option<String>,
        #[serde(default)]
        content: Vec<RawMessageContent>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
    Reasoning {
        #[serde(default)]
        summary: Vec<Value>,
        #[serde(default)]
        encrypted_content: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct RawMessageContent {
    #[serde(default)]
    text: Option<String>,
}

/// Parses one Codex rollout reader into user-visible turns.
fn parse_codex_rollout_reader<R: BufRead>(reader: R, source_path: &Path) -> Result<CodexRollout> {
    let raw_lines = read_raw_lines(reader)?;
    let has_event_user_boundaries = raw_lines.iter().any(|line| {
        line.line.kind == "event_msg"
            && line.line.payload.get("type").and_then(Value::as_str) == Some("user_message")
    });

    let mut session_id = None;
    let mut cwd = None;
    let mut pending_turn_id = None;
    let mut current_turn = None;
    let mut turns = Vec::new();

    for numbered_line in raw_lines {
        let line_no = numbered_line.line_no;
        let RawLine {
            timestamp,
            kind,
            payload,
        } = numbered_line.line;

        match kind.as_str() {
            "session_meta" => {
                let meta: RawSessionMetaPayload = serde_json::from_value(payload)
                    .with_context(|| format!("failed to parse session_meta on line {line_no}"))?;
                session_id = Some(meta.id);
                cwd = Some(normalize_project_path(Path::new(&meta.cwd)));
            }
            "event_msg" => {
                let event: RawEventPayload = serde_json::from_value(payload)
                    .with_context(|| format!("failed to parse event_msg on line {line_no}"))?;
                match event.kind.as_str() {
                    "task_started" => pending_turn_id = event.turn_id,
                    "user_message" => {
                        if let Some(message) = event.message {
                            close_open_turn(&mut current_turn, &mut turns);
                            current_turn =
                                Some(start_turn(timestamp, pending_turn_id.take(), message));
                        }
                    }
                    "task_complete" => {
                        if let Some(mut turn) = current_turn.take() {
                            if !turn_has_final_answer(&turn)
                                && let Some(message) = non_empty_text(event.last_agent_message)
                            {
                                turn.final_answer = Some(CodexTurnMessage {
                                    timestamp: timestamp.clone(),
                                    text: message,
                                });
                            }
                            turn.completed_at = Some(timestamp);
                            turn.status = CodexTurnStatus::Completed;
                            turns.push(turn);
                        }
                    }
                    "turn_aborted" => {
                        if let Some(mut turn) = current_turn.take() {
                            turn.completed_at = Some(timestamp);
                            turn.status = CodexTurnStatus::Aborted;
                            turns.push(turn);
                        }
                    }
                    _ => {}
                }
            }
            "response_item" => {
                let item: RawResponseItemPayload = serde_json::from_value(payload)
                    .with_context(|| format!("failed to parse response_item on line {line_no}"))?;
                match item {
                    RawResponseItemPayload::Message {
                        role,
                        phase,
                        content,
                    } => {
                        let Some(text) = non_empty_text(message_text(content)) else {
                            continue;
                        };

                        if role == "user" {
                            if !has_event_user_boundaries && !is_user_boilerplate(&text) {
                                close_open_turn(&mut current_turn, &mut turns);
                                current_turn =
                                    Some(start_turn(timestamp, pending_turn_id.take(), text));
                            }
                            continue;
                        }

                        if let Some(turn) = current_turn.as_mut()
                            && role == "assistant"
                        {
                            match phase.as_deref() {
                                Some("commentary") => {
                                    turn.steps
                                        .push(CodexTurnStep::Commentary { timestamp, text });
                                }
                                Some("final_answer") => {
                                    turn.final_answer = Some(CodexTurnMessage { timestamp, text });
                                }
                                _ => {}
                            }
                        }
                    }
                    RawResponseItemPayload::FunctionCall {
                        call_id,
                        name,
                        arguments,
                    } => {
                        if let Some(turn) = current_turn.as_mut() {
                            turn.steps.push(CodexTurnStep::ToolCall {
                                timestamp,
                                call_id,
                                name,
                                arguments,
                            });
                        }
                    }
                    RawResponseItemPayload::FunctionCallOutput { call_id, output } => {
                        if let Some(turn) = current_turn.as_mut() {
                            turn.steps.push(CodexTurnStep::ToolCallOutput {
                                timestamp,
                                call_id,
                                output,
                            });
                        }
                    }
                    RawResponseItemPayload::Reasoning {
                        summary,
                        encrypted_content,
                    } => {
                        if let Some(turn) = current_turn.as_mut() {
                            turn.steps.push(CodexTurnStep::Reasoning {
                                timestamp,
                                summary: reasoning_summary(summary),
                                encrypted: encrypted_content.is_some(),
                            });
                        }
                    }
                    RawResponseItemPayload::Unknown => {}
                }
            }
            _ => {}
        }
    }

    close_open_turn(&mut current_turn, &mut turns);

    let session_id = session_id
        .with_context(|| format!("missing session_meta id in {}", source_path.display()))?;
    let cwd =
        cwd.with_context(|| format!("missing session_meta cwd in {}", source_path.display()))?;

    Ok(CodexRollout {
        session_id,
        cwd,
        turns,
    })
}

/// Reads and deserializes every JSONL line with source line numbers.
fn read_raw_lines<R: BufRead>(reader: R) -> Result<Vec<NumberedRawLine>> {
    let mut raw_lines = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line_no = index + 1;
        let line = line.with_context(|| format!("failed to read line {line_no}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let raw: RawLine = serde_json::from_str(&line)
            .with_context(|| format!("failed to deserialize JSONL line {line_no}"))?;
        raw_lines.push(NumberedRawLine { line_no, line: raw });
    }
    Ok(raw_lines)
}

/// Starts a new Codex turn from a parsed user message boundary.
fn start_turn(timestamp: String, turn_id: Option<String>, user_message: String) -> CodexTurn {
    CodexTurn {
        turn_id,
        user_message,
        final_answer: None,
        started_at: timestamp,
        completed_at: None,
        status: CodexTurnStatus::Incomplete,
        steps: Vec::new(),
    }
}

/// Finalizes the current turn before a new boundary or end-of-file.
fn close_open_turn(current_turn: &mut Option<CodexTurn>, turns: &mut Vec<CodexTurn>) {
    if let Some(turn) = current_turn.take() {
        turns.push(normalize_turn(turn));
    }
}

/// Normalizes implicit completion for turns closed without an explicit event.
fn normalize_turn(mut turn: CodexTurn) -> CodexTurn {
    if turn.status == CodexTurnStatus::Incomplete && turn_has_final_answer(&turn) {
        turn.status = CodexTurnStatus::Completed;
        if turn.completed_at.is_none()
            && let Some(timestamp) = final_answer_timestamp(&turn)
        {
            turn.completed_at = Some(timestamp.to_owned());
        }
    }
    turn
}

/// Returns whether a turn already has a parsed final answer.
fn turn_has_final_answer(turn: &CodexTurn) -> bool {
    turn.final_answer.is_some()
}

/// Returns the final-answer timestamp recorded for a turn.
fn final_answer_timestamp(turn: &CodexTurn) -> Option<&str> {
    turn.final_answer
        .as_ref()
        .map(|answer| answer.timestamp.as_str())
}

/// Joins all text fragments from a message content array.
fn message_text(content: Vec<RawMessageContent>) -> Option<String> {
    let text: Vec<String> = content.into_iter().filter_map(|part| part.text).collect();
    if text.is_empty() {
        return None;
    }
    Some(text.join("\n"))
}

/// Extracts string summaries from a reasoning payload.
fn reasoning_summary(summary: Vec<Value>) -> Vec<String> {
    summary
        .into_iter()
        .filter_map(|value| match value {
            Value::String(text) => non_empty_text(Some(text)),
            Value::Object(map) => map
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .and_then(|text| non_empty_text(Some(text))),
            _ => None,
        })
        .collect()
}

/// Returns a trimmed string when the source text is not empty.
fn non_empty_text(text: Option<String>) -> Option<String> {
    let text = text?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.to_owned())
}

/// Filters legacy response-item user messages that only repeat setup boilerplate.
fn is_user_boilerplate(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.is_empty()
        || trimmed.starts_with("# AGENTS.md instructions for ")
        || trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<turn_aborted>")
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, path::Path};

    use anyhow::Result;

    use super::{
        CodexRollout, CodexTurnMessage, CodexTurnStatus, CodexTurnStep, parse_codex_rollout_reader,
    };

    #[test]
    fn parses_two_turns_with_event_boundaries() -> Result<()> {
        let rollout = parse_fixture(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-two-turns","cwd":"/tmp/repo"}}
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
            r##"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-fallback","cwd":"/tmp/repo"}}
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
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-complete","cwd":"/tmp/repo"}}
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
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-aborted","cwd":"/tmp/repo"}}
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

    fn parse_fixture(input: &str) -> Result<CodexRollout> {
        parse_codex_rollout_reader(Cursor::new(input), Path::new("fixture.jsonl"))
    }
}
