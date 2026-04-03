use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::{
    parse::{CodexTurn, CodexTurnMessage, CodexTurnStatus, CodexTurnStep},
    rollout::ParseDeterminism,
};

const EXACT_CLAUDE_VERSIONS: &[&str] = &["2.1.81", "2.1.84", "2.1.87"];
const PRIMARY_SCHEMA_ID: &str = "claude.primary_transcript";
const SUBAGENT_SCHEMA_ID: &str = "claude.subagent_transcript";

/// Identifies whether one archived Claude rollout is a parent session or a subagent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeSessionKind {
    Primary,
    Subagent,
}

impl ClaudeSessionKind {}

/// Stores the archive-derived identity constraints for one Claude rollout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeArchivedContext {
    pub(crate) session_id: String,
    pub(crate) parent_session_id: Option<String>,
    pub(crate) session_kind: ClaudeSessionKind,
    pub(crate) expected_rollout_session_id: String,
    pub(crate) expected_agent_id: Option<String>,
}

/// Stores one parsed Claude rollout in the normalized indexing shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeRollout {
    pub(crate) session_id: String,
    pub(crate) parent_session_id: Option<String>,
    pub(crate) session_kind: ClaudeSessionKind,
    pub(crate) cwd: PathBuf,
    pub(crate) cli_version: Option<String>,
    pub(crate) schema_id: String,
    pub(crate) determinism: ParseDeterminism,
    pub(crate) turns: Vec<CodexTurn>,
}

/// Parses one archived Claude rollout file into normalized turns.
pub(crate) fn parse_rollout_file(
    path: &Path,
    context: &ClaudeArchivedContext,
) -> Result<ClaudeRollout> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    parse_rollout_reader(BufReader::new(file), path, context)
}

/// Parses one Claude rollout reader into the shared normalized indexing model.
fn parse_rollout_reader<R: BufRead>(
    reader: R,
    source_path: &Path,
    context: &ClaudeArchivedContext,
) -> Result<ClaudeRollout> {
    let mut parser = ClaudeRolloutParser::new(source_path, context);
    for (index, line) in reader.lines().enumerate() {
        let line_no = index + 1;
        let line = line.with_context(|| {
            format!(
                "failed to read Claude rollout line {line_no} from {}",
                source_path.display()
            )
        })?;
        parser.process_line(line_no, &line)?;
    }
    parser.finish()
}

/// Tracks the in-progress Claude turn while one rollout is parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaudeTurnBuilder {
    turn_id: Option<String>,
    user_message: String,
    started_at: String,
    completed_at: Option<String>,
    status: CodexTurnStatus,
    final_answer: Option<CodexTurnMessage>,
    steps: Vec<CodexTurnStep>,
}

impl ClaudeTurnBuilder {
    /// Starts one new Claude turn from a user prompt line.
    fn new(turn_id: Option<String>, user_message: String, started_at: String) -> Self {
        Self {
            turn_id,
            user_message,
            started_at,
            completed_at: None,
            status: CodexTurnStatus::Incomplete,
            final_answer: None,
            steps: Vec::new(),
        }
    }

    /// Finalizes one in-progress turn into the shared persisted model.
    fn finish(self) -> CodexTurn {
        CodexTurn {
            turn_id: self.turn_id,
            user_message: self.user_message,
            final_answer: self.final_answer,
            started_at: self.started_at,
            completed_at: self.completed_at,
            status: self.status,
            steps: self.steps,
        }
    }
}

/// Tracks Claude rollout metadata and turn state while parsing one file.
struct ClaudeRolloutParser<'a> {
    source_path: &'a Path,
    context: &'a ClaudeArchivedContext,
    cwd: Option<PathBuf>,
    cli_version: Option<String>,
    best_effort: bool,
    current_turn: Option<ClaudeTurnBuilder>,
    turns: Vec<CodexTurn>,
}

impl<'a> ClaudeRolloutParser<'a> {
    /// Creates one Claude rollout parser for a single archived rollout file.
    fn new(source_path: &'a Path, context: &'a ClaudeArchivedContext) -> Self {
        Self {
            source_path,
            context,
            cwd: None,
            cli_version: None,
            best_effort: false,
            current_turn: None,
            turns: Vec::new(),
        }
    }

    /// Applies one raw JSONL line to the current parser state.
    fn process_line(&mut self, line_no: usize, line: &str) -> Result<()> {
        let value: Value = serde_json::from_str(line).with_context(|| {
            format!(
                "failed to parse Claude JSONL line {line_no} in {}",
                self.source_path.display()
            )
        })?;
        let object = value.as_object().with_context(|| {
            format!(
                "Claude rollout line {line_no} in {} is not a JSON object",
                self.source_path.display()
            )
        })?;

        self.capture_metadata(object)?;
        let line_type = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match line_type {
            "user" => self.process_user_line(object)?,
            "assistant" => self.process_assistant_line(object)?,
            "progress" => self.push_provider_item(
                object
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                progress_item_type(object),
                object.get("data").cloned().unwrap_or(Value::Null),
            )?,
            "system" => self.push_provider_item(
                object
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                system_item_type(object),
                Value::Object(object.clone()),
            )?,
            "queue-operation" | "file-history-snapshot" | "last-prompt" => {}
            _ => {
                self.best_effort = true;
                self.push_provider_item(
                    object
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    format!("claude.{line_type}"),
                    Value::Object(object.clone()),
                )?;
            }
        }

        Ok(())
    }

    /// Finishes the rollout after the last input line has been processed.
    fn finish(mut self) -> Result<ClaudeRollout> {
        if let Some(turn) = self.current_turn.take() {
            self.turns.push(turn.finish());
        }

        let cwd = self.cwd.clone().with_context(|| {
            format!(
                "missing Claude cwd metadata in {}",
                self.source_path.display()
            )
        })?;
        let exact_version = self
            .cli_version
            .as_deref()
            .is_some_and(is_exact_supported_claude_version);
        let determinism = if exact_version && !self.best_effort {
            ParseDeterminism::Exact
        } else {
            ParseDeterminism::BestEffortForward
        };

        Ok(ClaudeRollout {
            session_id: self.context.session_id.clone(),
            parent_session_id: self.context.parent_session_id.clone(),
            session_kind: self.context.session_kind,
            cwd,
            cli_version: self.cli_version.clone(),
            schema_id: match self.context.session_kind {
                ClaudeSessionKind::Primary => PRIMARY_SCHEMA_ID.to_owned(),
                ClaudeSessionKind::Subagent => SUBAGENT_SCHEMA_ID.to_owned(),
            },
            determinism,
            turns: self.turns,
        })
    }

    /// Captures and validates the stable session metadata present on one Claude line.
    fn capture_metadata(&mut self, object: &Map<String, Value>) -> Result<()> {
        if let Some(raw_session_id) = object.get("sessionId").and_then(Value::as_str)
            && raw_session_id != self.context.expected_rollout_session_id
        {
            bail!(
                "mismatched Claude session ids in {}: archive expects `{}`, rollout reported `{raw_session_id}`",
                self.source_path.display(),
                self.context.expected_rollout_session_id
            );
        }
        if let Some(expected_agent_id) = &self.context.expected_agent_id
            && let Some(raw_agent_id) = object.get("agentId").and_then(Value::as_str)
            && normalize_agent_id(raw_agent_id) != normalize_agent_id(expected_agent_id)
        {
            bail!(
                "mismatched Claude agent ids in {}: archive expects `{expected_agent_id}`, rollout reported `{raw_agent_id}`",
                self.source_path.display()
            );
        }

        if let Some(cwd) = object.get("cwd").and_then(Value::as_str) {
            let parsed = PathBuf::from(cwd);
            if let Some(existing) = &self.cwd {
                if existing != &parsed {
                    self.best_effort = true;
                }
            } else {
                self.cwd = Some(parsed);
            }
        }
        if let Some(version) = object.get("version").and_then(Value::as_str) {
            if let Some(existing) = &self.cli_version {
                if existing != version {
                    self.best_effort = true;
                }
            } else {
                self.cli_version = Some(version.to_owned());
            }
        }

        Ok(())
    }

    /// Handles one Claude `user` line using prompt and tool-result heuristics.
    fn process_user_line(&mut self, object: &Map<String, Value>) -> Result<()> {
        let message = object
            .get("message")
            .and_then(Value::as_object)
            .context("missing Claude user message object")?;
        let timestamp = object
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if message.get("role").and_then(Value::as_str) != Some("user") {
            self.best_effort = true;
            self.push_provider_item(
                &timestamp,
                "claude.user".to_owned(),
                Value::Object(object.clone()),
            )?;
            return Ok(());
        }

        let content = message.get("content").cloned().unwrap_or(Value::Null);
        let tool_results = extract_tool_results(&content);
        let has_tool_results = tool_results.is_some();
        let is_prompt = is_prompt_message(object, &content);

        if let Some(tool_results) = tool_results {
            self.push_tool_results(&timestamp, tool_results)?;
        }

        if is_prompt {
            let Some(user_message) = extract_prompt_text(&content).filter(|text| !text.is_empty())
            else {
                self.best_effort = true;
                return Ok(());
            };
            if let Some(turn) = self.current_turn.take() {
                self.turns.push(turn.finish());
            }
            let turn_id = object
                .get("promptId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    object
                        .get("uuid")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                });
            self.current_turn = Some(ClaudeTurnBuilder::new(turn_id, user_message, timestamp));
            return Ok(());
        }

        if has_tool_results {
            return Ok(());
        }

        self.push_provider_item(
            &timestamp,
            user_item_type(object),
            Value::Object(object.clone()),
        )
    }

    /// Handles one Claude `assistant` line and maps it into normalized turn steps.
    fn process_assistant_line(&mut self, object: &Map<String, Value>) -> Result<()> {
        let Some(turn) = self.current_turn.as_mut() else {
            self.best_effort = true;
            return Ok(());
        };

        let message = object
            .get("message")
            .and_then(Value::as_object)
            .context("missing Claude assistant message object")?;
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            self.best_effort = true;
            turn.steps.push(CodexTurnStep::ProviderResponseItem {
                timestamp: object
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                item_type: "claude.assistant".to_owned(),
                payload_json: serde_json::to_string(&Value::Object(object.clone()))
                    .context("failed to serialize Claude assistant payload")?,
            });
            return Ok(());
        }

        let timestamp = object
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let stop_reason = message.get("stop_reason").and_then(Value::as_str);
        let content = message.get("content").and_then(Value::as_array);
        if content.is_none() {
            self.best_effort = true;
        }

        let mut terminal_text = Vec::new();
        for item in content.into_iter().flatten() {
            let Some(item_object) = item.as_object() else {
                self.best_effort = true;
                continue;
            };
            let item_type = item_object
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match item_type {
                "thinking" => {
                    let summary = item_object
                        .get("thinking")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned)
                        .into_iter()
                        .collect();
                    turn.steps.push(CodexTurnStep::Reasoning {
                        timestamp: timestamp.clone(),
                        summary,
                        encrypted: item_object.get("signature").is_some(),
                    });
                }
                "text" => {
                    let text = item_object
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if is_terminal_stop_reason(stop_reason) {
                        terminal_text.push(text);
                    } else {
                        turn.steps.push(CodexTurnStep::Commentary {
                            timestamp: timestamp.clone(),
                            text,
                        });
                    }
                }
                "tool_use" => {
                    let call_id = item_object
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let name = item_object
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let arguments = value_to_text(item_object.get("input").unwrap_or(&Value::Null))
                        .context("failed to serialize Claude tool input")?;
                    turn.steps.push(CodexTurnStep::ToolCall {
                        timestamp: timestamp.clone(),
                        call_id,
                        name,
                        arguments,
                    });
                }
                other => {
                    self.best_effort = true;
                    turn.steps.push(CodexTurnStep::ProviderResponseItem {
                        timestamp: timestamp.clone(),
                        item_type: format!("claude.assistant_content.{other}"),
                        payload_json: serde_json::to_string(item)
                            .context("failed to serialize Claude assistant content item")?,
                    });
                }
            }
        }

        if !terminal_text.is_empty() {
            let text = terminal_text.join("\n\n");
            turn.final_answer = Some(CodexTurnMessage {
                timestamp: timestamp.clone(),
                text: text.clone(),
            });
            turn.completed_at = Some(timestamp);
            turn.status = terminal_stop_status(object, stop_reason, &text);
        } else if object.get("error").is_some()
            || object
                .get("isApiErrorMessage")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            turn.completed_at = Some(timestamp);
            turn.status = CodexTurnStatus::Incomplete;
        }

        Ok(())
    }

    /// Adds one normalized tool result step to the current turn when available.
    fn push_tool_results(
        &mut self,
        timestamp: &str,
        tool_results: Vec<&Map<String, Value>>,
    ) -> Result<()> {
        let Some(turn) = self.current_turn.as_mut() else {
            self.best_effort = true;
            return Ok(());
        };

        for result in tool_results {
            let call_id = result
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let payload = tool_result_output(result)?;
            turn.steps.push(CodexTurnStep::ToolCallOutput {
                timestamp: timestamp.to_owned(),
                call_id,
                output: payload,
            });
        }

        Ok(())
    }

    /// Adds one provider-specific preserved step to the active turn when possible.
    fn push_provider_item(
        &mut self,
        timestamp: &str,
        item_type: String,
        payload: Value,
    ) -> Result<()> {
        let Some(turn) = self.current_turn.as_mut() else {
            return Ok(());
        };

        turn.steps.push(CodexTurnStep::ProviderResponseItem {
            timestamp: timestamp.to_owned(),
            item_type,
            payload_json: serde_json::to_string(&payload)
                .context("failed to serialize preserved Claude provider item")?,
        });
        Ok(())
    }
}

/// Returns whether one Claude version string is covered exactly by observed fixtures.
fn is_exact_supported_claude_version(version: &str) -> bool {
    EXACT_CLAUDE_VERSIONS.contains(&version)
}

/// Normalizes one Claude subagent id across filename and rollout payload variants.
fn normalize_agent_id(value: &str) -> &str {
    value.strip_prefix("agent-").unwrap_or(value)
}

/// Returns whether one Claude user line should begin a new normalized turn.
fn is_prompt_message(object: &Map<String, Value>, content: &Value) -> bool {
    if object
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || object.contains_key("origin")
    {
        return false;
    }

    match content {
        Value::String(_) => true,
        Value::Array(items) => {
            items.iter().any(is_text_item) && !items.iter().all(is_tool_result_item)
        }
        _ => false,
    }
}

/// Extracts one user-visible prompt string from a Claude `message.content` value.
fn extract_prompt_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.to_owned()),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(|item| {
                    item.as_object()
                        .filter(|object| object.get("type").and_then(Value::as_str) == Some("text"))
                        .and_then(|object| object.get("text").and_then(Value::as_str))
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            Some(text)
        }
        _ => None,
    }
}

/// Returns the tool-result objects when one Claude user line only reports tool output.
fn extract_tool_results(content: &Value) -> Option<Vec<&Map<String, Value>>> {
    let items = content.as_array()?;
    let results = items
        .iter()
        .filter_map(Value::as_object)
        .filter(|object| object.get("type").and_then(Value::as_str) == Some("tool_result"))
        .collect::<Vec<_>>();
    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

/// Returns whether one assistant stop reason marks a terminal reply.
fn is_terminal_stop_reason(stop_reason: Option<&str>) -> bool {
    matches!(stop_reason, Some("end_turn") | Some("stop_sequence"))
}

/// Resolves the normalized turn status for one terminal Claude assistant line.
fn terminal_stop_status(
    object: &Map<String, Value>,
    stop_reason: Option<&str>,
    text: &str,
) -> CodexTurnStatus {
    if matches!(stop_reason, Some("end_turn")) {
        return CodexTurnStatus::Completed;
    }
    if object
        .get("isApiErrorMessage")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || object.get("error").is_some()
        || text == "No response requested."
    {
        CodexTurnStatus::Incomplete
    } else {
        CodexTurnStatus::Completed
    }
}

/// Returns whether one Claude array item is a plain prompt text payload.
fn is_text_item(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|object| object.get("type").and_then(Value::as_str))
        == Some("text")
}

/// Returns whether one Claude array item is a tool-result payload.
fn is_tool_result_item(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|object| object.get("type").and_then(Value::as_str))
        == Some("tool_result")
}

/// Formats one provider item type for a Claude progress line.
fn progress_item_type(object: &Map<String, Value>) -> String {
    object
        .get("data")
        .and_then(Value::as_object)
        .and_then(|data| data.get("type").and_then(Value::as_str))
        .map(|kind| format!("claude.progress.{kind}"))
        .unwrap_or_else(|| "claude.progress".to_owned())
}

/// Formats one provider item type for a Claude system line.
fn system_item_type(object: &Map<String, Value>) -> String {
    object
        .get("subtype")
        .and_then(Value::as_str)
        .map(|kind| format!("claude.system.{kind}"))
        .unwrap_or_else(|| "claude.system".to_owned())
}

/// Formats one provider item type for a non-prompt Claude user line.
fn user_item_type(object: &Map<String, Value>) -> String {
    if object
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return "claude.user.meta".to_owned();
    }
    if let Some(origin) = object
        .get("origin")
        .and_then(Value::as_object)
        .and_then(|origin| origin.get("kind").and_then(Value::as_str))
    {
        return format!("claude.user.origin.{origin}");
    }
    "claude.user".to_owned()
}

/// Serializes one Claude tool-result payload into the shared string column shape.
fn tool_result_output(result: &Map<String, Value>) -> Result<String> {
    let content = result.get("content").cloned().unwrap_or(Value::Null);
    if result
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        serde_json::to_string(&json!({
            "is_error": true,
            "content": content,
        }))
        .context("failed to serialize Claude tool error payload")
    } else {
        value_to_text(&content).context("failed to serialize Claude tool payload")
    }
}

/// Serializes one JSON value as plain text when possible and JSON otherwise.
fn value_to_text(value: &Value) -> Result<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        other => serde_json::to_string(other).context("failed to serialize JSON value"),
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, path::Path};

    use anyhow::Result;

    use super::{ClaudeArchivedContext, ClaudeRollout, ClaudeSessionKind, parse_rollout_reader};
    use crate::parse::{CodexTurnMessage, CodexTurnStatus, CodexTurnStep};
    use crate::rollout::ParseDeterminism;

    fn parse_fixture(input: &str, context: &ClaudeArchivedContext) -> Result<ClaudeRollout> {
        parse_rollout_reader(Cursor::new(input), Path::new("fixture.jsonl"), context)
    }

    #[test]
    fn parses_parent_rollout_into_normalized_turns() -> Result<()> {
        let rollout = parse_fixture(
            r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-04-01T00:00:00Z","sessionId":"parent-session"}
{"parentUuid":null,"isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":"Inspect sync.rs"},"uuid":"user-1","timestamp":"2026-04-01T00:00:01Z","permissionMode":"acceptEdits","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"user-1","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-1","type":"message","role":"assistant","content":[{"type":"thinking","thinking":"Check files","signature":"sig"}],"stop_reason":null,"stop_sequence":null},"requestId":"req-1","type":"assistant","uuid":"assistant-msg-1","timestamp":"2026-04-01T00:00:02Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"assistant-msg-1","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-2","type":"message","role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"sync.rs"}}],"stop_reason":"tool_use","stop_sequence":null},"requestId":"req-2","type":"assistant","uuid":"assistant-msg-2","timestamp":"2026-04-01T00:00:03Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"assistant-msg-2","isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":[{"tool_use_id":"tool-1","type":"tool_result","content":"ok","is_error":false}]},"uuid":"user-2","timestamp":"2026-04-01T00:00:04Z","toolUseResult":"ok","sourceToolAssistantUUID":"assistant-msg-2","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"assistant-msg-2","isSidechain":false,"type":"progress","data":{"type":"hook_progress","hookEvent":"PostToolUse","hookName":"PostToolUse:Read","command":"callback"},"parentToolUseID":"tool-1","toolUseID":"hook-1","timestamp":"2026-04-01T00:00:05Z","uuid":"progress-1","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"assistant-msg-2","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-3","type":"message","role":"assistant","content":[{"type":"text","text":"Done."}],"stop_reason":"end_turn","stop_sequence":null},"requestId":"req-3","type":"assistant","uuid":"assistant-msg-3","timestamp":"2026-04-01T00:00:06Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
"#,
            &ClaudeArchivedContext {
                session_id: "parent-session".to_owned(),
                parent_session_id: None,
                session_kind: ClaudeSessionKind::Primary,
                expected_rollout_session_id: "parent-session".to_owned(),
                expected_agent_id: None,
            },
        )?;

        assert_eq!(rollout.session_id, "parent-session");
        assert_eq!(rollout.session_kind, ClaudeSessionKind::Primary);
        assert_eq!(rollout.cwd, Path::new("/tmp/repo"));
        assert_eq!(rollout.cli_version.as_deref(), Some("2.1.87"));
        assert_eq!(rollout.schema_id, "claude.primary_transcript");
        assert_eq!(rollout.determinism, ParseDeterminism::Exact);
        assert_eq!(rollout.turns.len(), 1);
        assert_eq!(rollout.turns[0].turn_id.as_deref(), Some("prompt-1"));
        assert_eq!(rollout.turns[0].user_message, "Inspect sync.rs");
        assert_eq!(rollout.turns[0].status, CodexTurnStatus::Completed);
        assert_eq!(
            rollout.turns[0].final_answer,
            Some(CodexTurnMessage {
                timestamp: "2026-04-01T00:00:06Z".to_owned(),
                text: "Done.".to_owned(),
            })
        );
        assert_eq!(rollout.turns[0].steps.len(), 4);
        assert!(matches!(
            rollout.turns[0].steps[0],
            CodexTurnStep::Reasoning { .. }
        ));
        assert!(matches!(
            rollout.turns[0].steps[1],
            CodexTurnStep::ToolCall { .. }
        ));
        let CodexTurnStep::ToolCallOutput {
            timestamp,
            call_id,
            output,
        } = &rollout.turns[0].steps[2]
        else {
            panic!("expected tool result step");
        };
        assert_eq!(timestamp, "2026-04-01T00:00:04Z");
        assert_eq!(call_id, "tool-1");
        assert_eq!(output, "ok");
        assert!(matches!(
            rollout.turns[0].steps[3],
            CodexTurnStep::ProviderResponseItem { .. }
        ));

        Ok(())
    }

    #[test]
    fn parses_subagent_rollout_as_separate_session() -> Result<()> {
        let rollout = parse_fixture(
            r#"{"parentUuid":null,"isSidechain":true,"promptId":"prompt-1","agentId":"a487e2adbf00a7a09","type":"user","message":{"role":"user","content":"Explore the codebase"},"uuid":"user-1","timestamp":"2026-04-01T00:00:01Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"user-1","isSidechain":true,"agentId":"a487e2adbf00a7a09","message":{"model":"claude-haiku-4-5-20251001","id":"assistant-1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"Bash","input":{"command":"rg todo"}}],"stop_reason":"tool_use","stop_sequence":null},"requestId":"req-1","type":"assistant","uuid":"assistant-1","timestamp":"2026-04-01T00:00:02Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"assistant-1","isSidechain":true,"promptId":"prompt-1","agentId":"a487e2adbf00a7a09","type":"user","message":{"role":"user","content":[{"tool_use_id":"tool-1","type":"tool_result","content":"done","is_error":false}]},"uuid":"user-2","timestamp":"2026-04-01T00:00:03Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"assistant-1","isSidechain":true,"agentId":"a487e2adbf00a7a09","message":{"model":"claude-haiku-4-5-20251001","id":"assistant-2","type":"message","role":"assistant","content":[{"type":"text","text":"Mapped the repo."}],"stop_reason":"end_turn","stop_sequence":null},"requestId":"req-2","type":"assistant","uuid":"assistant-2","timestamp":"2026-04-01T00:00:04Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
"#,
            &ClaudeArchivedContext {
                session_id: "parent-session/subagents/agent-a487e2adbf00a7a09".to_owned(),
                parent_session_id: Some("parent-session".to_owned()),
                session_kind: ClaudeSessionKind::Subagent,
                expected_rollout_session_id: "parent-session".to_owned(),
                expected_agent_id: Some("agent-a487e2adbf00a7a09".to_owned()),
            },
        )?;

        assert_eq!(
            rollout.session_id,
            "parent-session/subagents/agent-a487e2adbf00a7a09"
        );
        assert_eq!(rollout.parent_session_id.as_deref(), Some("parent-session"));
        assert_eq!(rollout.session_kind, ClaudeSessionKind::Subagent);
        assert_eq!(rollout.schema_id, "claude.subagent_transcript");
        assert_eq!(rollout.determinism, ParseDeterminism::Exact);
        assert_eq!(rollout.turns.len(), 1);
        assert_eq!(rollout.turns[0].status, CodexTurnStatus::Completed);

        Ok(())
    }

    #[test]
    fn parses_mixed_tool_result_and_prompt_user_lines_as_two_turns() -> Result<()> {
        let rollout = parse_fixture(
            r#"{"parentUuid":null,"isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":"Inspect the repo"},"uuid":"user-1","timestamp":"2026-04-01T00:00:01Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"user-1","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"README.md"}}],"stop_reason":"tool_use","stop_sequence":null},"requestId":"req-1","type":"assistant","uuid":"assistant-1","timestamp":"2026-04-01T00:00:02Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"assistant-1","isSidechain":false,"promptId":"prompt-2","type":"user","message":{"role":"user","content":[{"tool_use_id":"tool-1","type":"tool_result","content":"done","is_error":false},{"type":"text","text":"Summarize the result"}]},"uuid":"user-2","timestamp":"2026-04-01T00:00:03Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"user-2","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-2","type":"message","role":"assistant","content":[{"type":"text","text":"Summary ready."}],"stop_reason":"end_turn","stop_sequence":null},"requestId":"req-2","type":"assistant","uuid":"assistant-2","timestamp":"2026-04-01T00:00:04Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
"#,
            &ClaudeArchivedContext {
                session_id: "parent-session".to_owned(),
                parent_session_id: None,
                session_kind: ClaudeSessionKind::Primary,
                expected_rollout_session_id: "parent-session".to_owned(),
                expected_agent_id: None,
            },
        )?;

        assert_eq!(rollout.turns.len(), 2);
        assert_eq!(rollout.turns[0].user_message, "Inspect the repo");
        assert_eq!(rollout.turns[0].status, CodexTurnStatus::Incomplete);
        let CodexTurnStep::ToolCallOutput {
            timestamp,
            call_id,
            output,
        } = &rollout.turns[0].steps[1]
        else {
            panic!("expected first-turn tool result step");
        };
        assert_eq!(timestamp, "2026-04-01T00:00:03Z");
        assert_eq!(call_id, "tool-1");
        assert_eq!(output, "done");
        assert_eq!(rollout.turns[1].turn_id.as_deref(), Some("prompt-2"));
        assert_eq!(rollout.turns[1].user_message, "Summarize the result");
        assert_eq!(rollout.turns[1].status, CodexTurnStatus::Completed);
        assert_eq!(
            rollout.turns[1].final_answer,
            Some(CodexTurnMessage {
                timestamp: "2026-04-01T00:00:04Z".to_owned(),
                text: "Summary ready.".to_owned(),
            })
        );

        Ok(())
    }

    #[test]
    fn falls_back_to_best_effort_for_unknown_versions_and_line_types() -> Result<()> {
        let rollout = parse_fixture(
            r#"{"parentUuid":null,"isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":"What changed?"},"uuid":"user-1","timestamp":"2026-04-01T00:00:01Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.9.0","gitBranch":"main"}
{"type":"mystery-event","timestamp":"2026-04-01T00:00:02Z","sessionId":"parent-session","cwd":"/tmp/repo","version":"2.9.0","payload":{"hello":"world"}}
{"parentUuid":"user-1","isSidechain":false,"type":"assistant","uuid":"assistant-1","timestamp":"2026-04-01T00:00:03Z","message":{"id":"synthetic","container":null,"model":"<synthetic>","role":"assistant","stop_reason":"stop_sequence","stop_sequence":"","type":"message","usage":{"input_tokens":0,"output_tokens":0},"content":[{"type":"text","text":"No response requested."}],"context_management":null},"isApiErrorMessage":false,"userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.9.0","gitBranch":"main"}
"#,
            &ClaudeArchivedContext {
                session_id: "parent-session".to_owned(),
                parent_session_id: None,
                session_kind: ClaudeSessionKind::Primary,
                expected_rollout_session_id: "parent-session".to_owned(),
                expected_agent_id: None,
            },
        )?;

        assert_eq!(rollout.determinism, ParseDeterminism::BestEffortForward);
        assert_eq!(rollout.turns.len(), 1);
        assert_eq!(rollout.turns[0].status, CodexTurnStatus::Incomplete);

        Ok(())
    }
}
