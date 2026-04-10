mod active_time;
mod file_access;
mod search;
mod shell;

pub use active_time::{ActiveTimePolicy, active_time_policy, should_include_turn_in_active_time};
use darc_paths::SourceKind;
pub use file_access::{
    CodeChangeSummary, FileAccessRecord, ToolAccessKind, ToolCallRecord, classify_tool_access,
    derive_file_access_records, extract_tool_call_records, extract_tool_path, extract_tool_paths,
    summarize_apply_patch_changes,
};
pub use search::build_turn_search_text;
pub use shell::{ShellCommand, extract_shell_command, summarize_shell_code_changes};

/// Defines the fields needed to rank hard-debugging candidates.
pub trait HardDebuggingCandidate {
    /// Returns the project id used to break ranking ties.
    fn project_id(&self) -> &str;

    /// Returns the provider used to break ranking ties.
    fn provider(&self) -> SourceKind;

    /// Returns the session id used to break ranking ties.
    fn session_id(&self) -> &str;

    /// Returns the turn ordinal used to break ranking ties.
    fn turn_ordinal(&self) -> u64;

    /// Returns the step count used to rank harder debugging turns first.
    fn step_count(&self) -> u64;

    /// Returns the turn duration used as the secondary ranking signal.
    fn duration_ms(&self) -> u64;
}

/// Applies the current provisional hard-debugging ranking policy in place.
pub fn rank_hard_debuggings<T>(turns: &mut Vec<T>)
where
    T: HardDebuggingCandidate,
{
    turns.sort_by(|left, right| {
        right
            .step_count()
            .cmp(&left.step_count())
            .then_with(|| right.duration_ms().cmp(&left.duration_ms()))
            .then_with(|| left.project_id().cmp(right.project_id()))
            .then_with(|| left.provider().cmp(&right.provider()))
            .then_with(|| left.session_id().cmp(right.session_id()))
            .then_with(|| left.turn_ordinal().cmp(&right.turn_ordinal()))
    });
    turns.truncate(10);
}
