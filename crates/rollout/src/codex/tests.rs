use std::{io::Cursor, path::Path};

use anyhow::Result;

use super::parser::parse_rollout_reader;
use crate::ParseDeterminism;
use crate::model::{
    NormalizedTurnMessage as CodexTurnMessage, NormalizedTurnStatus as CodexTurnStatus,
    NormalizedTurnStep as CodexTurnStep,
};

#[test]
fn parses_turn_lifecycle_rollout_and_records_schema_metadata() -> Result<()> {
    let rollout = parse_rollout_reader(
        Cursor::new(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture","cwd":"/tmp/repo","cli_version":"0.128.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1","started_at":1767225601}}
{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"model_verification","verifications":[]}}
{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"Inspect repo"}}
{"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Reading"}]}}
{"timestamp":"2026-01-01T00:00:04Z","type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"exec_command","arguments":"{\"cmd\":\"ls\"}"}}
{"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":[{"type":"input_image","image_url":"data:image/png;base64,abc"}]}}
{"timestamp":"2026-01-01T00:00:06Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Done"}]}}
{"timestamp":"2026-01-01T00:00:07Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","completed_at":1767225607,"duration_ms":6000,"time_to_first_token_ms":2000}}
"#,
        ),
        Path::new("fixture.jsonl"),
    )?;

    assert_eq!(rollout.cli_version, "0.128.0");
    assert_eq!(rollout.schema_id, "codex.turn_lifecycle");
    assert_eq!(rollout.determinism, ParseDeterminism::Exact);
    assert_eq!(rollout.turns.len(), 1);
    assert_eq!(rollout.turns[0].status, CodexTurnStatus::Completed);
    assert_eq!(rollout.turns[0].primary_model, None);
    assert_eq!(rollout.turns[0].total_token_count(), None);
    assert_eq!(
        rollout.turns[0].final_answer,
        Some(CodexTurnMessage {
            timestamp: "2026-01-01T00:00:06Z".to_owned(),
            text: "Done".to_owned(),
        })
    );
    assert!(matches!(
        &rollout.turns[0].steps[0],
        CodexTurnStep::Commentary { text, .. } if text == "Reading"
    ));
    assert!(matches!(
        &rollout.turns[0].steps[2],
        CodexTurnStep::ToolCallOutput { output, .. } if output.contains("input_image")
    ));
    assert_eq!(rollout.turns[0].steps.len(), 3);

    Ok(())
}

#[test]
fn rejects_structured_tool_output_in_pre_097_epoch() {
    let error = parse_rollout_reader(
            Cursor::new(
                r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture","cwd":"/tmp/repo","cli_version":"0.95.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Inspect repo"}}
{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":[{"type":"input_image","image_url":"data:image/png;base64,abc"}]}}
"#,
            ),
            Path::new("fixture.jsonl"),
        )
        .unwrap_err();

    assert!(error.to_string().contains("unsupported tool output shape"));
}

#[test]
fn rejects_response_item_variants_before_their_supported_version() {
    let error = parse_rollout_reader(
            Cursor::new(
                r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture","cwd":"/tmp/repo","cli_version":"0.94.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Inspect repo"}}
{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"image_generation_call","status":"completed","result":"image-bytes","id":"ig_123"}}
"#,
            ),
            Path::new("fixture.jsonl"),
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("unsupported response_item `image_generation_call`")
    );
}

#[test]
fn extracts_model_and_tokens_from_turn_context_and_token_count() -> Result<()> {
    let rollout = parse_rollout_reader(
        Cursor::new(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture","cwd":"/tmp/repo","cli_version":"0.118.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"turn_id":"turn-1","model":"gpt-5.4"}}
{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"Inspect repo"}}
{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":100},"last_token_usage":{"total_tokens":100}}}}
{"timestamp":"2026-01-01T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":160},"last_token_usage":{"total_tokens":60}}}}
{"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Done"}]}}
"#,
        ),
        Path::new("fixture.jsonl"),
    )?;

    assert_eq!(rollout.turns.len(), 1);
    assert_eq!(rollout.turns[0].primary_model.as_deref(), Some("gpt-5.4"));
    assert_eq!(rollout.turns[0].total_token_count(), Some(160));

    Ok(())
}
