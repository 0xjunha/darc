use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use serde_json::{Map, Value, json};

use super::error::{ClaudeError, Result};
use super::version::{ClaudeSchemaEpoch, ClaudeSchemaResolution, resolve_claude_schema};
use crate::{
    ParseDeterminism,
    model::{
        NormalizedTurn as CodexTurn, NormalizedTurnMessage as CodexTurnMessage,
        NormalizedTurnStatus as CodexTurnStatus, NormalizedTurnStep as CodexTurnStep,
    },
};

/// Identifies whether one archived Claude rollout is a parent session or a subagent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeSessionKind {
    Primary,
    Subagent,
}

impl ClaudeSessionKind {}

/// Stores the archive-derived identity constraints for one Claude rollout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeArchivedContext {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub session_kind: ClaudeSessionKind,
    pub expected_rollout_session_id: String,
    pub expected_agent_id: Option<String>,
}

/// Stores one parsed Claude rollout in the normalized indexing shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeRollout {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub session_kind: ClaudeSessionKind,
    pub cwd: PathBuf,
    pub cli_version: Option<String>,
    pub schema_id: String,
    pub determinism: ParseDeterminism,
    pub turns: Vec<CodexTurn>,
}

/// Parses one archived Claude rollout file into normalized turns.
pub fn parse_rollout_file(path: &Path, context: &ClaudeArchivedContext) -> Result<ClaudeRollout> {
    let file = File::open(path).map_err(|source| ClaudeError::open_file(path, source))?;
    parse_rollout_reader(BufReader::new(file), path, context)
}

/// Parses one Claude rollout reader into the shared normalized indexing model.
pub(crate) fn parse_rollout_reader<R: BufRead>(
    reader: R,
    source_path: &Path,
    context: &ClaudeArchivedContext,
) -> Result<ClaudeRollout> {
    let mut parser = ClaudeRolloutParser::new(source_path, context);
    for (index, line) in reader.lines().enumerate() {
        let line_no = index + 1;
        let line = line.map_err(|source| ClaudeError::read_line(source_path, line_no, source))?;
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
    primary_model: Option<String>,
    direct_request_token_counts: BTreeMap<String, u64>,
    has_direct_token_usage: bool,
    delegated_token_count: u64,
    has_delegated_token_usage: bool,
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
            primary_model: None,
            direct_request_token_counts: BTreeMap::new(),
            has_direct_token_usage: false,
            delegated_token_count: 0,
            has_delegated_token_usage: false,
            steps: Vec::new(),
        }
    }

    /// Records one assistant model name observed during the turn.
    fn observe_model(&mut self, model: Option<&str>) {
        let Some(model) = model.and_then(stable_model_name) else {
            return;
        };
        if self.primary_model.is_none() {
            self.primary_model = Some(model.to_owned());
        }
    }

    /// Records one direct assistant request token total while avoiding duplicate request counts.
    fn observe_direct_request_tokens(&mut self, request_key: String, total_tokens: u64) {
        self.has_direct_token_usage = true;
        let entry = self
            .direct_request_token_counts
            .entry(request_key)
            .or_insert(0);
        *entry = (*entry).max(total_tokens);
    }

    /// Adds one delegated token total reported by the provider.
    fn observe_delegated_tokens(&mut self, total_tokens: u64) {
        self.has_delegated_token_usage = true;
        self.delegated_token_count = self.delegated_token_count.saturating_add(total_tokens);
    }

    /// Finalizes one in-progress turn into the shared persisted model.
    fn finish(self) -> CodexTurn {
        let direct_token_count = self
            .direct_request_token_counts
            .values()
            .copied()
            .fold(0_u64, u64::saturating_add);
        let total_token_count = (self.has_direct_token_usage || self.has_delegated_token_usage)
            .then_some(direct_token_count.saturating_add(self.delegated_token_count));
        CodexTurn {
            turn_id: self.turn_id,
            user_message: self.user_message,
            final_answer: self.final_answer,
            started_at: self.started_at,
            completed_at: self.completed_at,
            status: self.status,
            primary_model: self.primary_model,
            total_token_count,
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
        let value: Value = serde_json::from_str(line)
            .map_err(|source| ClaudeError::parse_json_line(self.source_path, line_no, source))?;
        let object = value
            .as_object()
            .ok_or_else(|| ClaudeError::json_line_not_object(self.source_path, line_no))?;

        self.capture_metadata(object)?;
        self.process_epoch_line(self.current_epoch(), object)
    }

    /// Returns the currently selected Claude schema resolution from observed metadata.
    fn schema_resolution(&self) -> ClaudeSchemaResolution {
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

        let cwd = self
            .cwd
            .clone()
            .ok_or_else(|| ClaudeError::missing_cwd_metadata(self.source_path))?;
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
            return Err(ClaudeError::mismatched_session_id(
                self.source_path,
                &self.context.expected_rollout_session_id,
                raw_session_id,
            ));
        }
        if let Some(expected_agent_id) = &self.context.expected_agent_id
            && let Some(raw_agent_id) = object.get("agentId").and_then(Value::as_str)
            && normalize_agent_id(raw_agent_id) != normalize_agent_id(expected_agent_id)
        {
            return Err(ClaudeError::mismatched_agent_id(
                self.source_path,
                expected_agent_id,
                raw_agent_id,
            ));
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
            .ok_or_else(|| ClaudeError::missing_user_message_object(self.source_path))?;
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
            .ok_or_else(|| ClaudeError::missing_assistant_message_object(self.source_path))?;
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            self.best_effort = true;
            turn.steps.push(CodexTurnStep::ProviderResponseItem {
                timestamp: object
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                item_type: "claude.assistant".to_owned(),
                payload_json: serde_json::to_string(&Value::Object(object.clone())).map_err(
                    |source| ClaudeError::serialize_json("Claude assistant payload", source),
                )?,
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
        turn.observe_model(message.get("model").and_then(Value::as_str));
        if let Some(total_tokens) = assistant_message_total_tokens(message) {
            turn.observe_direct_request_tokens(
                assistant_request_token_key(object, message, &timestamp),
                total_tokens,
            );
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
                    let arguments =
                        value_to_text(item_object.get("input").unwrap_or(&Value::Null))?;
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
                        payload_json: serde_json::to_string(item).map_err(|source| {
                            ClaudeError::serialize_json("Claude assistant content item", source)
                        })?,
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

        if let Some(total_tokens) = delegated_total_tokens(user_line) {
            turn.observe_delegated_tokens(total_tokens);
        }

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
                payload_json: serde_json::to_string(item).map_err(|source| {
                    ClaudeError::serialize_json("Claude user content item", source)
                })?,
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
            payload_json: serde_json::to_string(&payload).map_err(|source| {
                ClaudeError::serialize_json("preserved Claude provider item", source)
            })?,
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

/// Returns one stable assistant request key used to deduplicate cumulative Claude usage rows.
fn assistant_request_token_key(
    object: &Map<String, Value>,
    message: &Map<String, Value>,
    timestamp: &str,
) -> String {
    object
        .get("requestId")
        .and_then(Value::as_str)
        .or_else(|| message.get("id").and_then(Value::as_str))
        .unwrap_or(timestamp)
        .to_owned()
}

/// Returns one best-effort direct Claude assistant token total for a single provider request.
fn assistant_message_total_tokens(message: &Map<String, Value>) -> Option<u64> {
    let usage = message.get("usage").and_then(Value::as_object)?;
    let has_input = usage.contains_key("input_tokens");
    let has_output = usage.contains_key("output_tokens");
    if !has_input && !has_output {
        return None;
    }
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(input_tokens.saturating_add(output_tokens))
}

/// Returns one delegated Claude token total when the user line reports completed subagent usage.
fn delegated_total_tokens(user_line: &Map<String, Value>) -> Option<u64> {
    user_line
        .get("toolUseResult")
        .and_then(Value::as_object)
        .and_then(|payload| payload.get("totalTokens").and_then(Value::as_u64))
}

/// Filters one raw provider model string down to a user-visible stable model name.
fn stable_model_name(value: &str) -> Option<&str> {
    let value = value.trim();
    (!(value.is_empty() || value.starts_with('<') && value.ends_with('>'))).then_some(value)
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
        return serde_json::to_string(&tool_use_result).map_err(|source| {
            ClaudeError::serialize_json("Claude delegated tool payload", source)
        });
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
        .map_err(|source| ClaudeError::serialize_json("Claude tool error payload", source))
    } else {
        value_to_text(&content)
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
            .map_err(|source| ClaudeError::serialize_json("Claude attachment payload", source))?,
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
        payload_json: serde_json::to_string(&Value::Object(data.clone())).map_err(|source| {
            ClaudeError::serialize_json("Claude agent progress payload", source)
        })?,
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
            payload_json: serde_json::to_string(&Value::Object(object.clone())).map_err(
                |source| ClaudeError::serialize_json("Claude task lifecycle payload", source),
            )?,
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
            payload_json: serde_json::to_string(&Value::Object(object.clone())).map_err(
                |source| ClaudeError::serialize_json("Claude hook summary payload", source),
            )?,
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
        payload_json: serde_json::to_string(&Value::Object(tool_use_result.clone())).map_err(
            |source| ClaudeError::serialize_json("Claude delegated tool result payload", source),
        )?,
    }))
}

/// Serializes one JSON value as plain text when possible and JSON otherwise.
fn value_to_text(value: &Value) -> Result<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        other => serde_json::to_string(other)
            .map_err(|source| ClaudeError::serialize_json("JSON value", source)),
    }
}
