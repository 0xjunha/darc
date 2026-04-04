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

mod schema_audit;
mod version;

pub use schema_audit::{
    ClaudeSchemaAuditOptions, ClaudeSchemaAuditOutcome, ClaudeSchemaAuditReport, ClaudeSchemaDrift,
    ClaudeSchemaDriftWindow, ClaudeSchemaSurveyMode, ClaudeSdkSchemaDrift, run_claude_schema_audit,
    run_claude_schema_audit_with_progress,
};
use version::{ClaudeSchemaEpoch, resolve_claude_schema};

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
        self.process_epoch_line(self.current_epoch(), object)
    }

    /// Returns the currently selected Claude schema resolution from observed metadata.
    fn schema_resolution(&self) -> version::ClaudeSchemaResolution {
        resolve_claude_schema(self.cli_version.as_deref())
    }

    /// Returns the currently selected Claude epoch from observed metadata.
    fn current_epoch(&self) -> ClaudeSchemaEpoch {
        self.schema_resolution().epoch
    }

    /// Dispatches one Claude line through the parser rules for the observed epoch.
    fn process_epoch_line(
        &mut self,
        epoch: ClaudeSchemaEpoch,
        object: &Map<String, Value>,
    ) -> Result<()> {
        let line_type = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match line_type {
            "user" => self.process_user_line(object)?,
            "assistant" => self.process_assistant_line(epoch, object)?,
            "progress" => self.process_progress_line(object)?,
            "system" => self.process_system_line(object)?,
            "attachment" if epoch.supports_attachment_line() => {
                self.process_attachment_line(object)?
            }
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

    /// Preserves one Claude `progress` line on the active turn when present.
    fn process_progress_line(&mut self, object: &Map<String, Value>) -> Result<()> {
        if let Some(step) = progress_step(object)? {
            return self.push_step(step);
        }
        self.push_provider_item(
            object
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            progress_item_type(object),
            object.get("data").cloned().unwrap_or(Value::Null),
        )
    }

    /// Preserves one Claude `system` line on the active turn when present.
    fn process_system_line(&mut self, object: &Map<String, Value>) -> Result<()> {
        if let Some(step) = system_step(object)? {
            return self.push_step(step);
        }
        self.push_provider_item(
            object
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            system_item_type(object),
            Value::Object(object.clone()),
        )
    }

    /// Preserves one Claude `attachment` line on the active turn when present.
    fn process_attachment_line(&mut self, object: &Map<String, Value>) -> Result<()> {
        if let Some(step) = attachment_step(object)? {
            self.push_step(step)
        } else {
            self.push_provider_item(
                object
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                "claude.attachment".to_owned(),
                object.get("attachment").cloned().unwrap_or(Value::Null),
            )
        }
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
        let resolution = self.schema_resolution();
        let determinism = if resolution.determinism.is_exact() && !self.best_effort {
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
            schema_id: resolution
                .epoch
                .schema_id(self.context.session_kind)
                .to_owned(),
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
            self.push_tool_results(&timestamp, object, tool_results)?;
        }

        if is_prompt {
            let Some(user_message) = prompt_user_message(&content) else {
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
            self.push_prompt_payload_items(&content)?;
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
    fn process_assistant_line(
        &mut self,
        epoch: ClaudeSchemaEpoch,
        object: &Map<String, Value>,
    ) -> Result<()> {
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

        let mut text_items = Vec::new();
        let mut saw_tool_use = false;
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
                    text_items.push(
                        item_object
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    );
                }
                "tool_use" => {
                    saw_tool_use = true;
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

        if assistant_text_is_terminal(epoch, stop_reason, saw_tool_use) && !text_items.is_empty() {
            let text = text_items.join("\n\n");
            turn.final_answer = Some(CodexTurnMessage {
                timestamp: timestamp.clone(),
                text: text.clone(),
            });
            turn.completed_at = Some(timestamp.clone());
            turn.status = terminal_stop_status(object, stop_reason, &text);
        } else {
            for text in text_items {
                turn.steps.push(CodexTurnStep::Commentary {
                    timestamp: timestamp.clone(),
                    text,
                });
            }
        }

        if turn.final_answer.is_none()
            && (object.get("error").is_some()
                || object
                    .get("isApiErrorMessage")
                    .and_then(Value::as_bool)
                    .unwrap_or(false))
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
        user_line: &Map<String, Value>,
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
            let payload = tool_result_output(result, user_line)?;
            turn.steps.push(CodexTurnStep::ToolCallOutput {
                timestamp: timestamp.to_owned(),
                call_id,
                output: payload,
            });
            if let Some(step) = delegation_result_step(timestamp, user_line, result)? {
                turn.steps.push(step);
            }
        }

        Ok(())
    }

    /// Preserves non-text prompt content items on the newly opened Claude turn.
    fn push_prompt_payload_items(&mut self, content: &Value) -> Result<()> {
        let Some(turn) = self.current_turn.as_mut() else {
            return Ok(());
        };
        let Some(items) = content.as_array() else {
            return Ok(());
        };

        for item in items {
            let Some(object) = item.as_object() else {
                self.best_effort = true;
                continue;
            };
            let item_type = object
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if matches!(item_type, "text" | "tool_result") {
                continue;
            }

            turn.steps.push(CodexTurnStep::ProviderResponseItem {
                timestamp: turn.started_at.clone(),
                item_type: match item_type {
                    "" => "claude.user_content".to_owned(),
                    other => format!("claude.user_content.{other}"),
                },
                payload_json: serde_json::to_string(item)
                    .context("failed to serialize Claude user content item")?,
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

    /// Adds one already-normalized step to the active turn when possible.
    fn push_step(&mut self, step: CodexTurnStep) -> Result<()> {
        let Some(turn) = self.current_turn.as_mut() else {
            return Ok(());
        };
        turn.steps.push(step);
        Ok(())
    }
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
        Value::Array(items) => items.iter().any(|item| !is_tool_result_item(item)),
        _ => false,
    }
}

/// Returns one stable user-message summary for a Claude prompt payload.
fn prompt_user_message(content: &Value) -> Option<String> {
    extract_prompt_text(content)
        .filter(|text| !text.is_empty())
        .or_else(|| summarize_non_text_prompt(content))
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

/// Builds one synthetic user-message summary for prompts that contain no plain text.
fn summarize_non_text_prompt(content: &Value) -> Option<String> {
    let items = content.as_array()?;
    let item_types = items
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|object| object.get("type").and_then(Value::as_str))
        .filter(|item_type| *item_type != "tool_result" && !item_type.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if item_types.is_empty() {
        None
    } else {
        Some(format!("<{} prompt>", item_types.join(" + ")))
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

/// Returns whether one assistant text payload should terminate the current Claude turn.
fn assistant_text_is_terminal(
    epoch: ClaudeSchemaEpoch,
    stop_reason: Option<&str>,
    saw_tool_use: bool,
) -> bool {
    is_terminal_stop_reason(stop_reason) || (epoch.uses_text_completion_fallback() && !saw_tool_use)
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
fn tool_result_output(
    result: &Map<String, Value>,
    user_line: &Map<String, Value>,
) -> Result<String> {
    let content = result.get("content").cloned().unwrap_or(Value::Null);
    if let Some(tool_use_result) = user_line.get("toolUseResult").cloned()
        && (tool_use_result.get("agentId").is_some() || tool_use_result.get("agentType").is_some())
    {
        return serde_json::to_string(&tool_use_result)
            .context("failed to serialize Claude delegated tool payload");
    }
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

/// Normalizes one Claude top-level `attachment` line when the payload is recognized.
fn attachment_step(object: &Map<String, Value>) -> Result<Option<CodexTurnStep>> {
    let Some(attachment) = object.get("attachment").and_then(Value::as_object) else {
        return Ok(None);
    };
    Ok(Some(CodexTurnStep::Attachment {
        timestamp: object
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        attachment_type: attachment
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        payload_json: serde_json::to_string(&Value::Object(attachment.clone()))
            .context("failed to serialize Claude attachment payload")?,
    }))
}

/// Normalizes one Claude `progress` line when it carries stable delegation analytics signals.
fn progress_step(object: &Map<String, Value>) -> Result<Option<CodexTurnStep>> {
    let Some(data) = object.get("data").and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(kind) = data.get("type").and_then(Value::as_str) else {
        return Ok(None);
    };
    if kind != "agent_progress" {
        return Ok(None);
    }

    Ok(Some(CodexTurnStep::Delegation {
        timestamp: object
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        call_id: object
            .get("parentToolUseID")
            .or_else(|| object.get("toolUseID"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        task_id: None,
        event: "agent_progress".to_owned(),
        agent_id: data
            .get("agentId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        agent_type: None,
        status: None,
        summary: data
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        payload_json: serde_json::to_string(&Value::Object(data.clone()))
            .context("failed to serialize Claude agent progress payload")?,
    }))
}

/// Normalizes one Claude `system` line when it carries stable task or hook-summary structure.
fn system_step(object: &Map<String, Value>) -> Result<Option<CodexTurnStep>> {
    let Some(subtype) = object.get("subtype").and_then(Value::as_str) else {
        return Ok(None);
    };

    if matches!(
        subtype,
        "task_started" | "task_progress" | "task_notification"
    ) {
        return Ok(Some(CodexTurnStep::Delegation {
            timestamp: object
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            call_id: object
                .get("tool_use_id")
                .or_else(|| object.get("toolUseID"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            task_id: object
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            event: subtype.to_owned(),
            agent_id: object
                .get("agentId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            agent_type: object
                .get("task_type")
                .and_then(Value::as_str)
                .map(str::to_owned),
            status: object
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned),
            summary: object
                .get("summary")
                .or_else(|| object.get("description"))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned),
            payload_json: serde_json::to_string(&Value::Object(object.clone()))
                .context("failed to serialize Claude task lifecycle payload")?,
        }));
    }

    if subtype == "stop_hook_summary" {
        let hook_count = object
            .get("hookCount")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or_default();
        return Ok(Some(CodexTurnStep::HookSummary {
            timestamp: object
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            call_id: object
                .get("toolUseID")
                .and_then(Value::as_str)
                .map(str::to_owned),
            hook_count,
            prevented_continuation: object
                .get("preventedContinuation")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            has_output: object
                .get("hasOutput")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            level: object
                .get("level")
                .and_then(Value::as_str)
                .map(str::to_owned),
            payload_json: serde_json::to_string(&Value::Object(object.clone()))
                .context("failed to serialize Claude hook summary payload")?,
        }));
    }

    Ok(None)
}

/// Normalizes one delegated Claude tool result into a stable analytics-facing step.
fn delegation_result_step(
    timestamp: &str,
    user_line: &Map<String, Value>,
    result: &Map<String, Value>,
) -> Result<Option<CodexTurnStep>> {
    let Some(tool_use_result) = user_line.get("toolUseResult").and_then(Value::as_object) else {
        return Ok(None);
    };
    if tool_use_result.get("agentId").is_none() && tool_use_result.get("agentType").is_none() {
        return Ok(None);
    }

    Ok(Some(CodexTurnStep::Delegation {
        timestamp: timestamp.to_owned(),
        call_id: result
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        task_id: None,
        event: "completed".to_owned(),
        agent_id: tool_use_result
            .get("agentId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        agent_type: tool_use_result
            .get("agentType")
            .and_then(Value::as_str)
            .map(str::to_owned),
        status: tool_use_result
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned),
        summary: tool_use_result
            .get("summary")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        payload_json: serde_json::to_string(&Value::Object(tool_use_result.clone()))
            .context("failed to serialize Claude delegated tool result payload")?,
    }))
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
    use std::{
        io::Cursor,
        path::{Path, PathBuf},
    };

    use anyhow::Result;
    use serde_json::Value;

    use super::{
        ClaudeArchivedContext, ClaudeRollout, ClaudeSessionKind, parse_rollout_file,
        parse_rollout_reader,
    };
    use crate::parse::{CodexTurnMessage, CodexTurnStatus, CodexTurnStep};
    use crate::rollout::ParseDeterminism;

    fn parse_fixture(input: &str, context: &ClaudeArchivedContext) -> Result<ClaudeRollout> {
        parse_rollout_reader(Cursor::new(input), Path::new("fixture.jsonl"), context)
    }

    fn fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/claude")
            .join(name)
    }

    fn parse_fixture_file(name: &str, context: &ClaudeArchivedContext) -> Result<ClaudeRollout> {
        parse_rollout_file(&fixture_path(name), context)
    }

    fn primary_context() -> ClaudeArchivedContext {
        ClaudeArchivedContext {
            session_id: "parent-session".to_owned(),
            parent_session_id: None,
            session_kind: ClaudeSessionKind::Primary,
            expected_rollout_session_id: "parent-session".to_owned(),
            expected_agent_id: None,
        }
    }

    fn primary_context_for_session(session_id: &str) -> ClaudeArchivedContext {
        ClaudeArchivedContext {
            session_id: session_id.to_owned(),
            parent_session_id: None,
            session_kind: ClaudeSessionKind::Primary,
            expected_rollout_session_id: session_id.to_owned(),
            expected_agent_id: None,
        }
    }

    fn subagent_context(agent_id: &str) -> ClaudeArchivedContext {
        ClaudeArchivedContext {
            session_id: format!("parent-session/subagents/{agent_id}"),
            parent_session_id: Some("parent-session".to_owned()),
            session_kind: ClaudeSessionKind::Subagent,
            expected_rollout_session_id: "parent-session".to_owned(),
            expected_agent_id: Some(agent_id.to_owned()),
        }
    }

    fn representative_epoch_fixture(version: &str, stop_reason: Option<&str>) -> String {
        let stop_reason = stop_reason
            .map(|value| format!("\"{value}\""))
            .unwrap_or_else(|| "null".to_owned());
        format!(
            concat!(
                "{{\"parentUuid\":null,\"isSidechain\":false,\"promptId\":\"prompt-{0}\",\"type\":\"user\",",
                "\"message\":{{\"role\":\"user\",\"content\":\"Inspect {0}\"}},\"uuid\":\"user-{0}\",",
                "\"timestamp\":\"2026-04-01T00:00:01Z\",\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",",
                "\"cwd\":\"/tmp/repo\",\"sessionId\":\"parent-session\",\"version\":\"{0}\",\"gitBranch\":\"HEAD\"}}\n",
                "{{\"parentUuid\":\"user-{0}\",\"isSidechain\":false,\"message\":{{\"model\":\"claude-sonnet-4-6\",",
                "\"id\":\"assistant-{0}\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",",
                "\"text\":\"Reply {0}\"}}],\"stop_reason\":{1},\"stop_sequence\":null}},\"requestId\":\"req-{0}\",",
                "\"type\":\"assistant\",\"uuid\":\"assistant-msg-{0}\",\"timestamp\":\"2026-04-01T00:00:02Z\",",
                "\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",",
                "\"sessionId\":\"parent-session\",\"version\":\"{0}\",\"gitBranch\":\"HEAD\"}}\n"
            ),
            version, stop_reason
        )
    }

    #[test]
    fn extracts_final_answers_for_representative_schema_epochs() -> Result<()> {
        let cases = [
            (
                "1.0.88",
                "claude.primary_transcript.1_0_88_to_2_0_5",
                ParseDeterminism::BestEffortForward,
                None,
            ),
            (
                "2.0.28",
                "claude.primary_transcript.2_0_8_to_2_0_28",
                ParseDeterminism::BestEffortForward,
                None,
            ),
            (
                "2.0.52",
                "claude.primary_transcript.2_0_29_to_2_0_52",
                ParseDeterminism::BestEffortForward,
                None,
            ),
            (
                "2.0.72",
                "claude.primary_transcript.2_0_53_to_2_0_72",
                ParseDeterminism::BestEffortForward,
                None,
            ),
            (
                "2.1.15",
                "claude.primary_transcript.2_0_73_to_2_1_15",
                ParseDeterminism::BestEffortForward,
                None,
            ),
            (
                "2.1.37",
                "claude.primary_transcript.2_1_16_to_2_1_37",
                ParseDeterminism::BestEffortForward,
                None,
            ),
            (
                "2.1.61",
                "claude.primary_transcript.2_1_38_to_2_1_61",
                ParseDeterminism::BestEffortForward,
                None,
            ),
            (
                "2.1.83",
                "claude.primary_transcript.2_1_62_to_2_1_83",
                ParseDeterminism::BestEffortForward,
                None,
            ),
            (
                "2.1.84",
                "claude.primary_transcript.2_1_84_to_2_1_89",
                ParseDeterminism::Exact,
                Some("end_turn"),
            ),
            (
                "2.1.89",
                "claude.primary_transcript.2_1_84_to_2_1_89",
                ParseDeterminism::BestEffortForward,
                Some("end_turn"),
            ),
            (
                "2.1.90",
                "claude.primary_transcript.2_1_90_to_latest",
                ParseDeterminism::BestEffortForward,
                Some("end_turn"),
            ),
        ];

        for (version, expected_schema_id, expected_determinism, stop_reason) in cases {
            let rollout = parse_fixture(
                &representative_epoch_fixture(version, stop_reason),
                &primary_context(),
            )?;

            assert_eq!(rollout.schema_id, expected_schema_id);
            assert_eq!(rollout.determinism, expected_determinism);
            assert_eq!(rollout.turns.len(), 1);
            assert_eq!(
                rollout.turns[0].turn_id.as_deref(),
                Some(format!("prompt-{version}").as_str())
            );
            assert_eq!(rollout.turns[0].user_message, format!("Inspect {version}"));
            assert_eq!(rollout.turns[0].status, CodexTurnStatus::Completed);
            assert_eq!(
                rollout.turns[0].final_answer,
                Some(CodexTurnMessage {
                    timestamp: "2026-04-01T00:00:02Z".to_owned(),
                    text: format!("Reply {version}"),
                })
            );
        }

        Ok(())
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
            &primary_context(),
        )?;

        assert_eq!(rollout.session_id, "parent-session");
        assert_eq!(rollout.session_kind, ClaudeSessionKind::Primary);
        assert_eq!(rollout.cwd, Path::new("/tmp/repo"));
        assert_eq!(rollout.cli_version.as_deref(), Some("2.1.87"));
        assert_eq!(
            rollout.schema_id,
            "claude.primary_transcript.2_1_84_to_2_1_89"
        );
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
            &subagent_context("agent-a487e2adbf00a7a09"),
        )?;

        assert_eq!(
            rollout.session_id,
            "parent-session/subagents/agent-a487e2adbf00a7a09"
        );
        assert_eq!(rollout.parent_session_id.as_deref(), Some("parent-session"));
        assert_eq!(rollout.session_kind, ClaudeSessionKind::Subagent);
        assert_eq!(
            rollout.schema_id,
            "claude.subagent_transcript.2_1_84_to_2_1_89"
        );
        assert_eq!(rollout.determinism, ParseDeterminism::Exact);
        assert_eq!(rollout.turns.len(), 1);
        assert_eq!(rollout.turns[0].status, CodexTurnStatus::Completed);

        Ok(())
    }

    #[test]
    fn preserves_historical_task_delegation_metadata() -> Result<()> {
        let rollout = parse_fixture(
            r##"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-04-01T00:00:00Z","sessionId":"parent-session"}
{"parentUuid":null,"isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":"Delegate README.md"},"uuid":"user-1","timestamp":"2026-04-01T00:00:01Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.0.52","gitBranch":"HEAD"}
{"parentUuid":"user-1","isSidechain":false,"message":{"model":"claude-sonnet-4-5","id":"assistant-1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"Task","input":{"description":"Read README heading","prompt":"Read README.md and return the first heading.","subagent_type":"general-purpose"}}],"stop_reason":null,"stop_sequence":null},"requestId":"req-1","type":"assistant","uuid":"assistant-1","timestamp":"2026-04-01T00:00:02Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.0.52","gitBranch":"HEAD"}
{"parentUuid":"assistant-1","isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":[{"tool_use_id":"tool-1","type":"tool_result","content":[{"type":"text","text":"# Audit Fixture"}]}]},"uuid":"user-2","timestamp":"2026-04-01T00:00:03Z","toolUseResult":{"status":"completed","prompt":"Read README.md and return the first heading.","agentId":"agent-1","content":[{"type":"text","text":"# Audit Fixture"}],"totalDurationMs":12,"totalTokens":34,"totalToolUseCount":1},"userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.0.52","gitBranch":"HEAD"}
{"parentUuid":"user-2","isSidechain":false,"message":{"model":"claude-sonnet-4-5","id":"assistant-2","type":"message","role":"assistant","content":[{"type":"text","text":"# Audit Fixture"}],"stop_reason":null,"stop_sequence":null},"requestId":"req-2","type":"assistant","uuid":"assistant-2","timestamp":"2026-04-01T00:00:04Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.0.52","gitBranch":"HEAD"}
"##,
            &primary_context(),
        )?;

        assert_eq!(
            rollout.schema_id,
            "claude.primary_transcript.2_0_29_to_2_0_52"
        );
        assert_eq!(rollout.determinism, ParseDeterminism::BestEffortForward);
        assert_eq!(rollout.turns.len(), 1);
        assert_eq!(rollout.turns[0].status, CodexTurnStatus::Completed);
        let CodexTurnStep::ToolCall { name, .. } = &rollout.turns[0].steps[0] else {
            panic!("expected task tool call");
        };
        assert_eq!(name, "Task");
        let CodexTurnStep::ToolCallOutput { output, .. } = &rollout.turns[0].steps[1] else {
            panic!("expected task tool output");
        };
        let payload: Value = serde_json::from_str(output)?;
        assert_eq!(payload["agentId"], "agent-1");
        assert_eq!(payload["content"][0]["text"], "# Audit Fixture");
        let CodexTurnStep::Delegation {
            call_id,
            event,
            agent_id,
            ..
        } = &rollout.turns[0].steps[2]
        else {
            panic!("expected normalized delegation step");
        };
        assert_eq!(call_id.as_deref(), Some("tool-1"));
        assert_eq!(event, "completed");
        assert_eq!(agent_id.as_deref(), Some("agent-1"));
        assert_eq!(
            rollout.turns[0].final_answer,
            Some(CodexTurnMessage {
                timestamp: "2026-04-01T00:00:04Z".to_owned(),
                text: "# Audit Fixture".to_owned(),
            })
        );

        Ok(())
    }

    #[test]
    fn preserves_modern_agent_attachments_and_system_events() -> Result<()> {
        let rollout = parse_fixture(
            r##"{"parentUuid":null,"isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":"Delegate README.md"},"uuid":"user-1","timestamp":"2026-04-01T00:00:01Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.90","gitBranch":"HEAD"}
{"parentUuid":"user-1","isSidechain":false,"attachment":{"type":"deferred_tools_delta","addedNames":["Agent"],"addedLines":["Agent"],"removedNames":[]},"type":"attachment","uuid":"attachment-1","timestamp":"2026-04-01T00:00:01Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.90","gitBranch":"HEAD"}
{"parentUuid":"attachment-1","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"Agent","input":{"description":"Read README heading","prompt":"Read README.md and return the first heading.","subagent_type":"general-purpose"}}],"stop_reason":"tool_use","stop_sequence":null},"requestId":"req-1","type":"assistant","uuid":"assistant-1","timestamp":"2026-04-01T00:00:02Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.90","gitBranch":"HEAD"}
{"parentUuid":"assistant-1","isSidechain":false,"type":"system","subtype":"task_started","task_id":"task-1","tool_use_id":"tool-1","description":"Read README heading","task_type":"local_agent","prompt":"Read README.md and return the first heading.","uuid":"system-1","timestamp":"2026-04-01T00:00:03Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.90","gitBranch":"HEAD"}
{"parentUuid":"assistant-1","isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":[{"tool_use_id":"tool-1","type":"tool_result","content":[{"type":"text","text":"# Audit Fixture"},{"type":"text","text":"agentId: agent-1"}]}]},"uuid":"user-2","timestamp":"2026-04-01T00:00:04Z","toolUseResult":{"status":"completed","prompt":"Read README.md and return the first heading.","agentId":"agent-1","agentType":"general-purpose","content":[{"type":"text","text":"# Audit Fixture"}],"totalDurationMs":12,"totalTokens":34,"totalToolUseCount":1},"userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.90","gitBranch":"HEAD"}
{"parentUuid":"user-2","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-2","type":"message","role":"assistant","content":[{"type":"text","text":"# Audit Fixture"}],"stop_reason":"end_turn","stop_sequence":null},"requestId":"req-2","type":"assistant","uuid":"assistant-2","timestamp":"2026-04-01T00:00:05Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.90","gitBranch":"HEAD"}
"##,
            &primary_context(),
        )?;

        assert_eq!(
            rollout.schema_id,
            "claude.primary_transcript.2_1_90_to_latest"
        );
        assert_eq!(rollout.determinism, ParseDeterminism::BestEffortForward);
        assert_eq!(rollout.turns.len(), 1);
        let CodexTurnStep::Attachment {
            attachment_type, ..
        } = &rollout.turns[0].steps[0]
        else {
            panic!("expected attachment item");
        };
        assert_eq!(attachment_type, "deferred_tools_delta");
        let CodexTurnStep::ToolCall { name, .. } = &rollout.turns[0].steps[1] else {
            panic!("expected agent tool call");
        };
        assert_eq!(name, "Agent");
        let CodexTurnStep::Delegation { event, task_id, .. } = &rollout.turns[0].steps[2] else {
            panic!("expected task lifecycle item");
        };
        assert_eq!(event, "task_started");
        assert_eq!(task_id.as_deref(), Some("task-1"));
        let CodexTurnStep::ToolCallOutput { output, .. } = &rollout.turns[0].steps[3] else {
            panic!("expected agent tool output");
        };
        let payload: Value = serde_json::from_str(output)?;
        assert_eq!(payload["agentId"], "agent-1");
        assert_eq!(payload["agentType"], "general-purpose");
        assert_eq!(payload["content"][0]["text"], "# Audit Fixture");
        let CodexTurnStep::Delegation {
            event, agent_id, ..
        } = &rollout.turns[0].steps[4]
        else {
            panic!("expected delegation completion item");
        };
        assert_eq!(event, "completed");
        assert_eq!(agent_id.as_deref(), Some("agent-1"));
        assert_eq!(rollout.turns[0].status, CodexTurnStatus::Completed);

        Ok(())
    }

    #[test]
    fn parses_checked_in_real_fixture_before_attachment_drift() -> Result<()> {
        let rollout = parse_fixture_file(
            "modern/2.1.89-subagent-task-parent.jsonl",
            &primary_context_for_session("session-2-1-89"),
        )?;

        assert_eq!(
            rollout.schema_id,
            "claude.primary_transcript.2_1_84_to_2_1_89"
        );
        assert_eq!(rollout.determinism, ParseDeterminism::BestEffortForward);
        assert_eq!(rollout.turns.len(), 1);
        let turn = &rollout.turns[0];
        assert_eq!(turn.turn_id.as_deref(), Some("prompt-modern-boundary"));
        assert_eq!(
            turn.user_message,
            "You must delegate this work with exactly one Task tool call. Do not use Read yourself. Ask the subagent to inspect README.md and return the first markdown heading, then reply with only that heading."
        );
        assert_eq!(turn.status, CodexTurnStatus::Completed);
        assert_eq!(
            turn.final_answer,
            Some(CodexTurnMessage {
                timestamp: "2026-04-04T04:43:37.420Z".to_owned(),
                text: "# Audit Fixture".to_owned(),
            })
        );
        assert_eq!(turn.steps.len(), 4);
        assert!(matches!(turn.steps[0], CodexTurnStep::Reasoning { .. }));
        let CodexTurnStep::ToolCall {
            call_id,
            name,
            arguments,
            ..
        } = &turn.steps[1]
        else {
            panic!("expected delegated tool call");
        };
        assert_eq!(call_id, "tool-agent");
        assert_eq!(name, "Agent");
        assert!(arguments.contains("\"description\":\"Read README.md first heading\""));
        let CodexTurnStep::ToolCallOutput { output, .. } = &turn.steps[2] else {
            panic!("expected delegated tool output");
        };
        let payload: Value = serde_json::from_str(output)?;
        assert_eq!(payload["agentId"], "delegated-agent-2-1-89");
        assert_eq!(payload["agentType"], "general-purpose");
        assert_eq!(
            payload["content"][0]["text"],
            "The first markdown heading in README.md is:\n\n`# Audit Fixture`"
        );
        let CodexTurnStep::Delegation {
            event, agent_id, ..
        } = &turn.steps[3]
        else {
            panic!("expected delegation completion step");
        };
        assert_eq!(event, "completed");
        assert_eq!(agent_id.as_deref(), Some("delegated-agent-2-1-89"));

        Ok(())
    }

    #[test]
    fn parses_checked_in_real_fixture_after_attachment_drift() -> Result<()> {
        let rollout = parse_fixture_file(
            "modern/2.1.90-subagent-task-parent.jsonl",
            &primary_context_for_session("session-2-1-90"),
        )?;

        assert_eq!(
            rollout.schema_id,
            "claude.primary_transcript.2_1_90_to_latest"
        );
        assert_eq!(rollout.determinism, ParseDeterminism::BestEffortForward);
        assert_eq!(rollout.turns.len(), 1);
        let turn = &rollout.turns[0];
        assert_eq!(turn.turn_id.as_deref(), Some("prompt-modern-boundary"));
        assert_eq!(turn.status, CodexTurnStatus::Completed);
        assert_eq!(
            turn.final_answer,
            Some(CodexTurnMessage {
                timestamp: "2026-04-04T04:44:08.516Z".to_owned(),
                text: "# Audit Fixture".to_owned(),
            })
        );
        assert_eq!(turn.steps.len(), 5);
        let CodexTurnStep::Attachment {
            attachment_type,
            payload_json,
            ..
        } = &turn.steps[0]
        else {
            panic!("expected attachment item");
        };
        assert_eq!(attachment_type, "deferred_tools_delta");
        let attachment: Value = serde_json::from_str(payload_json)?;
        assert_eq!(attachment["type"], "deferred_tools_delta");
        assert_eq!(attachment["addedNames"][0], "AskUserQuestion");
        assert!(matches!(turn.steps[1], CodexTurnStep::Reasoning { .. }));
        let CodexTurnStep::ToolCall { name, .. } = &turn.steps[2] else {
            panic!("expected delegated tool call");
        };
        assert_eq!(name, "Agent");
        let CodexTurnStep::ToolCallOutput { output, .. } = &turn.steps[3] else {
            panic!("expected delegated tool output");
        };
        let payload: Value = serde_json::from_str(output)?;
        assert_eq!(payload["agentId"], "delegated-agent-2-1-90");
        assert_eq!(payload["agentType"], "general-purpose");
        assert_eq!(
            payload["content"][0]["text"],
            "The first markdown heading in README.md is:\n\n`# Audit Fixture`"
        );
        let CodexTurnStep::Delegation {
            event, agent_id, ..
        } = &turn.steps[4]
        else {
            panic!("expected delegation completion step");
        };
        assert_eq!(event, "completed");
        assert_eq!(agent_id.as_deref(), Some("delegated-agent-2-1-90"));

        Ok(())
    }

    #[test]
    fn parses_checked_in_real_historical_epoch_fixtures() -> Result<()> {
        let cases = [
            (
                "historical/1.0.88-read-tool-parent.jsonl",
                "session-1-0-88",
                "claude.primary_transcript.1_0_88_to_2_0_5",
            ),
            (
                "historical/2.0.28-read-tool-parent.jsonl",
                "session-2-0-28",
                "claude.primary_transcript.2_0_8_to_2_0_28",
            ),
            (
                "historical/2.0.52-read-tool-parent.jsonl",
                "session-2-0-52",
                "claude.primary_transcript.2_0_29_to_2_0_52",
            ),
            (
                "historical/2.0.72-read-tool-parent.jsonl",
                "session-2-0-72",
                "claude.primary_transcript.2_0_53_to_2_0_72",
            ),
            (
                "historical/2.1.15-read-tool-parent.jsonl",
                "session-2-1-15",
                "claude.primary_transcript.2_0_73_to_2_1_15",
            ),
            (
                "historical/2.1.37-read-tool-parent.jsonl",
                "session-2-1-37",
                "claude.primary_transcript.2_1_16_to_2_1_37",
            ),
            (
                "historical/2.1.61-read-tool-parent.jsonl",
                "session-2-1-61",
                "claude.primary_transcript.2_1_38_to_2_1_61",
            ),
            (
                "historical/2.1.83-read-tool-parent.jsonl",
                "session-2-1-83",
                "claude.primary_transcript.2_1_62_to_2_1_83",
            ),
        ];

        for (fixture_name, session_id, expected_schema_id) in cases {
            let rollout =
                parse_fixture_file(fixture_name, &primary_context_for_session(session_id))?;

            assert_eq!(rollout.schema_id, expected_schema_id);
            assert_eq!(rollout.determinism, ParseDeterminism::BestEffortForward);
            assert_eq!(rollout.turns.len(), 1);
            let turn = &rollout.turns[0];
            assert_eq!(
                turn.user_message,
                "Use the Read tool exactly once on README.md and then reply with only the first markdown heading."
            );
            assert_eq!(turn.status, CodexTurnStatus::Completed);
            assert_eq!(
                turn.final_answer,
                Some(CodexTurnMessage {
                    timestamp: turn.completed_at.clone().unwrap(),
                    text: "# Audit Fixture".to_owned(),
                })
            );
            assert_eq!(turn.steps.len(), 2);
            let CodexTurnStep::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } = &turn.steps[0]
            else {
                panic!("expected read tool call");
            };
            assert_eq!(call_id, "tool-read");
            assert_eq!(name, "Read");
            assert!(arguments.contains("\"file_path\":\"/tmp/repo/README.md\""));
            let CodexTurnStep::ToolCallOutput {
                call_id, output, ..
            } = &turn.steps[1]
            else {
                panic!("expected read tool output");
            };
            assert_eq!(call_id, "tool-read");
            assert!(output.contains("# Audit Fixture"));
        }

        Ok(())
    }

    #[test]
    fn parses_checked_in_real_subagent_fixture() -> Result<()> {
        let rollout = parse_fixture_file(
            "subagents/2.1.15-read-subagent.jsonl",
            &ClaudeArchivedContext {
                session_id: "session-2-1-15-parent/subagents/agent-2-1-15-sub".to_owned(),
                parent_session_id: Some("session-2-1-15-parent".to_owned()),
                session_kind: ClaudeSessionKind::Subagent,
                expected_rollout_session_id: "session-2-1-15-parent".to_owned(),
                expected_agent_id: Some("agent-2-1-15-sub".to_owned()),
            },
        )?;

        assert_eq!(
            rollout.session_id,
            "session-2-1-15-parent/subagents/agent-2-1-15-sub"
        );
        assert_eq!(
            rollout.parent_session_id.as_deref(),
            Some("session-2-1-15-parent")
        );
        assert_eq!(rollout.session_kind, ClaudeSessionKind::Subagent);
        assert_eq!(
            rollout.schema_id,
            "claude.subagent_transcript.2_0_73_to_2_1_15"
        );
        assert_eq!(rollout.determinism, ParseDeterminism::BestEffortForward);
        assert_eq!(rollout.turns.len(), 1);
        let turn = &rollout.turns[0];
        assert_eq!(
            turn.user_message,
            "Please read the file README.md in the current working directory and return only the first markdown heading (the first line that starts with #). Return just that heading text, nothing else."
        );
        assert_eq!(turn.status, CodexTurnStatus::Completed);
        assert_eq!(
            turn.final_answer,
            Some(CodexTurnMessage {
                timestamp: "2026-04-04T06:39:29.242Z".to_owned(),
                text: "# Audit Fixture".to_owned(),
            })
        );
        assert_eq!(turn.steps.len(), 2);
        let CodexTurnStep::ToolCall { name, .. } = &turn.steps[0] else {
            panic!("expected read tool call");
        };
        assert_eq!(name, "Read");
        let CodexTurnStep::ToolCallOutput { output, .. } = &turn.steps[1] else {
            panic!("expected read tool output");
        };
        assert!(output.contains("# Audit Fixture"));

        Ok(())
    }

    #[test]
    fn parses_checked_in_real_api_error_fixture() -> Result<()> {
        let rollout = parse_fixture_file(
            "errors/2.1.81-auth-error-parent.jsonl",
            &primary_context_for_session("session-2-1-81-auth-error"),
        )?;

        assert_eq!(
            rollout.schema_id,
            "claude.primary_transcript.2_1_62_to_2_1_83"
        );
        assert_eq!(rollout.determinism, ParseDeterminism::Exact);
        assert_eq!(rollout.turns.len(), 1);
        let turn = &rollout.turns[0];
        assert_eq!(turn.turn_id.as_deref(), Some("prompt-auth-error"));
        assert_eq!(
            turn.user_message,
            "What is the timeout value for pending reports?"
        );
        assert_eq!(turn.status, CodexTurnStatus::Incomplete);
        assert_eq!(
            turn.final_answer,
            Some(CodexTurnMessage {
                timestamp: "2026-03-23T04:34:37.641Z".to_owned(),
                text: "Please run /login · API Error: 401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"OAuth token has expired. Please obtain a new token or refresh your existing token.\"},\"request_id\":\"req_auth_error\"}".to_owned(),
            })
        );
        assert!(turn.completed_at.is_some());
        assert!(turn.steps.is_empty());

        Ok(())
    }

    #[test]
    fn normalizes_agent_progress_and_hook_summary_steps() -> Result<()> {
        let rollout = parse_fixture(
            r##"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-04-01T00:00:00Z","sessionId":"parent-session","content":"Delegate README.md"}
{"parentUuid":null,"isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":"Delegate README.md"},"uuid":"user-1","timestamp":"2026-04-01T00:00:01Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.83","gitBranch":"HEAD"}
{"parentUuid":"user-1","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"tool-agent","name":"Agent","input":{"description":"Read README heading","prompt":"Read README.md and return the first heading."}}],"stop_reason":"tool_use","stop_sequence":null},"requestId":"req-1","type":"assistant","uuid":"assistant-1","timestamp":"2026-04-01T00:00:02Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.83","gitBranch":"HEAD"}
{"parentUuid":"assistant-1","isSidechain":false,"type":"progress","data":{"type":"agent_progress","prompt":"Read README.md and return the first heading.","agentId":"delegated-agent-1"},"toolUseID":"agent-msg-1","parentToolUseID":"tool-agent","uuid":"progress-1","timestamp":"2026-04-01T00:00:03Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.83","gitBranch":"HEAD"}
{"parentUuid":"assistant-1","isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":[{"tool_use_id":"tool-agent","type":"tool_result","content":[{"type":"text","text":"# Audit Fixture"}]}]},"uuid":"user-2","timestamp":"2026-04-01T00:00:04Z","toolUseResult":{"status":"completed","prompt":"Read README.md and return the first heading.","agentId":"delegated-agent-1","agentType":"general-purpose","content":[{"type":"text","text":"# Audit Fixture"}],"totalDurationMs":12,"totalTokens":34,"totalToolUseCount":1},"userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.83","gitBranch":"HEAD"}
{"parentUuid":"user-2","isSidechain":false,"type":"system","subtype":"stop_hook_summary","hookCount":2,"hookInfos":[{"command":"callback","durationMs":12}],"hookErrors":[],"preventedContinuation":false,"stopReason":"","hasOutput":true,"level":"suggestion","timestamp":"2026-04-01T00:00:05Z","uuid":"system-1","toolUseID":"tool-agent","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.83","gitBranch":"HEAD"}
{"parentUuid":"user-2","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-2","type":"message","role":"assistant","content":[{"type":"text","text":"# Audit Fixture"}],"stop_reason":"end_turn","stop_sequence":null},"requestId":"req-2","type":"assistant","uuid":"assistant-2","timestamp":"2026-04-01T00:00:06Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.83","gitBranch":"HEAD"}
"##,
            &primary_context(),
        )?;

        let turn = &rollout.turns[0];
        assert_eq!(turn.steps.len(), 5);
        let CodexTurnStep::ToolCall { name, .. } = &turn.steps[0] else {
            panic!("expected agent tool call");
        };
        assert_eq!(name, "Agent");
        let CodexTurnStep::Delegation {
            event,
            call_id,
            agent_id,
            ..
        } = &turn.steps[1]
        else {
            panic!("expected agent progress step");
        };
        assert_eq!(event, "agent_progress");
        assert_eq!(call_id.as_deref(), Some("tool-agent"));
        assert_eq!(agent_id.as_deref(), Some("delegated-agent-1"));
        let CodexTurnStep::ToolCallOutput { call_id, .. } = &turn.steps[2] else {
            panic!("expected tool output step");
        };
        assert_eq!(call_id, "tool-agent");
        let CodexTurnStep::Delegation { event, .. } = &turn.steps[3] else {
            panic!("expected delegated completion step");
        };
        assert_eq!(event, "completed");
        let CodexTurnStep::HookSummary {
            call_id,
            hook_count,
            has_output,
            ..
        } = &turn.steps[4]
        else {
            panic!("expected hook summary step");
        };
        assert_eq!(call_id.as_deref(), Some("tool-agent"));
        assert_eq!(*hook_count, 2);
        assert!(*has_output);

        Ok(())
    }

    #[test]
    fn parses_mixed_tool_result_and_prompt_user_lines_as_two_turns() -> Result<()> {
        let rollout = parse_fixture(
            r#"{"parentUuid":null,"isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":"Inspect the repo"},"uuid":"user-1","timestamp":"2026-04-01T00:00:01Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"user-1","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"README.md"}}],"stop_reason":"tool_use","stop_sequence":null},"requestId":"req-1","type":"assistant","uuid":"assistant-1","timestamp":"2026-04-01T00:00:02Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"assistant-1","isSidechain":false,"promptId":"prompt-2","type":"user","message":{"role":"user","content":[{"tool_use_id":"tool-1","type":"tool_result","content":"done","is_error":false},{"type":"text","text":"Summarize the result"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"abc"}}]},"uuid":"user-2","timestamp":"2026-04-01T00:00:03Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"user-2","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-2","type":"message","role":"assistant","content":[{"type":"text","text":"Summary ready."}],"stop_reason":"end_turn","stop_sequence":null},"requestId":"req-2","type":"assistant","uuid":"assistant-2","timestamp":"2026-04-01T00:00:04Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
"#,
            &primary_context(),
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
        let CodexTurnStep::ProviderResponseItem {
            timestamp,
            item_type,
            payload_json,
        } = &rollout.turns[1].steps[0]
        else {
            panic!("expected preserved prompt payload step");
        };
        assert_eq!(timestamp, "2026-04-01T00:00:03Z");
        assert_eq!(item_type, "claude.user_content.image");
        let payload: Value = serde_json::from_str(payload_json)?;
        assert_eq!(payload["type"], "image");
        assert_eq!(payload["source"]["media_type"], "image/png");
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
    fn preserves_non_text_prompt_content_on_the_new_turn() -> Result<()> {
        let rollout = parse_fixture(
            r#"{"parentUuid":null,"isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":[{"type":"text","text":"Review this screenshot"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"xyz"}}]},"uuid":"user-1","timestamp":"2026-04-01T00:00:01Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"user-1","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-1","type":"message","role":"assistant","content":[{"type":"text","text":"Reviewed."}],"stop_reason":"end_turn","stop_sequence":null},"requestId":"req-1","type":"assistant","uuid":"assistant-1","timestamp":"2026-04-01T00:00:02Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
"#,
            &primary_context(),
        )?;

        assert_eq!(rollout.turns.len(), 1);
        assert_eq!(rollout.turns[0].user_message, "Review this screenshot");
        let CodexTurnStep::ProviderResponseItem {
            timestamp,
            item_type,
            payload_json,
        } = &rollout.turns[0].steps[0]
        else {
            panic!("expected preserved prompt content step");
        };
        assert_eq!(timestamp, "2026-04-01T00:00:01Z");
        assert_eq!(item_type, "claude.user_content.image");
        let payload: Value = serde_json::from_str(payload_json)?;
        assert_eq!(payload["type"], "image");

        Ok(())
    }

    #[test]
    fn opens_a_turn_for_image_only_user_prompts() -> Result<()> {
        let rollout = parse_fixture(
            r#"{"parentUuid":null,"isSidechain":false,"promptId":"prompt-image","type":"user","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"xyz"}}]},"uuid":"user-1","timestamp":"2026-04-01T00:00:01Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"user-1","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-1","type":"message","role":"assistant","content":[{"type":"text","text":"Reviewed image."}],"stop_reason":"end_turn","stop_sequence":null},"requestId":"req-1","type":"assistant","uuid":"assistant-1","timestamp":"2026-04-01T00:00:02Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
"#,
            &primary_context(),
        )?;

        assert_eq!(rollout.turns.len(), 1);
        assert_eq!(rollout.turns[0].turn_id.as_deref(), Some("prompt-image"));
        assert_eq!(rollout.turns[0].user_message, "<image prompt>");
        assert_eq!(rollout.turns[0].status, CodexTurnStatus::Completed);
        let CodexTurnStep::ProviderResponseItem { item_type, .. } = &rollout.turns[0].steps[0]
        else {
            panic!("expected preserved image prompt payload");
        };
        assert_eq!(item_type, "claude.user_content.image");
        assert_eq!(
            rollout.turns[0].final_answer,
            Some(CodexTurnMessage {
                timestamp: "2026-04-01T00:00:02Z".to_owned(),
                text: "Reviewed image.".to_owned(),
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
            &primary_context(),
        )?;

        assert_eq!(rollout.determinism, ParseDeterminism::BestEffortForward);
        assert_eq!(rollout.turns.len(), 1);
        assert_eq!(rollout.turns[0].status, CodexTurnStatus::Incomplete);

        Ok(())
    }
}
