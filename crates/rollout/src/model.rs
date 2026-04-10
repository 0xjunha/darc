use serde::{Deserialize, Serialize};

/// Stores one user turn and the assistant activity that followed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedTurn {
    pub turn_id: Option<String>,
    pub user_message: String,
    pub final_answer: Option<NormalizedTurnMessage>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub status: NormalizedTurnStatus,
    pub primary_model: Option<String>,
    pub total_token_count: Option<u64>,
    pub steps: Vec<NormalizedTurnStep>,
}

/// Stores one top-level assistant message attached to a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedTurnMessage {
    pub timestamp: String,
    pub text: String,
}

/// Tracks whether a parsed turn finished normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedTurnStatus {
    Completed,
    Aborted,
    Incomplete,
}

impl NormalizedTurnStatus {
    /// Returns the stable SQLite string value for one turn status.
    pub fn as_sql_text(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Aborted => "aborted",
            Self::Incomplete => "incomplete",
        }
    }
}

/// Stores one ordered assistant-visible step inside a normalized turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NormalizedTurnStep {
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
    Attachment {
        timestamp: String,
        attachment_type: String,
        payload_json: String,
    },
    Delegation {
        timestamp: String,
        call_id: Option<String>,
        task_id: Option<String>,
        event: String,
        agent_id: Option<String>,
        agent_type: Option<String>,
        status: Option<String>,
        summary: Option<String>,
        payload_json: String,
    },
    HookSummary {
        timestamp: String,
        call_id: Option<String>,
        hook_count: u32,
        prevented_continuation: bool,
        has_output: bool,
        level: Option<String>,
        payload_json: String,
    },
    ProviderResponseItem {
        timestamp: String,
        item_type: String,
        payload_json: String,
    },
}
