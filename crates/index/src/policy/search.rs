use darc_rollout::model::NormalizedTurnStep;

use super::extract_shell_command;

/// Builds the flattened turn text indexed for keyword search.
pub fn build_turn_search_text(steps: &[NormalizedTurnStep]) -> String {
    let mut parts = Vec::<String>::new();

    for step in steps {
        match step {
            NormalizedTurnStep::Commentary { text, .. } => push_search_text(&mut parts, text),
            NormalizedTurnStep::ToolCall {
                name, arguments, ..
            } => {
                push_search_text(&mut parts, name);
                if let Some(shell_command) = extract_shell_command(name, arguments) {
                    push_search_text(&mut parts, &shell_command.command_text);
                }
                push_search_text(&mut parts, arguments);
            }
            NormalizedTurnStep::ToolCallOutput { output, .. } => {
                push_search_text(&mut parts, output);
            }
            NormalizedTurnStep::Attachment { payload_json, .. }
            | NormalizedTurnStep::HookSummary { payload_json, .. }
            | NormalizedTurnStep::ProviderResponseItem { payload_json, .. } => {
                push_search_text(&mut parts, payload_json);
            }
            NormalizedTurnStep::Delegation {
                summary,
                payload_json,
                ..
            } => {
                if let Some(summary) = summary {
                    push_search_text(&mut parts, summary);
                }
                push_search_text(&mut parts, payload_json);
            }
            NormalizedTurnStep::Reasoning { summary, .. } => {
                for line in summary {
                    push_search_text(&mut parts, line);
                }
            }
        }
    }

    parts.join("\n")
}

/// Pushes one non-empty search fragment into the flattened turn text.
fn push_search_text(parts: &mut Vec<String>, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    parts.push(trimmed.to_owned());
}
