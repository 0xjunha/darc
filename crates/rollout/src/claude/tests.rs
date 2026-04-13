use std::{
    io::Cursor,
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde_json::Value;

use super::parser::{parse_rollout_file, parse_rollout_reader};
use super::{ClaudeArchivedContext, ClaudeRollout, ClaudeSessionKind};
use crate::ParseDeterminism;
use crate::model::{
    NormalizedTurnMessage as CodexTurnMessage, NormalizedTurnStatus as CodexTurnStatus,
    NormalizedTurnStep as CodexTurnStep,
};

fn parse_fixture(input: &str, context: &ClaudeArchivedContext) -> Result<ClaudeRollout> {
    Ok(parse_rollout_reader(
        Cursor::new(input),
        Path::new("fixture.jsonl"),
        context,
    )?)
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata/claude")
        .join(name)
}

fn parse_fixture_file(name: &str, context: &ClaudeArchivedContext) -> Result<ClaudeRollout> {
    Ok(parse_rollout_file(&fixture_path(name), context)?)
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
        let rollout = parse_fixture_file(fixture_name, &primary_context_for_session(session_id))?;

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
    let CodexTurnStep::ProviderResponseItem { item_type, .. } = &rollout.turns[0].steps[0] else {
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
    assert_eq!(rollout.turns[0].total_token_count(), Some(0));
    assert_eq!(
        rollout.turns[0]
            .token_usage
            .and_then(|usage| usage.cache_read_token_count),
        None
    );

    Ok(())
}

#[test]
fn extracts_total_tokens_from_assistant_usage_without_cache_fields() -> Result<()> {
    let rollout = parse_fixture(
        r#"{"parentUuid":null,"isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":"Summarize this"},"uuid":"user-1","timestamp":"2026-04-01T00:00:01Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"user-1","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-1","type":"message","role":"assistant","content":[{"type":"text","text":"Done."}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":7}},"requestId":"req-1","type":"assistant","uuid":"assistant-1","timestamp":"2026-04-01T00:00:02Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
"#,
        &primary_context(),
    )?;

    assert_eq!(rollout.turns.len(), 1);
    assert_eq!(rollout.turns[0].total_token_count(), Some(12));
    assert_eq!(
        rollout.turns[0]
            .token_usage
            .and_then(|usage| usage.cache_read_token_count),
        None
    );
    assert_eq!(
        rollout.turns[0]
            .token_usage
            .and_then(|usage| usage.cache_write_token_count),
        None
    );

    Ok(())
}

#[test]
fn extracts_model_and_total_tokens_from_assistant_usage() -> Result<()> {
    let rollout = parse_fixture(
        r#"{"parentUuid":null,"isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":"Summarize this"},"uuid":"user-1","timestamp":"2026-04-01T00:00:01Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"user-1","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-1","type":"message","role":"assistant","content":[{"type":"text","text":"Done."}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":7,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0},"service_tier":"standard","inference_geo":"not_available"}},"requestId":"req-1","type":"assistant","uuid":"assistant-1","timestamp":"2026-04-01T00:00:02Z","userType":"external","entrypoint":"claude-desktop","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
"#,
        &primary_context(),
    )?;

    assert_eq!(rollout.turns.len(), 1);
    assert_eq!(
        rollout.turns[0].primary_model.as_deref(),
        Some("claude-sonnet-4-6")
    );
    assert_eq!(rollout.turns[0].total_token_count(), Some(12));
    assert_eq!(
        rollout.turns[0]
            .token_usage
            .and_then(|usage| usage.cache_read_token_count),
        Some(0)
    );

    Ok(())
}

#[test]
fn delegated_total_tokens_fallback_survives_partial_usage() -> Result<()> {
    let rollout = parse_fixture(
        r#"{"parentUuid":null,"isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":"Inspect this"},"uuid":"user-1","timestamp":"2026-04-01T00:00:01Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"user-1","isSidechain":false,"message":{"model":"claude-sonnet-4-6","id":"assistant-1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"Agent","input":{"prompt":"Inspect this"}}],"stop_reason":"tool_use","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0},"service_tier":"standard","inference_geo":"not_available"}},"requestId":"req-1","type":"assistant","uuid":"assistant-1","timestamp":"2026-04-01T00:00:02Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
{"parentUuid":"assistant-1","isSidechain":false,"promptId":"prompt-1","type":"user","message":{"role":"user","content":[{"tool_use_id":"tool-1","type":"tool_result","content":[{"type":"text","text":"Done."}]}]},"uuid":"user-2","timestamp":"2026-04-01T00:00:03Z","toolUseResult":{"status":"completed","prompt":"Inspect this","agentId":"agent-1","content":[{"type":"text","text":"Done."}],"totalDurationMs":12,"totalTokens":34,"totalToolUseCount":1,"usage":{"input_tokens":5,"output_tokens":7}},"userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/repo","sessionId":"parent-session","version":"2.1.87","gitBranch":"main"}
"#,
        &primary_context(),
    )?;

    assert_eq!(rollout.turns.len(), 1);
    assert_eq!(rollout.turns[0].total_token_count(), Some(36));
    assert_eq!(
        rollout.turns[0]
            .token_usage
            .and_then(|usage| usage.provider_total_token_count),
        Some(34)
    );

    Ok(())
}
