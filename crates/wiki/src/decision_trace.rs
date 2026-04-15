use crate::{
    DigestProposalEntry, DigestProposalOptionStatus, EntryFrontmatter,
    proposal::normalize_identity_domains, render::normalize_trimmed_list,
};

const CONTEXT_HEADER: &str = "## Context";
const OPTIONS_HEADER: &str = "## Options Considered";
const FINAL_DECISION_HEADER: &str = "## Final Decision";
const RATIONALE_HEADER: &str = "## Rationale";
const CONSEQUENCES_HEADER: &str = "## Consequences";
const EVIDENCE_HEADER: &str = "## Evidence";

/// Computes one stable semantic fingerprint for a proposal decision trace.
pub(crate) fn proposal_content_fingerprint(entry: &DigestProposalEntry) -> String {
    semantic_fingerprint(SemanticFingerprintInput {
        title: entry.title.trim(),
        category: entry.category.trim(),
        domains: &entry.domains,
        decision_date: entry.decision_date.trim(),
        context: entry.context.trim(),
        options: normalize_proposal_options(entry),
        final_decision: entry.final_decision.trim(),
        rationale: entry.rationale.trim(),
        consequences: entry.consequences.trim(),
        evidence: normalize_trimmed_list(&entry.evidence),
    })
}

/// Computes one stable semantic fingerprint for a stored canonical decision trace.
pub(crate) fn existing_content_fingerprint(
    frontmatter: &EntryFrontmatter,
    body_markdown: &str,
) -> Option<String> {
    if let Some(fingerprint) = &frontmatter.content_fingerprint {
        return Some(fingerprint.clone());
    }
    let decision_date = frontmatter.decision_date.as_deref()?.trim();
    let sections = parse_canonical_sections(body_markdown)?;
    Some(semantic_fingerprint(SemanticFingerprintInput {
        title: frontmatter.title.trim(),
        category: frontmatter.category.trim(),
        domains: &frontmatter.domains,
        decision_date,
        context: &sections.context,
        options: sections.options,
        final_decision: &sections.final_decision,
        rationale: &sections.rationale,
        consequences: &sections.consequences,
        evidence: sections.evidence,
    }))
}

/// Stores one parsed canonical decision-trace body split into semantic sections.
struct CanonicalSections {
    context: String,
    options: Vec<String>,
    final_decision: String,
    rationale: String,
    consequences: String,
    evidence: Vec<String>,
}

/// Stores the normalized semantic fields that feed the stable content fingerprint.
struct SemanticFingerprintInput<'a> {
    title: &'a str,
    category: &'a str,
    domains: &'a [String],
    decision_date: &'a str,
    context: &'a str,
    options: Vec<String>,
    final_decision: &'a str,
    rationale: &'a str,
    consequences: &'a str,
    evidence: Vec<String>,
}

/// Parses the current canonical decision-trace Markdown body into semantic sections.
fn parse_canonical_sections(body_markdown: &str) -> Option<CanonicalSections> {
    let mut current_header = None;
    let mut current_lines = Vec::new();
    let mut sections = std::collections::BTreeMap::new();

    for line in body_markdown.lines() {
        if is_section_header(line) {
            if let Some(header) = current_header.take() {
                sections.insert(header, current_lines.join("\n").trim().to_owned());
            }
            current_header = Some(line.trim());
            current_lines.clear();
        } else if current_header.is_some() {
            current_lines.push(line);
        }
    }
    if let Some(header) = current_header {
        sections.insert(header, current_lines.join("\n").trim().to_owned());
    }

    Some(CanonicalSections {
        context: sections.get(CONTEXT_HEADER)?.trim().to_owned(),
        options: parse_options_section(sections.get(OPTIONS_HEADER)?)?,
        final_decision: sections.get(FINAL_DECISION_HEADER)?.trim().to_owned(),
        rationale: sections.get(RATIONALE_HEADER)?.trim().to_owned(),
        consequences: sections.get(CONSEQUENCES_HEADER)?.trim().to_owned(),
        evidence: parse_evidence_section(sections.get(EVIDENCE_HEADER)?)?,
    })
}

/// Returns whether one line starts a canonical decision-trace section.
fn is_section_header(line: &str) -> bool {
    matches!(
        line.trim(),
        CONTEXT_HEADER
            | OPTIONS_HEADER
            | FINAL_DECISION_HEADER
            | RATIONALE_HEADER
            | CONSEQUENCES_HEADER
            | EVIDENCE_HEADER
    )
}

/// Parses the rendered options section back into normalized semantic rows.
fn parse_options_section(section: &str) -> Option<Vec<String>> {
    let mut options = Vec::new();
    for line in section
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let (_, remainder) = line.split_once(". ")?;
        options.push(normalize_option_row(remainder)?);
    }
    if options.is_empty() {
        None
    } else {
        options.sort();
        options.dedup();
        Some(options)
    }
}

/// Parses the rendered evidence section back into normalized evidence refs.
fn parse_evidence_section(section: &str) -> Option<Vec<String>> {
    let mut evidence = Vec::new();
    for line in section
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let value = line.strip_prefix("- ")?;
        evidence.push(value.trim_matches('`').trim().to_owned());
    }
    if evidence.is_empty() {
        None
    } else {
        evidence.sort();
        evidence.dedup();
        Some(evidence)
    }
}

/// Builds the normalized semantic option rows for one proposal entry.
fn normalize_proposal_options(entry: &DigestProposalEntry) -> Vec<String> {
    let mut options = entry
        .options
        .iter()
        .filter_map(|option| {
            normalize_option_row(&format!(
                "{}: {}",
                option_status_label(option.status),
                option.description.trim()
            ))
        })
        .collect::<Vec<_>>();
    options.sort();
    options.dedup();
    options
}

/// Normalizes one option row into the semantic identity representation.
fn normalize_option_row(value: &str) -> Option<String> {
    let (status, description) = value.split_once(':')?;
    let status = status.trim().to_ascii_lowercase();
    let description = description.trim();
    if description.is_empty() {
        None
    } else {
        Some(format!("{status}:{description}"))
    }
}

/// Builds one stable hash string from the semantic decision-trace content.
fn semantic_fingerprint(input: SemanticFingerprintInput<'_>) -> String {
    let canonical = [
        format!("title={}", input.title),
        format!("category={}", input.category),
        format!(
            "domains={}",
            normalize_identity_domains(input.domains).join(",")
        ),
        format!("decision_date={}", input.decision_date),
        format!("context={}", input.context),
        format!("options={}", input.options.join("|")),
        format!("final_decision={}", input.final_decision),
        format!("rationale={}", input.rationale),
        format!("consequences={}", input.consequences),
        format!("evidence={}", input.evidence.join("|")),
    ]
    .join("\n");
    format!("{:016x}", fnv1a_64(canonical.as_bytes()))
}

/// Returns the stable status label used in the proposal semantic fingerprint.
fn option_status_label(status: DigestProposalOptionStatus) -> &'static str {
    match status {
        DigestProposalOptionStatus::Chosen => "chosen",
        DigestProposalOptionStatus::Rejected => "rejected",
    }
}

/// Computes one deterministic 64-bit FNV-1a hash without external dependencies.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
