use std::collections::BTreeSet;

use darc_rollout::model::{
    NormalizedTokenUsage, NormalizedTurn as CodexTurn, NormalizedTurnStep as CodexTurnStep,
};
use serde_json::Value;

use crate::policy::{
    apply_patch_changed_paths, shell_apply_patch_changed_paths, summarize_apply_patch_changes,
    summarize_shell_code_changes,
};

/// Stores the derived per-turn analytics counters persisted in the SQLite index.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct IndexedTurnMetrics {
    pub(crate) step_count: u32,
    pub(crate) tool_call_count: u32,
    pub(crate) tool_output_count: u32,
    pub(crate) attachment_count: u32,
    pub(crate) delegation_count: u32,
    pub(crate) hook_summary_count: u32,
    pub(crate) has_final_answer: bool,
    pub(crate) duration_ms: Option<i64>,
    pub(crate) effective_agent_runtime_ms: Option<i64>,
    pub(crate) provider_total_token_count: Option<i64>,
    pub(crate) input_uncached_token_count: Option<i64>,
    pub(crate) cache_read_token_count: Option<i64>,
    pub(crate) cache_write_token_count: Option<i64>,
    pub(crate) output_token_count: Option<i64>,
    pub(crate) reasoning_token_count: Option<i64>,
    pub(crate) total_token_count: Option<i64>,
    pub(crate) changed_file_count: u32,
    pub(crate) added_line_count: u32,
    pub(crate) removed_line_count: u32,
}

/// Summarizes one normalized turn into the derived analytics counters stored in SQLite.
pub(crate) fn summarize_turn_metrics(turn: &CodexTurn) -> IndexedTurnMetrics {
    summarize_turn_parts(
        &turn.started_at,
        turn.completed_at.as_deref(),
        turn.final_answer
            .as_ref()
            .map(|message| message.timestamp.as_str()),
        turn.final_answer
            .as_ref()
            .map(|message| message.text.as_str()),
        turn.token_usage,
        &turn.steps,
    )
}

/// Summarizes one persisted turn row back into the derived analytics counters it should store.
pub(crate) fn summarize_stored_turn_metrics(
    started_at: &str,
    completed_at: Option<&str>,
    final_answer_at: Option<&str>,
    final_answer_text: Option<&str>,
    total_token_count: Option<u64>,
    steps: &[CodexTurnStep],
) -> IndexedTurnMetrics {
    summarize_turn_parts(
        started_at,
        completed_at,
        final_answer_at,
        final_answer_text,
        total_token_count.map(|total_token_count| NormalizedTokenUsage {
            normalized_total_token_count: Some(total_token_count),
            ..NormalizedTokenUsage::default()
        }),
        steps,
    )
}

/// Summarizes one turn from its canonical timestamps, final answer fields, and steps.
fn summarize_turn_parts(
    started_at: &str,
    completed_at: Option<&str>,
    final_answer_at: Option<&str>,
    final_answer_text: Option<&str>,
    token_usage: Option<NormalizedTokenUsage>,
    steps: &[CodexTurnStep],
) -> IndexedTurnMetrics {
    let duration_ms =
        completed_at.and_then(|completed| indexed_timestamp_duration_ms(started_at, completed));
    let total_token_count = token_usage.and_then(|usage| usage.normalized_total_token_count);
    let mut metrics = IndexedTurnMetrics {
        step_count: steps.len().try_into().unwrap_or(u32::MAX),
        has_final_answer: final_answer_at.is_some() || final_answer_text.is_some(),
        duration_ms,
        effective_agent_runtime_ms: duration_ms,
        provider_total_token_count: token_usage
            .and_then(|usage| usage.provider_total_token_count)
            .and_then(|value| i64::try_from(value).ok()),
        input_uncached_token_count: token_usage
            .and_then(|usage| usage.input_uncached_token_count)
            .and_then(|value| i64::try_from(value).ok()),
        cache_read_token_count: token_usage
            .and_then(|usage| usage.cache_read_token_count)
            .and_then(|value| i64::try_from(value).ok()),
        cache_write_token_count: token_usage
            .and_then(|usage| usage.cache_write_token_count)
            .and_then(|value| i64::try_from(value).ok()),
        output_token_count: token_usage
            .and_then(|usage| usage.output_token_count)
            .and_then(|value| i64::try_from(value).ok()),
        reasoning_token_count: token_usage
            .and_then(|usage| usage.reasoning_token_count)
            .and_then(|value| i64::try_from(value).ok()),
        total_token_count: total_token_count.and_then(|value| i64::try_from(value).ok()),
        ..IndexedTurnMetrics::default()
    };
    let mut changed_paths = BTreeSet::new();

    for step in steps {
        match step {
            CodexTurnStep::ToolCall {
                name, arguments, ..
            } => {
                metrics.tool_call_count = metrics.tool_call_count.saturating_add(1);
                let (code_changes, patch_paths) = if name == "apply_patch" {
                    (
                        summarize_apply_patch_changes(arguments),
                        apply_patch_changed_paths(arguments),
                    )
                } else {
                    (
                        summarize_shell_code_changes(arguments),
                        shell_apply_patch_changed_paths(arguments),
                    )
                };
                changed_paths.extend(patch_paths);
                metrics.added_line_count = metrics
                    .added_line_count
                    .saturating_add(code_changes.added_line_count);
                metrics.removed_line_count = metrics
                    .removed_line_count
                    .saturating_add(code_changes.removed_line_count);
            }
            CodexTurnStep::ToolCallOutput { .. } => {
                metrics.tool_output_count = metrics.tool_output_count.saturating_add(1);
            }
            CodexTurnStep::Attachment { .. } => {
                metrics.attachment_count = metrics.attachment_count.saturating_add(1);
            }
            CodexTurnStep::Delegation { payload_json, .. } => {
                metrics.delegation_count = metrics.delegation_count.saturating_add(1);
                if let Some(duration_ms) = delegation_duration_ms(payload_json) {
                    let base_runtime = metrics.effective_agent_runtime_ms.unwrap_or(0);
                    let delegated_runtime = i64::try_from(duration_ms).unwrap_or(i64::MAX);
                    metrics.effective_agent_runtime_ms =
                        Some(base_runtime.saturating_add(delegated_runtime));
                }
            }
            CodexTurnStep::HookSummary { .. } => {
                metrics.hook_summary_count = metrics.hook_summary_count.saturating_add(1);
            }
            CodexTurnStep::Reasoning { .. }
            | CodexTurnStep::Commentary { .. }
            | CodexTurnStep::ProviderResponseItem { .. } => {}
        }
    }

    metrics.changed_file_count = changed_paths.len().try_into().unwrap_or(u32::MAX);
    metrics
}

/// Extracts one delegated runtime in milliseconds from a stored delegation payload when present.
fn delegation_duration_ms(payload_json: &str) -> Option<u64> {
    serde_json::from_str::<Value>(payload_json)
        .ok()?
        .get("totalDurationMs")
        .and_then(Value::as_u64)
}

/// Calculates one indexed turn duration in milliseconds when both timestamps parse cleanly.
fn indexed_timestamp_duration_ms(started_at: &str, completed_at: &str) -> Option<i64> {
    let started = parse_utc_timestamp_millis(started_at)?;
    let completed = parse_utc_timestamp_millis(completed_at)?;
    completed
        .checked_sub(started)
        .filter(|duration| *duration >= 0)
}

/// Parses one darc UTC timestamp into Unix milliseconds.
fn parse_utc_timestamp_millis(value: &str) -> Option<i64> {
    let value = value.strip_suffix('Z')?;
    let (date, time) = value.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() {
        return None;
    }

    let (time, fractional) = match time.split_once('.') {
        Some((time, fractional)) => (time, Some(fractional)),
        None => (time, None),
    };
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts.next()?.parse().ok()?;
    if time_parts.next().is_some() {
        return None;
    }

    let fractional_ms = fractional.map_or(0_i64, |fractional| {
        let digits = fractional
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() {
            return 0;
        }
        let mut millis = digits.chars().take(3).collect::<String>();
        while millis.len() < 3 {
            millis.push('0');
        }
        millis.parse::<i64>().unwrap_or(0)
    });

    let days = days_from_civil(year, month, day)?;
    days.checked_mul(86_400_000)?
        .checked_add(hour.checked_mul(3_600_000)?)?
        .checked_add(minute.checked_mul(60_000)?)?
        .checked_add(second.checked_mul(1_000)?)?
        .checked_add(fractional_ms)
}

/// Converts one civil UTC date into the number of days since the Unix epoch.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let month = i64::from(month);
    let day = i64::from(day);
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = if adjusted_year >= 0 {
        adjusted_year / 400
    } else {
        (adjusted_year - 399) / 400
    };
    let year_of_era = adjusted_year - era * 400;
    let month_of_year = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_of_year + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

#[cfg(test)]
mod tests {
    use darc_rollout::model::NormalizedTurnStatus as CodexTurnStatus;

    use super::*;

    #[test]
    fn summarize_turn_metrics_counts_shell_heredoc_apply_patch_changes() {
        let turn = CodexTurn {
            turn_id: Some("turn-1".to_owned()),
            user_message: "Patch the file".to_owned(),
            final_answer: None,
            started_at: "2026-04-06T00:00:00Z".to_owned(),
            completed_at: Some("2026-04-06T00:00:05Z".to_owned()),
            status: CodexTurnStatus::Completed,
            primary_model: Some("gpt-5.4".to_owned()),
            token_usage: Some(NormalizedTokenUsage {
                normalized_total_token_count: Some(10),
                ..NormalizedTokenUsage::default()
            }),
            steps: vec![CodexTurnStep::ToolCall {
                timestamp: "2026-04-06T00:00:01Z".to_owned(),
                call_id: "call-1".to_owned(),
                name: "exec_command".to_owned(),
                arguments: r#"{"cmd":"apply_patch <<'PATCH'\n*** Begin Patch\n*** Update File: src/main.rs\n@@\n-old\n+new\n*** End Patch\nPATCH","workdir":"/tmp/repo"}"#
                    .to_owned(),
            }],
        };

        let metrics = summarize_turn_metrics(&turn);

        assert_eq!(metrics.changed_file_count, 1);
        assert_eq!(metrics.added_line_count, 1);
        assert_eq!(metrics.removed_line_count, 1);
    }

    #[test]
    fn summarize_turn_metrics_dedupes_changed_files_across_patch_calls() {
        let turn = CodexTurn {
            turn_id: Some("turn-1".to_owned()),
            user_message: "Patch twice".to_owned(),
            final_answer: None,
            started_at: "2026-04-06T00:00:00Z".to_owned(),
            completed_at: Some("2026-04-06T00:00:05Z".to_owned()),
            status: CodexTurnStatus::Completed,
            primary_model: Some("gpt-5.4".to_owned()),
            token_usage: Some(NormalizedTokenUsage {
                normalized_total_token_count: Some(10),
                ..NormalizedTokenUsage::default()
            }),
            steps: vec![
                CodexTurnStep::ToolCall {
                    timestamp: "2026-04-06T00:00:01Z".to_owned(),
                    call_id: "call-1".to_owned(),
                    name: "apply_patch".to_owned(),
                    arguments: "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-old\n+new\n*** End Patch\n"
                        .to_owned(),
                },
                CodexTurnStep::ToolCall {
                    timestamp: "2026-04-06T00:00:02Z".to_owned(),
                    call_id: "call-2".to_owned(),
                    name: "apply_patch".to_owned(),
                    arguments:
                        "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-older\n+newer\n*** End Patch\n"
                            .to_owned(),
                },
            ],
        };

        let metrics = summarize_turn_metrics(&turn);

        assert_eq!(metrics.changed_file_count, 1);
        assert_eq!(metrics.added_line_count, 2);
        assert_eq!(metrics.removed_line_count, 2);
    }
}
