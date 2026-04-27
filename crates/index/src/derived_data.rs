use anyhow::{Context, Result};
use darc_paths::SourceKind;
use darc_rollout::model::NormalizedTurnStep;
use rusqlite::{Connection, params};
use serde_json::{Map, Value};

use crate::{
    evidence::{
        ATTACHMENT_METADATA_FIELD, COMMENTARY_FIELD, DELEGATION_METADATA_FIELD,
        DELEGATION_SUMMARY_FIELD, FINAL_ANSWER_FIELD, HOOK_SUMMARY_FIELD,
        PROVIDER_RESPONSE_ITEM_METADATA_FIELD, REASONING_SUMMARY_FIELD, TOOL_ARGUMENTS_FIELD,
        TOOL_NAME_FIELD, TOOL_OUTPUT_FIELD, USER_MESSAGE_FIELD,
    },
    index_db::schema::{
        DELETE_DERIVED_ANALYTICS_SQL, INSERT_FILE_ACCESS_SQL, INSERT_TOOL_CALL_SQL,
        INSERT_TURN_EVIDENCE_SQL, INSERT_TURN_SEARCH_SQL,
        SELECT_DERIVED_ANALYTICS_REBUILD_ROWS_SQL,
    },
    policy::{build_turn_search_text, derive_file_access_records, extract_tool_call_records},
};

const MAX_USER_MESSAGE_SEARCH_CHARS: usize = 2_048;
const MAX_FINAL_ANSWER_SEARCH_CHARS: usize = 2_048;

/// Stores one canonical text fragment used for exact turn evidence search.
struct TurnEvidenceRecord {
    field: &'static str,
    text: String,
}

/// Stores the canonical turn identity and text needed to derive search analytics rows.
pub(crate) struct TurnDerivedContext<'a> {
    pub(crate) project_id: &'a str,
    pub(crate) provider: SourceKind,
    pub(crate) session_id: &'a str,
    pub(crate) turn_ordinal: i64,
    pub(crate) user_message: &'a str,
    pub(crate) final_answer_text: Option<&'a str>,
}

/// Inserts one turn's derived analytics and search records into SQLite.
pub(crate) fn insert_turn_derived_records(
    connection: &Connection,
    context: &TurnDerivedContext<'_>,
    steps: &[NormalizedTurnStep],
) -> Result<()> {
    let project_id = context.project_id;
    let provider = context.provider;
    let session_id = context.session_id;
    let user_message = context.user_message;
    let final_answer_text = context.final_answer_text;
    let turn_ordinal = context.turn_ordinal;
    let turn_ordinal = u64::try_from(turn_ordinal)
        .context("turn ordinal is negative while inserting analytics")?;
    let tool_calls =
        extract_tool_call_records(project_id, provider, session_id, turn_ordinal, steps);
    let file_accesses = derive_file_access_records(&tool_calls);

    let mut tool_call_statement = connection
        .prepare(INSERT_TOOL_CALL_SQL)
        .context("failed to prepare tool_call insert statement")?;
    for record in &tool_calls {
        tool_call_statement
            .execute(params![
                record.project_id.as_str(),
                record.provider.directory_name(),
                record.session_id.as_str(),
                i64::try_from(record.turn_ordinal)
                    .context("turn ordinal exceeds SQLite INTEGER range")?,
                i64::try_from(record.call_ordinal)
                    .context("call ordinal exceeds SQLite INTEGER range")?,
                record.call_id.as_str(),
                record.timestamp.as_str(),
                record.tool_name.as_deref(),
                record.arguments_text.as_deref(),
                record.output_text.as_deref(),
                record.status.as_deref(),
                i64::from(record.is_error),
            ])
            .with_context(|| {
                format!(
                    "failed to insert derived tool call {} for {}/{session_id}#{turn_ordinal}",
                    record.call_id,
                    provider.directory_name(),
                )
            })?;
    }
    drop(tool_call_statement);

    let mut file_access_statement = connection
        .prepare(INSERT_FILE_ACCESS_SQL)
        .context("failed to prepare file_access insert statement")?;
    for record in &file_accesses {
        file_access_statement
            .execute(params![
                record.project_id.as_str(),
                record.provider.directory_name(),
                record.session_id.as_str(),
                i64::try_from(record.turn_ordinal)
                    .context("turn ordinal exceeds SQLite INTEGER range")?,
                i64::try_from(record.call_ordinal)
                    .context("call ordinal exceeds SQLite INTEGER range")?,
                record.call_id.as_str(),
                record.timestamp.as_str(),
                record.tool_name.as_str(),
                record.access_type.as_sql_text(),
                record.path.as_str(),
                record.repo_relative_path.as_deref(),
                record.file_name.as_deref(),
            ])
            .with_context(|| {
                format!(
                    "failed to insert derived file access {} for {}/{session_id}#{turn_ordinal}",
                    record.path,
                    provider.directory_name(),
                )
            })?;
    }
    drop(file_access_statement);

    let mut evidence_statement = connection
        .prepare(INSERT_TURN_EVIDENCE_SQL)
        .context("failed to prepare turn_evidence insert statement")?;
    for (evidence_ordinal, record) in
        derive_turn_evidence_records(user_message, final_answer_text, steps)
            .into_iter()
            .enumerate()
    {
        evidence_statement
            .execute(params![
                project_id,
                provider.directory_name(),
                session_id,
                i64::try_from(turn_ordinal).context("turn ordinal exceeds SQLite INTEGER range")?,
                i64::try_from(evidence_ordinal)
                    .context("evidence ordinal exceeds SQLite INTEGER range")?,
                record.field,
                record.text.as_str(),
            ])
            .with_context(|| {
                format!(
                    "failed to insert derived turn evidence row for {}/{session_id}#{turn_ordinal}",
                    provider.directory_name(),
                )
            })?;
    }
    drop(evidence_statement);

    let tool_text = build_turn_search_text(steps);
    let user_message_text = normalize_search_text(user_message, MAX_USER_MESSAGE_SEARCH_CHARS);
    let final_answer_text = normalize_search_text(
        final_answer_text.unwrap_or_default(),
        MAX_FINAL_ANSWER_SEARCH_CHARS,
    );
    connection
        .execute(
            INSERT_TURN_SEARCH_SQL,
            params![
                project_id,
                provider.directory_name(),
                session_id,
                i64::try_from(turn_ordinal).context("turn ordinal exceeds SQLite INTEGER range")?,
                user_message_text,
                final_answer_text,
                tool_text,
            ],
        )
        .with_context(|| {
            format!(
                "failed to insert derived turn search row for {}/{session_id}#{turn_ordinal}",
                provider.directory_name(),
            )
        })?;

    Ok(())
}

/// Derives the ordered exact-search evidence rows for one normalized turn.
fn derive_turn_evidence_records(
    user_message: &str,
    final_answer_text: Option<&str>,
    steps: &[NormalizedTurnStep],
) -> Vec<TurnEvidenceRecord> {
    let mut records = Vec::new();
    push_evidence_record(&mut records, USER_MESSAGE_FIELD, user_message);
    if let Some(final_answer_text) = final_answer_text {
        push_evidence_record(&mut records, FINAL_ANSWER_FIELD, final_answer_text);
    }

    for step in steps {
        match step {
            NormalizedTurnStep::ToolCall {
                name, arguments, ..
            } => {
                push_evidence_record(&mut records, TOOL_NAME_FIELD, name);
                push_evidence_record(&mut records, TOOL_ARGUMENTS_FIELD, arguments);
            }
            NormalizedTurnStep::ToolCallOutput { output, .. } => {
                push_evidence_record(&mut records, TOOL_OUTPUT_FIELD, output);
            }
            NormalizedTurnStep::Reasoning { summary, .. } => {
                for summary in summary {
                    push_evidence_record(&mut records, REASONING_SUMMARY_FIELD, summary);
                }
            }
            NormalizedTurnStep::Commentary { text, .. } => {
                push_evidence_record(&mut records, COMMENTARY_FIELD, text);
            }
            NormalizedTurnStep::Attachment {
                attachment_type, ..
            } => {
                let metadata = attachment_metadata_text(attachment_type);
                push_evidence_record(&mut records, ATTACHMENT_METADATA_FIELD, &metadata);
            }
            NormalizedTurnStep::Delegation {
                call_id,
                task_id,
                event,
                agent_id,
                agent_type,
                status,
                summary,
                ..
            } => {
                if let Some(summary) = summary {
                    push_evidence_record(&mut records, DELEGATION_SUMMARY_FIELD, summary);
                }
                let metadata = delegation_metadata_text(
                    call_id.as_deref(),
                    task_id.as_deref(),
                    event,
                    agent_id.as_deref(),
                    agent_type.as_deref(),
                    status.as_deref(),
                );
                push_evidence_record(&mut records, DELEGATION_METADATA_FIELD, &metadata);
            }
            NormalizedTurnStep::HookSummary {
                call_id,
                hook_count,
                prevented_continuation,
                has_output,
                level,
                ..
            } => {
                let metadata = hook_summary_text(
                    call_id.as_deref(),
                    *hook_count,
                    *prevented_continuation,
                    *has_output,
                    level.as_deref(),
                );
                push_evidence_record(&mut records, HOOK_SUMMARY_FIELD, &metadata);
            }
            NormalizedTurnStep::ProviderResponseItem {
                item_type,
                payload_json,
                ..
            } => {
                let metadata = provider_response_item_metadata_text(item_type, payload_json);
                push_evidence_record(
                    &mut records,
                    PROVIDER_RESPONSE_ITEM_METADATA_FIELD,
                    &metadata,
                );
            }
        }
    }

    records
}

/// Pushes one non-empty evidence fragment with its stable field label.
fn push_evidence_record(records: &mut Vec<TurnEvidenceRecord>, field: &'static str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    records.push(TurnEvidenceRecord {
        field,
        text: text.to_owned(),
    });
}

/// Builds compact metadata for one attachment evidence row.
fn attachment_metadata_text(attachment_type: &str) -> String {
    let mut metadata = Map::new();
    insert_string_metadata(&mut metadata, "attachment_type", attachment_type);
    Value::Object(metadata).to_string()
}

/// Builds compact metadata for one delegation evidence row.
fn delegation_metadata_text(
    call_id: Option<&str>,
    task_id: Option<&str>,
    event: &str,
    agent_id: Option<&str>,
    agent_type: Option<&str>,
    status: Option<&str>,
) -> String {
    let mut metadata = Map::new();
    insert_optional_string_metadata(&mut metadata, "call_id", call_id);
    insert_optional_string_metadata(&mut metadata, "task_id", task_id);
    insert_string_metadata(&mut metadata, "event", event);
    insert_optional_string_metadata(&mut metadata, "agent_id", agent_id);
    insert_optional_string_metadata(&mut metadata, "agent_type", agent_type);
    insert_optional_string_metadata(&mut metadata, "status", status);
    Value::Object(metadata).to_string()
}

/// Builds compact metadata for one hook-summary evidence row.
fn hook_summary_text(
    call_id: Option<&str>,
    hook_count: u32,
    prevented_continuation: bool,
    has_output: bool,
    level: Option<&str>,
) -> String {
    let mut metadata = Map::new();
    insert_optional_string_metadata(&mut metadata, "call_id", call_id);
    metadata.insert("hook_count".to_owned(), Value::from(hook_count));
    metadata.insert(
        "prevented_continuation".to_owned(),
        Value::from(prevented_continuation),
    );
    metadata.insert("has_output".to_owned(), Value::from(has_output));
    insert_optional_string_metadata(&mut metadata, "level", level);
    Value::Object(metadata).to_string()
}

/// Builds compact metadata for one provider-response item evidence row.
fn provider_response_item_metadata_text(item_type: &str, payload_json: &str) -> String {
    let mut metadata = Map::new();
    insert_string_metadata(&mut metadata, "item_type", item_type);

    if let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(payload_json) {
        insert_json_scalar_metadata(&mut metadata, "id", payload.get("id"));
        insert_json_scalar_metadata(&mut metadata, "call_id", payload.get("call_id"));
        insert_json_scalar_metadata(&mut metadata, "payload_type", payload.get("type"));
        insert_json_scalar_metadata(&mut metadata, "status", payload.get("status"));
        insert_json_scalar_metadata(&mut metadata, "name", payload.get("name"));
        insert_json_scalar_metadata(&mut metadata, "role", payload.get("role"));
        insert_json_scalar_metadata(&mut metadata, "model", payload.get("model"));
        if let Some(Value::Object(action)) = payload.get("action") {
            insert_json_scalar_metadata(&mut metadata, "action_type", action.get("type"));
        }
    }

    Value::Object(metadata).to_string()
}

/// Inserts one non-empty string metadata value.
fn insert_string_metadata(metadata: &mut Map<String, Value>, key: &str, value: &str) {
    if value.trim().is_empty() {
        return;
    }
    metadata.insert(key.to_owned(), Value::String(value.to_owned()));
}

/// Inserts one optional non-empty string metadata value.
fn insert_optional_string_metadata(
    metadata: &mut Map<String, Value>,
    key: &str,
    value: Option<&str>,
) {
    if let Some(value) = value {
        insert_string_metadata(metadata, key, value);
    }
}

/// Inserts one scalar JSON metadata value while skipping arrays and objects.
fn insert_json_scalar_metadata(
    metadata: &mut Map<String, Value>,
    key: &str,
    value: Option<&Value>,
) {
    match value {
        Some(Value::String(value)) => insert_string_metadata(metadata, key, value),
        Some(Value::Bool(value)) => {
            metadata.insert(key.to_owned(), Value::Bool(*value));
        }
        Some(Value::Number(value)) => {
            metadata.insert(key.to_owned(), Value::Number(value.clone()));
        }
        Some(Value::Null | Value::Array(_) | Value::Object(_)) | None => {}
    }
}

/// Rebuilds every derived analytics and search row from stored turn steps.
pub(crate) fn rebuild_derived_analytics_tables(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(DELETE_DERIVED_ANALYTICS_SQL)
        .context("failed to clear derived analytics tables")?;

    let mut statement = connection
        .prepare(SELECT_DERIVED_ANALYTICS_REBUILD_ROWS_SQL)
        .context("failed to prepare derived analytics rebuild query")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })
        .context("failed to query turns for derived analytics rebuild")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read derived analytics rebuild rows")?;
    drop(statement);

    for (project_id, provider, session_id, turn_ordinal, steps_json, user_message, final_answer) in
        rows
    {
        let provider = parse_provider(&provider)?;
        let steps = serde_json::from_str::<Vec<NormalizedTurnStep>>(&steps_json).with_context(|| {
            format!(
                "failed to parse stored steps_json while rebuilding analytics for {provider:?}/{session_id}#{turn_ordinal}"
            )
        })?;
        insert_turn_derived_records(
            connection,
            &TurnDerivedContext {
                project_id: &project_id,
                provider,
                session_id: &session_id,
                turn_ordinal,
                user_message: &user_message,
                final_answer_text: final_answer.as_deref(),
            },
            &steps,
        )?;
    }

    Ok(())
}
/// Parses one persisted lowercase SQLite provider value back into a source kind.
fn parse_provider(value: &str) -> Result<SourceKind> {
    match value {
        "claude" => Ok(SourceKind::Claude),
        "codex" => Ok(SourceKind::Codex),
        other => anyhow::bail!("unsupported provider `{other}` in index"),
    }
}

/// Normalizes one stored search field into bounded single-line text.
fn normalize_search_text(text: &str, max_chars: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}
