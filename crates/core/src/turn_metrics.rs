use crate::parse::{CodexTurn, CodexTurnStep};

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
        &turn.steps,
    )
}

/// Summarizes one persisted turn row back into the derived analytics counters it should store.
pub(crate) fn summarize_stored_turn_metrics(
    started_at: &str,
    completed_at: Option<&str>,
    final_answer_at: Option<&str>,
    final_answer_text: Option<&str>,
    steps: &[CodexTurnStep],
) -> IndexedTurnMetrics {
    summarize_turn_parts(
        started_at,
        completed_at,
        final_answer_at,
        final_answer_text,
        steps,
    )
}

/// Summarizes one turn from its canonical timestamps, final answer fields, and steps.
fn summarize_turn_parts(
    started_at: &str,
    completed_at: Option<&str>,
    final_answer_at: Option<&str>,
    final_answer_text: Option<&str>,
    steps: &[CodexTurnStep],
) -> IndexedTurnMetrics {
    let mut metrics = IndexedTurnMetrics {
        step_count: steps.len().try_into().unwrap_or(u32::MAX),
        has_final_answer: final_answer_at.is_some() || final_answer_text.is_some(),
        duration_ms: completed_at
            .and_then(|completed| indexed_timestamp_duration_ms(started_at, completed)),
        ..IndexedTurnMetrics::default()
    };

    for step in steps {
        match step {
            CodexTurnStep::ToolCall { .. } => {
                metrics.tool_call_count = metrics.tool_call_count.saturating_add(1);
            }
            CodexTurnStep::ToolCallOutput { .. } => {
                metrics.tool_output_count = metrics.tool_output_count.saturating_add(1);
            }
            CodexTurnStep::Attachment { .. } => {
                metrics.attachment_count = metrics.attachment_count.saturating_add(1);
            }
            CodexTurnStep::Delegation { .. } => {
                metrics.delegation_count = metrics.delegation_count.saturating_add(1);
            }
            CodexTurnStep::HookSummary { .. } => {
                metrics.hook_summary_count = metrics.hook_summary_count.saturating_add(1);
            }
            CodexTurnStep::Reasoning { .. }
            | CodexTurnStep::Commentary { .. }
            | CodexTurnStep::ProviderResponseItem { .. } => {}
        }
    }

    metrics
}

/// Calculates one indexed turn duration in milliseconds when both timestamps parse cleanly.
fn indexed_timestamp_duration_ms(started_at: &str, completed_at: &str) -> Option<i64> {
    let started = parse_utc_timestamp_millis(started_at)?;
    let completed = parse_utc_timestamp_millis(completed_at)?;
    completed
        .checked_sub(started)
        .filter(|duration| *duration >= 0)
}

/// Parses one memstack UTC timestamp into Unix milliseconds.
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
