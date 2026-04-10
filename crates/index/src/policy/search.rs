use darc_rollout::model::NormalizedTurnStep;

const MAX_SEARCH_FRAGMENT_CHARS: usize = 256;
const MAX_SEARCH_TEXT_CHARS: usize = 2_048;

/// Builds the flattened turn text indexed for keyword search.
pub fn build_turn_search_text(steps: &[NormalizedTurnStep]) -> String {
    let mut parts = Vec::<String>::new();
    let mut total_chars = 0_usize;

    for step in steps {
        match step {
            NormalizedTurnStep::Commentary { text, .. } => {
                push_search_text(&mut parts, &mut total_chars, text);
            }
            NormalizedTurnStep::ToolCall {
                name, arguments, ..
            } => {
                let _ = arguments;
                push_search_text(&mut parts, &mut total_chars, name);
            }
            NormalizedTurnStep::Delegation { summary, .. } => {
                if let Some(summary) = summary {
                    push_search_text(&mut parts, &mut total_chars, summary);
                }
            }
            NormalizedTurnStep::Reasoning { .. }
            | NormalizedTurnStep::ToolCallOutput { .. }
            | NormalizedTurnStep::Attachment { .. }
            | NormalizedTurnStep::HookSummary { .. }
            | NormalizedTurnStep::ProviderResponseItem { .. } => {}
        }
    }

    parts.join("\n")
}

/// Pushes one non-empty search fragment into the flattened turn text.
fn push_search_text(parts: &mut Vec<String>, total_chars: &mut usize, text: &str) {
    if *total_chars >= MAX_SEARCH_TEXT_CHARS {
        return;
    }
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return;
    }

    let remaining = MAX_SEARCH_TEXT_CHARS.saturating_sub(*total_chars);
    let fragment = normalized
        .chars()
        .take(remaining.min(MAX_SEARCH_FRAGMENT_CHARS))
        .collect::<String>();
    if fragment.is_empty() {
        return;
    }

    *total_chars = total_chars.saturating_add(fragment.chars().count());
    parts.push(fragment);
}
