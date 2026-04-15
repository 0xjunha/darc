use crate::{DigestProposalEntry, DigestProposalOption, DigestProposalOptionStatus};

/// Renders one canonical decision-trace Markdown body from a validated proposal row.
pub(crate) fn render_decision_trace_body(proposal_entry: &DigestProposalEntry) -> String {
    let evidence = normalize_trimmed_list(&proposal_entry.evidence);
    format!(
        concat!(
            "## Context\n\n",
            "{context}\n\n",
            "## Options Considered\n\n",
            "{options}\n\n",
            "## Final Decision\n\n",
            "{final_decision}\n\n",
            "## Rationale\n\n",
            "{rationale}\n\n",
            "## Consequences\n\n",
            "{consequences}\n\n",
            "## Evidence\n\n",
            "{evidence}\n"
        ),
        context = proposal_entry.context.trim(),
        options = render_options_markdown(&proposal_entry.options),
        final_decision = proposal_entry.final_decision.trim(),
        rationale = proposal_entry.rationale.trim(),
        consequences = proposal_entry.consequences.trim(),
        evidence = render_evidence_markdown(&evidence),
    )
}

/// Normalizes one trimmed string list while preserving stable semantics.
pub(crate) fn normalize_trimmed_list(values: &[String]) -> Vec<String> {
    let mut normalized = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
}

/// Renders the numbered options section for one decision trace.
fn render_options_markdown(options: &[DigestProposalOption]) -> String {
    let mut options = options
        .iter()
        .map(|option| {
            (
                option_status_label(option.status).to_owned(),
                option.description.trim().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    options.sort();
    options.dedup();
    options
        .into_iter()
        .enumerate()
        .map(|(index, (status, description))| format!("{}. {}: {}", index + 1, status, description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders the evidence list for one canonical decision trace body.
fn render_evidence_markdown(evidence: &[String]) -> String {
    evidence
        .iter()
        .map(|item| format!("- `{item}`"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns the human-readable label for one proposal option status.
fn option_status_label(status: DigestProposalOptionStatus) -> &'static str {
    match status {
        DigestProposalOptionStatus::Chosen => "Chosen",
        DigestProposalOptionStatus::Rejected => "Rejected",
    }
}
