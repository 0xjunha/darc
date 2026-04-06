use darc_rollout::model::NormalizedTurnStatus;
use serde_json::Value;

use crate::query::HardDebuggingTurn;

/// Stores the hardened active-time inclusion policy used by query insights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ActiveTimePolicy {
    pub(crate) min_duration_ms: u64,
}

/// Stores the coarse file-access bucket inferred from one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolAccessKind {
    Read,
    Write,
    Other,
}

/// Returns the current active-time inclusion policy.
pub(crate) const fn active_time_policy() -> ActiveTimePolicy {
    ActiveTimePolicy {
        min_duration_ms: 2_000,
    }
}

/// Returns whether one turn should contribute to active-runtime charts.
pub(crate) fn should_include_turn_in_active_time(
    status: NormalizedTurnStatus,
    duration_ms: u64,
) -> bool {
    let policy = active_time_policy();
    status == NormalizedTurnStatus::Completed
        && duration_ms >= policy.min_duration_ms
        && duration_ms > 0
}

/// Extracts one candidate file path from one tool-call arguments payload.
pub(crate) fn extract_tool_path(arguments: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(arguments).ok()?;
    let object = value.as_object()?;
    for key in ["file_path", "path", "file"] {
        if let Some(value) = object.get(key) {
            let path = value.as_str()?.trim();
            if !path.is_empty() {
                return Some(path.to_owned());
            }
        }
    }
    None
}

/// Classifies one tool name into a provisional coarse file-access bucket.
pub(crate) fn classify_tool_access(name: &str) -> ToolAccessKind {
    let normalized = name.to_ascii_lowercase();
    if normalized.contains("write") || normalized.contains("edit") || normalized.contains("replace")
    {
        ToolAccessKind::Write
    } else if normalized.contains("read")
        || normalized.contains("view")
        || normalized.contains("list")
    {
        ToolAccessKind::Read
    } else {
        ToolAccessKind::Other
    }
}

/// Applies the current provisional hard-debugging ranking policy in place.
pub(crate) fn rank_hard_debuggings(turns: &mut Vec<HardDebuggingTurn>) {
    turns.sort_by(|left, right| {
        right
            .step_count
            .cmp(&left.step_count)
            .then_with(|| right.duration_ms.cmp(&left.duration_ms))
            .then_with(|| left.project_id.cmp(&right.project_id))
            .then_with(|| left.provider.cmp(&right.provider))
            .then_with(|| left.session_id.cmp(&right.session_id))
            .then_with(|| left.turn_ordinal.cmp(&right.turn_ordinal))
    });
    turns.truncate(10);
}
