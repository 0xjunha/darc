use serde::{Deserialize, Serialize};

/// Stores one normalized cross-provider token usage breakdown for a turn.
/// `reasoning_token_count` is a subset of `output_token_count`, not an additive peer bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedTokenUsage {
    pub input_uncached_token_count: Option<u64>,
    pub cache_read_token_count: Option<u64>,
    pub cache_write_token_count: Option<u64>,
    pub output_token_count: Option<u64>,
    pub reasoning_token_count: Option<u64>,
    pub provider_total_token_count: Option<u64>,
    pub normalized_total_token_count: Option<u64>,
}

impl NormalizedTokenUsage {
    /// Returns whether one normalized token usage record contains any known value.
    pub fn has_any_value(self) -> bool {
        self.input_uncached_token_count.is_some()
            || self.cache_read_token_count.is_some()
            || self.cache_write_token_count.is_some()
            || self.output_token_count.is_some()
            || self.reasoning_token_count.is_some()
            || self.provider_total_token_count.is_some()
            || self.normalized_total_token_count.is_some()
    }

    /// Adds one token-usage delta into the accumulated turn totals.
    pub fn saturating_add_assign(&mut self, delta: Self) {
        saturating_add_optional_counter(
            &mut self.input_uncached_token_count,
            delta.input_uncached_token_count,
        );
        saturating_add_optional_counter(
            &mut self.cache_read_token_count,
            delta.cache_read_token_count,
        );
        saturating_add_optional_counter(
            &mut self.cache_write_token_count,
            delta.cache_write_token_count,
        );
        saturating_add_optional_counter(&mut self.output_token_count, delta.output_token_count);
        saturating_add_optional_counter(
            &mut self.reasoning_token_count,
            delta.reasoning_token_count,
        );
        saturating_add_optional_counter(
            &mut self.provider_total_token_count,
            delta.provider_total_token_count,
        );
        saturating_add_optional_counter(
            &mut self.normalized_total_token_count,
            delta.normalized_total_token_count,
        );
    }

    /// Keeps the greater observed per-field counters while deduplicating cumulative rows.
    pub fn saturating_max_assign(&mut self, other: Self) {
        saturating_max_optional_counter(
            &mut self.input_uncached_token_count,
            other.input_uncached_token_count,
        );
        saturating_max_optional_counter(
            &mut self.cache_read_token_count,
            other.cache_read_token_count,
        );
        saturating_max_optional_counter(
            &mut self.cache_write_token_count,
            other.cache_write_token_count,
        );
        saturating_max_optional_counter(&mut self.output_token_count, other.output_token_count);
        saturating_max_optional_counter(
            &mut self.reasoning_token_count,
            other.reasoning_token_count,
        );
        saturating_max_optional_counter(
            &mut self.provider_total_token_count,
            other.provider_total_token_count,
        );
        saturating_max_optional_counter(
            &mut self.normalized_total_token_count,
            other.normalized_total_token_count,
        );
    }
}

/// Adds one optional token counter using saturating arithmetic.
fn saturating_add_optional_counter(total: &mut Option<u64>, delta: Option<u64>) {
    let Some(delta) = delta else {
        return;
    };
    *total = Some(total.unwrap_or(0).saturating_add(delta));
}

/// Keeps the greater observed value for one optional token counter.
fn saturating_max_optional_counter(total: &mut Option<u64>, observed: Option<u64>) {
    let Some(observed) = observed else {
        return;
    };
    *total = Some(total.unwrap_or(0).max(observed));
}

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
    pub token_usage: Option<NormalizedTokenUsage>,
    pub steps: Vec<NormalizedTurnStep>,
}

impl NormalizedTurn {
    /// Returns the normalized cache-aware token total for one turn when present.
    pub fn total_token_count(&self) -> Option<u64> {
        self.token_usage
            .and_then(|usage| usage.normalized_total_token_count)
    }
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
