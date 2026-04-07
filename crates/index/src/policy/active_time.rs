use darc_rollout::model::NormalizedTurnStatus;

/// Stores the hardened active-time inclusion policy used by query insights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveTimePolicy {
    pub min_duration_ms: u64,
}

/// Returns the current active-time inclusion policy.
pub const fn active_time_policy() -> ActiveTimePolicy {
    ActiveTimePolicy {
        min_duration_ms: 2_000,
    }
}

/// Returns whether one turn should contribute to active-runtime charts.
pub fn should_include_turn_in_active_time(status: NormalizedTurnStatus, duration_ms: u64) -> bool {
    let policy = active_time_policy();
    status == NormalizedTurnStatus::Completed
        && duration_ms >= policy.min_duration_ms
        && duration_ms > 0
}
