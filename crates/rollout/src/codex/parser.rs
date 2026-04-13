use std::{
    cmp::Ordering,
    convert::Infallible,
    error::Error as StdError,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::CodexRolloutHeader;
use super::error::{CodexError, ParseIntoError, ParseIntoResult, Result};
#[cfg(test)]
use super::header::parse_rollout_header_parts;
use super::header::read_rollout_header;
use super::version::{
    CodexCliVersion, CodexSchemaFeature, supports_feature, supports_response_item,
};
use crate::{
    ParseDeterminism,
    model::{
        NormalizedTokenUsage, NormalizedTurn as CodexTurn,
        NormalizedTurnMessage as CodexTurnMessage, NormalizedTurnStatus as CodexTurnStatus,
        NormalizedTurnStep as CodexTurnStep,
    },
};

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

/// Compares duplicate rollout copies by completeness, recency, and a stable path tie-break.
pub fn compare_rollout_priority<T: Ord>(
    left_size: u64,
    left_mtime_ms: u64,
    left_tie_break: &T,
    right_size: u64,
    right_mtime_ms: u64,
    right_tie_break: &T,
) -> Ordering {
    left_size
        .cmp(&right_size)
        .then_with(|| left_mtime_ms.cmp(&right_mtime_ms))
        .then_with(|| left_tie_break.cmp(right_tie_break))
}

/// Receives parsed rollout metadata and completed turns incrementally.
pub trait CodexRolloutSink {
    /// Stores one sink-local error type surfaced by callback implementations.
    type Error: StdError + Send + Sync + 'static;

    /// Starts one parsed rollout session before any turns are emitted.
    fn begin_rollout(
        &mut self,
        header: &CodexRolloutHeader,
    ) -> std::result::Result<(), Self::Error>;

    /// Stores one completed parsed turn.
    fn push_turn(&mut self, turn: CodexTurn) -> std::result::Result<(), Self::Error>;
}

/// Collects parsed rollout data into the in-memory inspect representation.
#[derive(Debug, Default)]
struct CollectingRolloutSink {
    header: Option<CodexRolloutHeader>,
    turns: Vec<CodexTurn>,
}

impl CollectingRolloutSink {
    /// Finishes one collected rollout after parsing completes.
    fn finish(self) -> Result<CodexRollout> {
        let header = self
            .header
            .ok_or_else(CodexError::missing_collected_header)?;
        Ok(CodexRollout {
            session_id: header.session_id,
            cwd: header.cwd,
            cli_version: header.cli_version,
            schema_id: header.schema_id.as_str().to_owned(),
            determinism: header.determinism,
            turns: self.turns,
        })
    }
}

impl CodexRolloutSink for CollectingRolloutSink {
    type Error = Infallible;

    fn begin_rollout(
        &mut self,
        header: &CodexRolloutHeader,
    ) -> std::result::Result<(), Self::Error> {
        self.header = Some(header.clone());
        Ok(())
    }

    fn push_turn(&mut self, turn: CodexTurn) -> std::result::Result<(), Self::Error> {
        self.turns.push(turn);
        Ok(())
    }
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
struct RawEventPayload {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    last_agent_message: Option<String>,
    #[serde(default)]
    info: Option<RawTokenCountInfo>,
}

#[derive(Debug, Deserialize)]
struct RawResponseItemPayload {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<Value>,
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    output: Option<Value>,
    #[serde(default)]
    summary: Vec<Value>,
    #[serde(default)]
    encrypted_content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTokenCountInfo {
    #[serde(default)]
    total_token_usage: Option<RawTokenUsage>,
    #[serde(default)]
    last_token_usage: Option<RawTokenUsage>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct RawTokenUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    cached_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    reasoning_output_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawMessageContent {
    #[serde(default)]
    text: Option<String>,
}

/// Parses one Codex rollout file into user-visible turns with schema metadata.
pub fn parse_rollout_file(path: &Path) -> Result<CodexRollout> {
    let mut sink = CollectingRolloutSink::default();
    parse_rollout_file_into(path, &mut sink)?;
    sink.finish()
}

/// Parses one Codex rollout file and emits turns incrementally to a sink.
pub fn parse_rollout_file_into<S: CodexRolloutSink>(
    path: &Path,
    sink: &mut S,
) -> ParseIntoResult<(), S::Error> {
    let header =
        read_rollout_header(path)?.ok_or_else(|| CodexError::missing_session_meta_line(path))?;
    let has_event_user_boundaries = scan_rollout_for_event_user_boundaries(path)?;
    let file = File::open(path).map_err(|source| CodexError::open_file(path, source))?;
    let reader = BufReader::new(file);

    parse_rollout_stream(reader, path, header, has_event_user_boundaries, sink)
}

/// Parses one Codex rollout reader into user-visible turns with version-based schema dispatch.
#[cfg(test)]
pub(crate) fn parse_rollout_reader<R: BufRead>(
    reader: R,
    source_path: &Path,
) -> Result<CodexRollout> {
    let raw_lines = read_raw_lines(reader)?;
    let header = parse_rollout_header_from_lines(&raw_lines, source_path)?;
    let has_event_user_boundaries = raw_lines
        .iter()
        .any(|line| raw_line_has_event_user_boundary(&line.line));
    let mut sink = CollectingRolloutSink::default();
    parse_rollout_lines(
        raw_lines,
        source_path,
        header,
        has_event_user_boundaries,
        &mut sink,
    )?;
    sink.finish()
}

/// Parses the strict rollout header from one buffered set of raw JSONL lines.
#[cfg(test)]
fn parse_rollout_header_from_lines(
    raw_lines: &[NumberedRawLine],
    source_path: &Path,
) -> Result<CodexRolloutHeader> {
    let first = raw_lines
        .first()
        .ok_or_else(|| CodexError::missing_session_meta_line(source_path))?;
    parse_rollout_header_parts(&first.line.kind, first.line.payload.clone(), source_path)?
        .ok_or_else(|| CodexError::missing_session_meta_line(source_path))
}

/// Replays buffered rollout lines through the standard streaming parser.
#[cfg(test)]
fn parse_rollout_lines<S: CodexRolloutSink>(
    raw_lines: Vec<NumberedRawLine>,
    source_path: &Path,
    header: CodexRolloutHeader,
    has_event_user_boundaries: bool,
    sink: &mut S,
) -> ParseIntoResult<(), S::Error> {
    let mut parser = RolloutLineParser::new(source_path, header, has_event_user_boundaries, sink)?;
    for numbered_line in raw_lines {
        parser.process_line(numbered_line)?;
    }
    parser.finish()
}

/// Streams one rollout reader into a sink without buffering the whole file.
fn parse_rollout_stream<R: BufRead, S: CodexRolloutSink>(
    reader: R,
    source_path: &Path,
    header: CodexRolloutHeader,
    has_event_user_boundaries: bool,
    sink: &mut S,
) -> ParseIntoResult<(), S::Error> {
    let mut parser = RolloutLineParser::new(source_path, header, has_event_user_boundaries, sink)?;

    for (index, line) in reader.lines().enumerate() {
        let line_no = index + 1;
        let line = line.map_err(|source| CodexError::read_line(source_path, line_no, source))?;
        if line.trim().is_empty() {
            continue;
        }
        let raw: RawLine = serde_json::from_str(&line).map_err(|source| {
            CodexError::deserialize_json_line(source_path, line_no, "JSONL line", source)
        })?;
        parser.process_line(NumberedRawLine { line_no, line: raw })?;
    }

    parser.finish()
}

/// Tracks parser state while streaming one rollout into a sink.
struct RolloutLineParser<'a, S> {
    source_path: &'a Path,
    header: CodexRolloutHeader,
    cli_version: CodexCliVersion,
    has_event_user_boundaries: bool,
    pending_turn_id: Option<String>,
    pending_turn_model: Option<String>,
    pending_turn_token_usage: NormalizedTokenUsage,
    pending_turn_has_token_usage: bool,
    last_cumulative_token_usage: Option<RawTokenUsage>,
    current_turn: Option<CodexTurn>,
    sink: &'a mut S,
}

impl<'a, S: CodexRolloutSink> RolloutLineParser<'a, S> {
    /// Creates one rollout parser and initializes the sink with session metadata.
    fn new(
        source_path: &'a Path,
        header: CodexRolloutHeader,
        has_event_user_boundaries: bool,
        sink: &'a mut S,
    ) -> ParseIntoResult<Self, S::Error> {
        let cli_version = CodexCliVersion::parse(&header.cli_version).map_err(|source| {
            CodexError::parse_cli_version(source_path, &header.cli_version, source)
        })?;
        sink.begin_rollout(&header).map_err(ParseIntoError::Sink)?;

        Ok(Self {
            source_path,
            header,
            cli_version,
            has_event_user_boundaries,
            pending_turn_id: None,
            pending_turn_model: None,
            pending_turn_token_usage: NormalizedTokenUsage::default(),
            pending_turn_has_token_usage: false,
            last_cumulative_token_usage: None,
            current_turn: None,
            sink,
        })
    }

    /// Applies one raw rollout line to the current parser state.
    fn process_line(&mut self, numbered_line: NumberedRawLine) -> ParseIntoResult<(), S::Error> {
        let line_no = numbered_line.line_no;
        let RawLine {
            timestamp,
            kind,
            payload,
        } = numbered_line.line;

        match kind.as_str() {
            "session_meta" => {}
            "event_msg" => {
                let event: RawEventPayload = serde_json::from_value(payload).map_err(|source| {
                    CodexError::deserialize_json_line(
                        self.source_path,
                        line_no,
                        "event_msg",
                        source,
                    )
                })?;
                match event.kind.as_str() {
                    "task_started" | "turn_started" => {
                        ensure_feature_support(
                            &self.cli_version,
                            CodexSchemaFeature::TaskLifecycleEvents,
                            self.header.determinism,
                            self.source_path,
                            line_no,
                            "task lifecycle events",
                        )?;
                        self.pending_turn_id = event.turn_id;
                        self.pending_turn_model = None;
                        self.pending_turn_token_usage = NormalizedTokenUsage::default();
                        self.pending_turn_has_token_usage = false;
                    }
                    "user_message" => {
                        if let Some(message) = event.message {
                            self.close_open_turn()?;
                            self.current_turn = Some(self.start_turn(timestamp, message));
                        }
                    }
                    "token_count" => {
                        if let Some(delta) = token_count_delta(
                            event.info.as_ref(),
                            &mut self.last_cumulative_token_usage,
                        ) {
                            self.observe_turn_token_usage(delta);
                        }
                    }
                    "task_complete" | "turn_complete" => {
                        ensure_feature_support(
                            &self.cli_version,
                            CodexSchemaFeature::TaskLifecycleEvents,
                            self.header.determinism,
                            self.source_path,
                            line_no,
                            "task lifecycle events",
                        )?;
                        if let Some(mut turn) = self.current_turn.take() {
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
                            self.sink.push_turn(turn).map_err(ParseIntoError::Sink)?;
                        }
                    }
                    "turn_aborted" => {
                        if let Some(mut turn) = self.current_turn.take() {
                            turn.completed_at = Some(timestamp);
                            turn.status = CodexTurnStatus::Aborted;
                            self.sink.push_turn(turn).map_err(ParseIntoError::Sink)?;
                        }
                    }
                    _ => {}
                }
            }
            "response_item" => {
                let raw_payload_json = serde_json::to_string(&payload).map_err(|source| {
                    CodexError::serialize_json_line(
                        self.source_path,
                        line_no,
                        "response_item",
                        source,
                    )
                })?;
                let item: RawResponseItemPayload =
                    serde_json::from_value(payload).map_err(|source| {
                        CodexError::deserialize_json_line(
                            self.source_path,
                            line_no,
                            "response_item",
                            source,
                        )
                    })?;
                if self.header.determinism.is_exact()
                    && !supports_response_item(&self.cli_version, &item.kind)
                {
                    return Err(ParseIntoError::Parse(
                        CodexError::unsupported_response_item(
                            self.source_path,
                            line_no,
                            &item.kind,
                            &self.header.cli_version,
                        ),
                    ));
                }
                match item.kind.as_str() {
                    "message" => {
                        if item.phase.is_some() {
                            ensure_feature_support(
                                &self.cli_version,
                                CodexSchemaFeature::MessagePhase,
                                self.header.determinism,
                                self.source_path,
                                line_no,
                                "message phases",
                            )?;
                        }
                        let Some(role) = item.role else {
                            return Ok(());
                        };
                        let Some(text) = message_text(
                            item.content,
                            self.source_path,
                            line_no,
                            self.header.determinism,
                        )?
                        else {
                            return Ok(());
                        };

                        if role == "user" {
                            if !self.has_event_user_boundaries && !is_user_boilerplate(&text) {
                                self.close_open_turn()?;
                                self.current_turn = Some(self.start_turn(timestamp, text));
                            }
                            return Ok(());
                        }

                        if let Some(turn) = self.current_turn.as_mut()
                            && role == "assistant"
                        {
                            record_assistant_message(turn, timestamp, item.phase.as_deref(), text);
                        }
                    }
                    "function_call" => {
                        let Some(call_id) = item.call_id else {
                            return Ok(());
                        };
                        let Some(name) = item.name else {
                            return Ok(());
                        };
                        let Some(arguments) = string_field(
                            item.arguments,
                            self.source_path,
                            line_no,
                            self.header.determinism,
                            "function_call.arguments",
                        )?
                        else {
                            return Ok(());
                        };
                        if let Some(turn) = self.current_turn.as_mut() {
                            turn.steps.push(CodexTurnStep::ToolCall {
                                timestamp,
                                call_id,
                                name,
                                arguments,
                            });
                        }
                    }
                    "custom_tool_call" => {
                        let Some(call_id) = item.call_id else {
                            return Ok(());
                        };
                        let Some(name) = item.name else {
                            return Ok(());
                        };
                        let Some(arguments) = string_field(
                            item.input,
                            self.source_path,
                            line_no,
                            self.header.determinism,
                            "custom_tool_call.input",
                        )?
                        else {
                            return Ok(());
                        };
                        if let Some(turn) = self.current_turn.as_mut() {
                            turn.steps.push(CodexTurnStep::ToolCall {
                                timestamp,
                                call_id,
                                name,
                                arguments,
                            });
                        }
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let Some(call_id) = item.call_id else {
                            return Ok(());
                        };
                        let Some(output) = output_field(
                            item.output,
                            self.source_path,
                            line_no,
                            &self.cli_version,
                            self.header.determinism,
                        )?
                        else {
                            return Ok(());
                        };
                        if let Some(turn) = self.current_turn.as_mut() {
                            turn.steps.push(CodexTurnStep::ToolCallOutput {
                                timestamp,
                                call_id,
                                output,
                            });
                        }
                    }
                    "reasoning" => {
                        if let Some(turn) = self.current_turn.as_mut() {
                            turn.steps.push(CodexTurnStep::Reasoning {
                                timestamp,
                                summary: reasoning_summary(item.summary),
                                encrypted: item.encrypted_content.is_some(),
                            });
                        }
                    }
                    _ => {
                        if let Some(turn) = self.current_turn.as_mut() {
                            turn.steps.push(CodexTurnStep::ProviderResponseItem {
                                timestamp,
                                item_type: item.kind,
                                payload_json: raw_payload_json,
                            });
                        }
                    }
                }
            }
            "turn_context" => ensure_feature_support(
                &self.cli_version,
                CodexSchemaFeature::TurnContextLine,
                self.header.determinism,
                self.source_path,
                line_no,
                "turn_context lines",
            )
            .map(|()| {
                if let Some(model) = turn_context_model(&payload) {
                    self.observe_turn_model(model);
                }
            })?,
            "compacted" => ensure_feature_support(
                &self.cli_version,
                CodexSchemaFeature::CompactedLine,
                self.header.determinism,
                self.source_path,
                line_no,
                "compacted lines",
            )?,
            _ => {
                if self.header.determinism.is_exact() {
                    return Err(ParseIntoError::Parse(CodexError::unsupported_rollout_item(
                        self.source_path,
                        line_no,
                        &kind,
                        self.header.schema_id.as_str(),
                    )));
                }
            }
        }

        Ok(())
    }

    /// Flushes any unfinished turn after all rollout lines have been processed.
    fn finish(mut self) -> ParseIntoResult<(), S::Error> {
        self.close_open_turn()
    }

    /// Starts one new turn and applies any already observed pending metadata.
    fn start_turn(&mut self, timestamp: String, user_message: String) -> CodexTurn {
        let mut turn = start_turn(timestamp, self.pending_turn_id.take(), user_message);
        if let Some(model) = self.pending_turn_model.take() {
            set_turn_primary_model(&mut turn, model);
        }
        if self.pending_turn_has_token_usage {
            add_turn_token_usage(&mut turn, self.pending_turn_token_usage);
            self.pending_turn_has_token_usage = false;
            self.pending_turn_token_usage = NormalizedTokenUsage::default();
        }
        turn
    }

    /// Closes the current turn and emits it to the sink when present.
    fn close_open_turn(&mut self) -> ParseIntoResult<(), S::Error> {
        if let Some(turn) = self.current_turn.take() {
            self.sink
                .push_turn(normalize_turn(turn))
                .map_err(ParseIntoError::Sink)?;
        }
        Ok(())
    }

    /// Records one observed Codex model name on the pending or active turn.
    fn observe_turn_model(&mut self, model: String) {
        if let Some(turn) = self.current_turn.as_mut() {
            set_turn_primary_model(turn, model);
        } else {
            self.pending_turn_model = Some(model);
        }
    }

    /// Adds one observed Codex token delta to the pending or active turn.
    fn observe_turn_token_usage(&mut self, delta: NormalizedTokenUsage) {
        if let Some(turn) = self.current_turn.as_mut() {
            add_turn_token_usage(turn, delta);
            return;
        }
        if self.pending_turn_id.is_some() || self.pending_turn_model.is_some() {
            self.pending_turn_has_token_usage = true;
            self.pending_turn_token_usage.saturating_add_assign(delta);
        }
    }
}

/// Scans one rollout file to decide whether event-based user boundaries are present.
fn scan_rollout_for_event_user_boundaries(path: &Path) -> Result<bool> {
    let file = File::open(path).map_err(|source| CodexError::open_file(path, source))?;

    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line_no = index + 1;
        let line = line.map_err(|source| CodexError::read_line(path, line_no, source))?;
        if line.trim().is_empty() {
            continue;
        }
        let raw: RawLine = serde_json::from_str(&line).map_err(|source| {
            CodexError::deserialize_json_line(path, line_no, "JSONL line", source)
        })?;
        if raw_line_has_event_user_boundary(&raw) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Returns whether one raw line is an event-based user-message boundary.
fn raw_line_has_event_user_boundary(raw_line: &RawLine) -> bool {
    raw_line.kind == "event_msg"
        && raw_line.payload.get("type").and_then(Value::as_str) == Some("user_message")
}

fn ensure_feature_support(
    cli_version: &CodexCliVersion,
    schema_feature: CodexSchemaFeature,
    determinism: ParseDeterminism,
    source_path: &Path,
    line_no: usize,
    feature_name: &'static str,
) -> Result<()> {
    if supports_feature(cli_version, schema_feature) || !determinism.is_exact() {
        return Ok(());
    }
    Err(CodexError::unsupported_feature(
        source_path,
        line_no,
        feature_name,
    ))
}

/// Buffers one rollout reader into numbered raw lines for shared parse entry points.
#[cfg(test)]
fn read_raw_lines<R: BufRead>(reader: R) -> Result<Vec<NumberedRawLine>> {
    let mut raw_lines = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line_no = index + 1;
        let line =
            line.map_err(|source| CodexError::read_line(Path::new("<reader>"), line_no, source))?;
        if line.trim().is_empty() {
            continue;
        }
        let raw: RawLine = serde_json::from_str(&line).map_err(|source| {
            CodexError::deserialize_json_line(Path::new("<reader>"), line_no, "JSONL line", source)
        })?;
        raw_lines.push(NumberedRawLine { line_no, line: raw });
    }
    Ok(raw_lines)
}

fn start_turn(timestamp: String, turn_id: Option<String>, user_message: String) -> CodexTurn {
    CodexTurn {
        turn_id,
        user_message,
        final_answer: None,
        started_at: timestamp,
        completed_at: None,
        status: CodexTurnStatus::Incomplete,
        primary_model: None,
        token_usage: None,
        steps: Vec::new(),
    }
}

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

fn turn_has_final_answer(turn: &CodexTurn) -> bool {
    turn.final_answer.is_some()
}

fn final_answer_timestamp(turn: &CodexTurn) -> Option<&str> {
    turn.final_answer
        .as_ref()
        .map(|answer| answer.timestamp.as_str())
}

fn record_assistant_message(
    turn: &mut CodexTurn,
    timestamp: String,
    phase: Option<&str>,
    text: String,
) {
    match phase {
        Some("commentary") => {
            turn.steps
                .push(CodexTurnStep::Commentary { timestamp, text });
        }
        Some("final_answer") => {
            turn.final_answer = Some(CodexTurnMessage { timestamp, text });
        }
        None => {
            turn.final_answer = Some(CodexTurnMessage { timestamp, text });
        }
        Some(_) => {}
    }
}

/// Returns one best-effort normalized token delta from a Codex `token_count` event.
fn token_count_delta(
    info: Option<&RawTokenCountInfo>,
    last_cumulative_token_usage: &mut Option<RawTokenUsage>,
) -> Option<NormalizedTokenUsage> {
    let info = info?;
    let current_usage = *info.total_token_usage.as_ref()?;
    let previous_usage = *last_cumulative_token_usage;
    let last_usage = info.last_token_usage.as_ref().copied().unwrap_or_default();

    let input_total_delta = token_counter_delta(
        current_usage.input_tokens,
        previous_usage.and_then(|usage| usage.input_tokens),
        last_usage.input_tokens,
    );
    let cache_read_delta = token_counter_delta(
        current_usage.cached_input_tokens,
        previous_usage.and_then(|usage| usage.cached_input_tokens),
        last_usage.cached_input_tokens,
    );
    let output_delta = token_counter_delta(
        current_usage.output_tokens,
        previous_usage.and_then(|usage| usage.output_tokens),
        last_usage.output_tokens,
    );
    let reasoning_delta = token_counter_delta(
        current_usage.reasoning_output_tokens,
        previous_usage.and_then(|usage| usage.reasoning_output_tokens),
        last_usage.reasoning_output_tokens,
    );
    let provider_total_delta = token_counter_delta(
        current_usage.total_tokens,
        previous_usage.and_then(|usage| usage.total_tokens),
        last_usage.total_tokens,
    );

    *last_cumulative_token_usage = Some(current_usage);

    let input_uncached_delta = input_total_delta
        .map(|input_total| input_total.saturating_sub(cache_read_delta.unwrap_or(0)));
    let normalized_total_delta = match (input_total_delta, output_delta) {
        (Some(input_total), Some(output_total)) => Some(input_total.saturating_add(output_total)),
        _ => provider_total_delta,
    };
    let token_usage = NormalizedTokenUsage {
        input_uncached_token_count: input_uncached_delta,
        cache_read_token_count: cache_read_delta,
        cache_write_token_count: None,
        output_token_count: output_delta,
        reasoning_token_count: reasoning_delta,
        provider_total_token_count: provider_total_delta,
        normalized_total_token_count: normalized_total_delta,
    };
    token_usage.has_any_value().then_some(token_usage)
}

/// Returns one best-effort counter delta from cumulative and last-request token rows.
fn token_counter_delta(
    current_total: Option<u64>,
    previous_total: Option<u64>,
    last_total: Option<u64>,
) -> Option<u64> {
    match (current_total, previous_total) {
        (Some(current_total), Some(previous_total)) if current_total >= previous_total => {
            Some(current_total - previous_total)
        }
        (Some(current_total), _) => Some(last_total.unwrap_or(current_total)),
        (None, _) => last_total,
    }
}

/// Extracts one stable model name from a Codex `turn_context` payload.
fn turn_context_model(payload: &Value) -> Option<String> {
    payload
        .as_object()
        .and_then(|object| object.get("model").and_then(Value::as_str))
        .and_then(stable_model_name)
        .map(str::to_owned)
}

/// Sets the primary model on one normalized turn when it is still unknown.
fn set_turn_primary_model(turn: &mut CodexTurn, model: String) {
    if turn.primary_model.is_none() {
        turn.primary_model = Some(model);
    }
}

/// Adds one token-usage delta to one normalized turn using saturating arithmetic.
fn add_turn_token_usage(turn: &mut CodexTurn, delta: NormalizedTokenUsage) {
    let token_usage = turn
        .token_usage
        .get_or_insert_with(NormalizedTokenUsage::default);
    token_usage.saturating_add_assign(delta);
}

/// Filters one raw provider model string down to a user-visible stable model name.
fn stable_model_name(value: &str) -> Option<&str> {
    let value = value.trim();
    (!(value.is_empty() || value.starts_with('<') && value.ends_with('>'))).then_some(value)
}

fn message_text(
    content: Option<Value>,
    source_path: &Path,
    line_no: usize,
    determinism: ParseDeterminism,
) -> Result<Option<String>> {
    let Some(content) = content else {
        return Ok(None);
    };

    let parts = match content {
        Value::Null => return Ok(None),
        Value::Array(_) => {
            serde_json::from_value::<Vec<RawMessageContent>>(content).map_err(|source| {
                CodexError::deserialize_json_line(
                    source_path,
                    line_no,
                    "response_item content",
                    source,
                )
            })?
        }
        Value::Object(_) => vec![
            serde_json::from_value::<RawMessageContent>(content).map_err(|source| {
                CodexError::deserialize_json_line(
                    source_path,
                    line_no,
                    "response_item content",
                    source,
                )
            })?,
        ],
        Value::String(text) if !determinism.is_exact() => {
            return Ok(non_empty_text(Some(text)));
        }
        _ => {
            return Err(CodexError::unsupported_message_content_shape(
                source_path,
                line_no,
            ));
        }
    };

    let text: Vec<String> = parts.into_iter().filter_map(|part| part.text).collect();
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(text.join("\n")))
}

fn string_field(
    value: Option<Value>,
    source_path: &Path,
    line_no: usize,
    determinism: ParseDeterminism,
    field_name: &'static str,
) -> Result<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(non_empty_text(Some(text))),
        Some(other) if !determinism.is_exact() => Ok(serde_json::to_string(&other)
            .ok()
            .and_then(|text| non_empty_text(Some(text)))),
        Some(_) => Err(CodexError::unsupported_field_shape(
            source_path,
            line_no,
            field_name,
        )),
    }
}

fn output_field(
    value: Option<Value>,
    source_path: &Path,
    line_no: usize,
    cli_version: &CodexCliVersion,
    determinism: ParseDeterminism,
) -> Result<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(non_empty_text(Some(text))),
        Some(Value::Array(items))
            if supports_feature(cli_version, CodexSchemaFeature::StructuredToolOutput)
                || !determinism.is_exact() =>
        {
            Ok(serde_json::to_string(&items)
                .ok()
                .and_then(|text| non_empty_text(Some(text))))
        }
        Some(other) if !determinism.is_exact() => Ok(serde_json::to_string(&other)
            .ok()
            .and_then(|text| non_empty_text(Some(text)))),
        Some(_) => Err(CodexError::unsupported_tool_output_shape(
            source_path,
            line_no,
        )),
    }
}

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

fn non_empty_text(text: Option<String>) -> Option<String> {
    let text = text?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.to_owned())
}

fn is_user_boilerplate(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.is_empty()
        || trimmed.starts_with("# AGENTS.md instructions for ")
        || trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<turn_aborted>")
}
