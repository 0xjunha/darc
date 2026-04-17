use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::{Deserialize, Serialize};

use crate::{
    EntryFrontmatter, EntryType,
    decision_trace::{existing_content_fingerprint, proposal_content_fingerprint},
    parse_evidence_reference,
    slug::is_valid_slug_id,
};

/// Stores the fixed schema identifier for digest proposal artifacts.
pub const DIGEST_PROPOSAL_SCHEMA: &str = "darc.wiki.digest.proposal.v1";

/// Stores the JSON Schema used by external runtimes for structured digest output.
pub const DIGEST_PROPOSAL_OUTPUT_SCHEMA_JSON: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "additionalProperties": false,
  "required": ["schema", "project_id", "run_id", "entries", "run_summary"],
  "properties": {
    "schema": { "type": "string", "const": "darc.wiki.digest.proposal.v1" },
    "project_id": { "type": "string", "minLength": 1 },
    "run_id": { "type": "string", "minLength": 1 },
    "entries": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": [
          "operation",
          "entry_type",
          "title",
          "category",
          "domains",
          "decision_date",
          "context",
          "options",
          "final_decision",
          "rationale",
          "consequences",
          "evidence"
        ],
        "properties": {
          "operation": { "type": "string", "const": "create" },
          "entry_type": { "type": "string", "const": "decision_trace" },
          "title": { "type": "string", "minLength": 1 },
          "category": { "type": "string", "minLength": 1 },
          "domains": {
            "type": "array",
            "items": { "type": "string", "minLength": 1 }
          },
          "decision_date": { "type": "string", "minLength": 1 },
          "context": { "type": "string", "minLength": 1 },
          "options": {
            "type": "array",
            "items": {
              "type": "object",
              "additionalProperties": false,
              "required": ["status", "description"],
              "properties": {
                "status": { "type": "string", "enum": ["chosen", "rejected"] },
                "description": { "type": "string", "minLength": 1 }
              }
            }
          },
          "final_decision": { "type": "string", "minLength": 1 },
          "rationale": { "type": "string", "minLength": 1 },
          "consequences": { "type": "string", "minLength": 1 },
          "evidence": {
            "type": "array",
            "items": { "type": "string", "minLength": 1 }
          }
        }
      }
    },
    "run_summary": {
      "type": "object",
      "additionalProperties": false,
      "required": ["title", "summary", "themes", "extracted_decision_count"],
      "properties": {
        "title": { "type": "string", "minLength": 1 },
        "summary": { "type": "string", "minLength": 1 },
        "themes": {
          "type": "array",
          "items": { "type": "string", "minLength": 1 }
        },
        "extracted_decision_count": { "type": "integer", "minimum": 0 }
      }
    }
  }
}"#;

/// Stores one structured digest proposal returned by an external runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestProposal {
    pub schema: String,
    pub project_id: String,
    pub run_id: String,
    pub entries: Vec<DigestProposalEntry>,
    pub run_summary: DigestProposalRunSummary,
}

/// Stores one structured decision-trace proposal entry within a digest proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestProposalEntry {
    pub operation: ProposalEntryOperation,
    pub entry_type: EntryType,
    pub title: String,
    pub category: String,
    pub domains: Vec<String>,
    pub decision_date: String,
    pub context: String,
    pub options: Vec<DigestProposalOption>,
    pub final_decision: String,
    pub rationale: String,
    pub consequences: String,
    pub evidence: Vec<String>,
}

/// Stores the supported proposal mutation operations for digest proposals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalEntryOperation {
    Create,
}

/// Stores one proposal option row for a decision trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestProposalOption {
    pub status: DigestProposalOptionStatus,
    pub description: String,
}

/// Stores one proposal option classification for a decision trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestProposalOptionStatus {
    Chosen,
    Rejected,
}

/// Stores the required run summary block for one digest proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestProposalRunSummary {
    pub title: String,
    pub summary: String,
    pub themes: Vec<String>,
    pub extracted_decision_count: usize,
}

/// Stores the contextual allowlists used when validating one digest proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalValidationOptions<'a> {
    pub project_id: &'a str,
    pub run_id: &'a str,
    pub allowed_categories: &'a [String],
    pub allowed_domains: &'a [String],
    pub allowed_evidence_refs: &'a [String],
}

/// Stores one structured proposal validation error for durable reporting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalValidationError {
    pub path: String,
    pub message: String,
}

/// Stores the successful validation summary for one proposal artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalValidationSummary {
    pub entry_count: usize,
    pub run_summary_title: String,
    pub extracted_decision_count: usize,
}

/// Stores the full validation error collection for one invalid proposal artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalValidationErrors {
    errors: Vec<ProposalValidationError>,
}

impl ProposalValidationErrors {
    /// Returns the structured validation errors for one invalid proposal artifact.
    pub fn errors(&self) -> &[ProposalValidationError] {
        &self.errors
    }

    /// Consumes the error wrapper into its structured validation issues.
    pub fn into_errors(self) -> Vec<ProposalValidationError> {
        self.errors
    }
}

impl fmt::Display for ProposalValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "proposal validation failed with {} issue(s)",
            self.errors.len()
        )
    }
}

impl std::error::Error for ProposalValidationErrors {}

/// Validates one candidate domain id against the current project-scoped slug rules.
pub fn is_valid_domain_id(value: &str) -> bool {
    is_valid_slug_id(value)
}

/// Validates one parsed digest proposal against the current digest proposal contract.
pub fn validate_digest_proposal(
    proposal: &DigestProposal,
    options: &ProposalValidationOptions<'_>,
) -> std::result::Result<ProposalValidationSummary, ProposalValidationErrors> {
    let mut errors = Vec::new();
    let allowed_categories = options
        .allowed_categories
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let allowed_domains = options
        .allowed_domains
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let allowed_evidence_refs = options
        .allowed_evidence_refs
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut proposed_entries = BTreeMap::new();

    if proposal.schema != DIGEST_PROPOSAL_SCHEMA {
        push_error(
            &mut errors,
            "schema",
            format!(
                "expected `{DIGEST_PROPOSAL_SCHEMA}`, found `{}`",
                proposal.schema
            ),
        );
    }
    if proposal.project_id != options.project_id {
        push_error(
            &mut errors,
            "project_id",
            format!(
                "expected `{}`, found `{}`",
                options.project_id, proposal.project_id
            ),
        );
    }
    if proposal.run_id != options.run_id {
        push_error(
            &mut errors,
            "run_id",
            format!("expected `{}`, found `{}`", options.run_id, proposal.run_id),
        );
    }

    validate_non_empty_string(
        &mut errors,
        "run_summary.title",
        &proposal.run_summary.title,
        "run summary title must not be empty",
    );
    validate_non_empty_string(
        &mut errors,
        "run_summary.summary",
        &proposal.run_summary.summary,
        "run summary summary must not be empty",
    );
    if proposal.run_summary.extracted_decision_count != proposal.entries.len() {
        push_error(
            &mut errors,
            "run_summary.extracted_decision_count",
            format!(
                "expected {} to match the number of proposed entries",
                proposal.entries.len()
            ),
        );
    }
    for (theme_index, theme) in proposal.run_summary.themes.iter().enumerate() {
        validate_non_empty_string(
            &mut errors,
            format!("run_summary.themes[{theme_index}]"),
            theme,
            "run summary theme must not be empty",
        );
    }

    for (entry_index, entry) in proposal.entries.iter().enumerate() {
        let base = format!("entries[{entry_index}]");
        let identity = proposal_entry_identity(entry);
        if let Some(first_index) = proposed_entries.insert(identity, entry_index) {
            push_error(
                &mut errors,
                base.clone(),
                format!("entry duplicates entries[{first_index}]"),
            );
        }
        validate_non_empty_string(
            &mut errors,
            format!("{base}.title"),
            &entry.title,
            "entry title must not be empty",
        );
        validate_non_empty_string(
            &mut errors,
            format!("{base}.category"),
            &entry.category,
            "entry category must not be empty",
        );
        if !allowed_categories.contains(entry.category.as_str()) {
            push_error(
                &mut errors,
                format!("{base}.category"),
                format!(
                    "category `{}` is not allowed for this project",
                    entry.category
                ),
            );
        }
        validate_non_empty_string(
            &mut errors,
            format!("{base}.decision_date"),
            &entry.decision_date,
            "entry decision date must not be empty",
        );
        if !is_valid_iso_date(&entry.decision_date) {
            push_error(
                &mut errors,
                format!("{base}.decision_date"),
                format!(
                    "decision date `{}` must use YYYY-MM-DD",
                    entry.decision_date
                ),
            );
        }
        validate_non_empty_string(
            &mut errors,
            format!("{base}.context"),
            &entry.context,
            "entry context must not be empty",
        );
        validate_non_empty_string(
            &mut errors,
            format!("{base}.final_decision"),
            &entry.final_decision,
            "entry final decision must not be empty",
        );
        validate_non_empty_string(
            &mut errors,
            format!("{base}.rationale"),
            &entry.rationale,
            "entry rationale must not be empty",
        );
        validate_non_empty_string(
            &mut errors,
            format!("{base}.consequences"),
            &entry.consequences,
            "entry consequences must not be empty",
        );
        if entry.options.is_empty() {
            push_error(
                &mut errors,
                format!("{base}.options"),
                "entry options must contain at least one option".to_owned(),
            );
        }
        if !entry
            .options
            .iter()
            .any(|option| matches!(option.status, DigestProposalOptionStatus::Chosen))
        {
            push_error(
                &mut errors,
                format!("{base}.options"),
                "entry options must include one chosen option".to_owned(),
            );
        }
        for (option_index, option) in entry.options.iter().enumerate() {
            validate_non_empty_string(
                &mut errors,
                format!("{base}.options[{option_index}].description"),
                &option.description,
                "entry option description must not be empty",
            );
        }
        if entry.evidence.is_empty() {
            push_error(
                &mut errors,
                format!("{base}.evidence"),
                "entry evidence must contain at least one reference".to_owned(),
            );
        }
        for (domain_index, domain) in entry.domains.iter().enumerate() {
            let path = format!("{base}.domains[{domain_index}]");
            validate_non_empty_string(
                &mut errors,
                path.clone(),
                domain,
                "entry domain must not be empty",
            );
            if !is_valid_domain_id(domain) {
                push_error(
                    &mut errors,
                    path.clone(),
                    format!("domain `{domain}` must use lowercase slug format"),
                );
            } else if !allowed_domains.contains(domain.as_str()) {
                push_error(
                    &mut errors,
                    path,
                    format!("domain `{domain}` is not allowed for this run"),
                );
            }
        }
        for (evidence_index, evidence) in entry.evidence.iter().enumerate() {
            let path = format!("{base}.evidence[{evidence_index}]");
            match parse_evidence_reference(evidence) {
                Some(_) => {
                    if !allowed_evidence_refs.contains(evidence.as_str()) {
                        push_error(
                            &mut errors,
                            path,
                            format!(
                                "evidence `{evidence}` must reference one of the selected session turns"
                            ),
                        );
                    }
                }
                None => push_error(
                    &mut errors,
                    path,
                    format!(
                        "evidence `{evidence}` must use `<provider>:<session-id>#<turn-ordinal>`"
                    ),
                ),
            }
        }
    }

    if errors.is_empty() {
        Ok(ProposalValidationSummary {
            entry_count: proposal.entries.len(),
            run_summary_title: proposal.run_summary.title.clone(),
            extracted_decision_count: proposal.run_summary.extracted_decision_count,
        })
    } else {
        Err(ProposalValidationErrors { errors })
    }
}

/// Stores the stable duplicate-detection key for one decision-trace proposal entry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ProposalEntryIdentity {
    title: String,
    category: String,
    domains: Vec<String>,
    decision_date: String,
    content_fingerprint: String,
}

/// Builds the duplicate-detection key for one proposal entry.
pub(crate) fn proposal_entry_identity(entry: &DigestProposalEntry) -> ProposalEntryIdentity {
    ProposalEntryIdentity {
        title: entry.title.trim().to_owned(),
        category: entry.category.trim().to_owned(),
        domains: normalize_identity_domains(&entry.domains),
        decision_date: entry.decision_date.trim().to_owned(),
        content_fingerprint: proposal_content_fingerprint(entry),
    }
}

/// Builds the duplicate-detection key for one existing canonical entry when possible.
pub(crate) fn existing_entry_identity(
    entry: &EntryFrontmatter,
    body_markdown: &str,
) -> Option<ProposalEntryIdentity> {
    Some(ProposalEntryIdentity {
        title: entry.title.trim().to_owned(),
        category: entry.category.trim().to_owned(),
        domains: normalize_identity_domains(&entry.domains),
        decision_date: entry.decision_date.as_ref()?.trim().to_owned(),
        content_fingerprint: existing_content_fingerprint(entry, body_markdown)?,
    })
}

/// Normalizes one domain list into a stable duplicate-detection ordering.
pub(crate) fn normalize_identity_domains(domains: &[String]) -> Vec<String> {
    let mut domains = domains
        .iter()
        .map(|domain| domain.trim().to_owned())
        .collect::<Vec<_>>();
    domains.sort();
    domains.dedup();
    domains
}

/// Validates one ISO date string against the fixed `YYYY-MM-DD` shape.
fn is_valid_iso_date(value: &str) -> bool {
    let mut parts = value.split('-');
    let Some(year) = parts.next() else {
        return false;
    };
    let Some(month) = parts.next() else {
        return false;
    };
    let Some(day) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && year.len() == 4
        && month.len() == 2
        && day.len() == 2
        && year.chars().all(|ch| ch.is_ascii_digit())
        && month.chars().all(|ch| ch.is_ascii_digit())
        && day.chars().all(|ch| ch.is_ascii_digit())
}

/// Pushes one structured validation error onto the proposal error accumulator.
fn push_error(errors: &mut Vec<ProposalValidationError>, path: impl Into<String>, message: String) {
    errors.push(ProposalValidationError {
        path: path.into(),
        message,
    });
}

/// Validates that one required string field is present after trimming.
fn validate_non_empty_string(
    errors: &mut Vec<ProposalValidationError>,
    path: impl Into<String>,
    value: &str,
    message: &str,
) {
    if value.trim().is_empty() {
        push_error(errors, path, message.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validation_options<'a>(
        allowed_evidence_refs: &'a [String],
        allowed_domains: &'a [String],
    ) -> ProposalValidationOptions<'a> {
        const CATEGORIES: &[&str] = &["architecture", "data", "product", "process"];
        let allowed_categories = CATEGORIES
            .iter()
            .map(|category| (*category).to_owned())
            .collect::<Vec<_>>();
        ProposalValidationOptions {
            project_id: "repo-abc123",
            run_id: "cwrun_01proposal",
            allowed_categories: Box::leak(allowed_categories.into_boxed_slice()),
            allowed_domains,
            allowed_evidence_refs,
        }
    }

    #[test]
    fn validation_allows_zero_entries_with_required_run_summary() {
        let allowed_evidence_refs = vec!["codex:session-1#0".to_owned()];
        let proposal = DigestProposal {
            schema: DIGEST_PROPOSAL_SCHEMA.to_owned(),
            project_id: "repo-abc123".to_owned(),
            run_id: "cwrun_01proposal".to_owned(),
            entries: Vec::new(),
            run_summary: DigestProposalRunSummary {
                title: "No durable decisions".to_owned(),
                summary: "The selected sessions did not contain stable decisions worth preserving."
                    .to_owned(),
                themes: vec!["refactoring".to_owned()],
                extracted_decision_count: 0,
            },
        };

        let summary =
            validate_digest_proposal(&proposal, &validation_options(&allowed_evidence_refs, &[]))
                .expect("proposal should be valid");
        assert_eq!(summary.entry_count, 0);
        assert_eq!(summary.extracted_decision_count, 0);
    }

    #[test]
    fn validation_rejects_invalid_domain_and_nonexistent_turn_reference() {
        let allowed_evidence_refs = vec!["codex:session-1#0".to_owned()];
        let allowed_domains = vec!["query-protocol".to_owned()];
        let proposal = DigestProposal {
            schema: DIGEST_PROPOSAL_SCHEMA.to_owned(),
            project_id: "repo-abc123".to_owned(),
            run_id: "cwrun_01proposal".to_owned(),
            entries: vec![DigestProposalEntry {
                operation: ProposalEntryOperation::Create,
                entry_type: EntryType::DecisionTrace,
                title: "Keep query protocol stable".to_owned(),
                category: "product".to_owned(),
                domains: vec!["Not Valid".to_owned()],
                decision_date: "2026-04-13".to_owned(),
                context: "Context".to_owned(),
                options: vec![DigestProposalOption {
                    status: DigestProposalOptionStatus::Chosen,
                    description: "Stay additive".to_owned(),
                }],
                final_decision: "Stay additive".to_owned(),
                rationale: "Desktop depends on it".to_owned(),
                consequences: "Protocol changes need migration".to_owned(),
                evidence: vec!["codex:session-1#9999".to_owned()],
            }],
            run_summary: DigestProposalRunSummary {
                title: "Summary".to_owned(),
                summary: "Summary".to_owned(),
                themes: Vec::new(),
                extracted_decision_count: 1,
            },
        };

        let errors = validate_digest_proposal(
            &proposal,
            &validation_options(&allowed_evidence_refs, &allowed_domains),
        )
        .expect_err("proposal should be invalid");
        assert!(
            errors
                .errors()
                .iter()
                .any(|error| error.path == "entries[0].domains[0]")
        );
        assert!(
            errors
                .errors()
                .iter()
                .any(|error| error.path == "entries[0].evidence[0]")
        );
    }

    #[test]
    fn validation_rejects_duplicate_entries_within_one_proposal() {
        let allowed_evidence_refs = vec!["codex:session-1#0".to_owned()];
        let allowed_domains = vec!["query-protocol".to_owned()];
        let duplicate_entry = DigestProposalEntry {
            operation: ProposalEntryOperation::Create,
            entry_type: EntryType::DecisionTrace,
            title: "Keep query protocol stable".to_owned(),
            category: "product".to_owned(),
            domains: vec!["query-protocol".to_owned()],
            decision_date: "2026-04-13".to_owned(),
            context: "Context".to_owned(),
            options: vec![DigestProposalOption {
                status: DigestProposalOptionStatus::Chosen,
                description: "Stay additive".to_owned(),
            }],
            final_decision: "Stay additive".to_owned(),
            rationale: "Desktop depends on it".to_owned(),
            consequences: "Protocol changes need migration".to_owned(),
            evidence: vec!["codex:session-1#0".to_owned()],
        };
        let proposal = DigestProposal {
            schema: DIGEST_PROPOSAL_SCHEMA.to_owned(),
            project_id: "repo-abc123".to_owned(),
            run_id: "cwrun_01proposal".to_owned(),
            entries: vec![duplicate_entry.clone(), duplicate_entry],
            run_summary: DigestProposalRunSummary {
                title: "Summary".to_owned(),
                summary: "Summary".to_owned(),
                themes: Vec::new(),
                extracted_decision_count: 2,
            },
        };

        let errors = validate_digest_proposal(
            &proposal,
            &validation_options(&allowed_evidence_refs, &allowed_domains),
        )
        .expect_err("duplicate entries should fail validation");
        assert!(errors.errors().iter().any(|error| {
            error.path == "entries[1]"
                && error.message.contains("duplicates")
                && error.message.contains("entries[0]")
        }));
    }

    #[test]
    fn validation_allows_entries_that_match_existing_canonical_identity() {
        let allowed_evidence_refs = vec!["codex:session-1#0".to_owned()];
        let allowed_domains = vec!["query-protocol".to_owned()];
        let existing_entry = EntryFrontmatter {
            schema_version: 1,
            entry_id: crate::EntryId::new("cw_existing-1").expect("entry id should be valid"),
            entry_type: EntryType::DecisionTrace,
            display_id: Some("DT-1".to_owned()),
            project_id: "repo-abc123".to_owned(),
            title: "Keep query protocol stable".to_owned(),
            category: "product".to_owned(),
            domains: vec!["query-protocol".to_owned()],
            status: crate::EntryStatus::Active,
            created_at: "2026-04-13T10:31:22Z".to_owned(),
            updated_at: "2026-04-13T10:31:22Z".to_owned(),
            decision_date: Some("2026-04-13".to_owned()),
            evidence: vec!["codex:session-1#0".to_owned()],
            content_fingerprint: None,
            created_by_run_id: crate::RunId::new("cwrun_existing-1")
                .expect("run id should be valid"),
            updated_by_run_id: crate::RunId::new("cwrun_existing-1")
                .expect("run id should be valid"),
            supersedes: Vec::new(),
        };
        let proposal = DigestProposal {
            schema: DIGEST_PROPOSAL_SCHEMA.to_owned(),
            project_id: "repo-abc123".to_owned(),
            run_id: "cwrun_01proposal".to_owned(),
            entries: vec![DigestProposalEntry {
                operation: ProposalEntryOperation::Create,
                entry_type: EntryType::DecisionTrace,
                title: "Keep query protocol stable".to_owned(),
                category: "product".to_owned(),
                domains: vec!["query-protocol".to_owned()],
                decision_date: "2026-04-13".to_owned(),
                context: "Context".to_owned(),
                options: vec![DigestProposalOption {
                    status: DigestProposalOptionStatus::Chosen,
                    description: "Stay additive".to_owned(),
                }],
                final_decision: "Stay additive".to_owned(),
                rationale: "Desktop depends on it".to_owned(),
                consequences: "Protocol changes need migration".to_owned(),
                evidence: vec!["codex:session-1#0".to_owned()],
            }],
            run_summary: DigestProposalRunSummary {
                title: "Summary".to_owned(),
                summary: "Summary".to_owned(),
                themes: Vec::new(),
                extracted_decision_count: 1,
            },
        };
        let _existing_entry = existing_entry;

        let summary = validate_digest_proposal(
            &proposal,
            &validation_options(&allowed_evidence_refs, &allowed_domains),
        )
        .expect("existing canonical identity should be merged later");
        assert_eq!(summary.entry_count, 1);
    }

    #[test]
    fn deserialization_rejects_unknown_fields() {
        let error = serde_json::from_str::<DigestProposal>(
            r#"{
                "schema": "darc.wiki.digest.proposal.v1",
                "project_id": "repo-abc123",
                "run_id": "cwrun_01proposal",
                "entries": [],
                "run_summary": {
                    "title": "Summary",
                    "summary": "Summary",
                    "themes": [],
                    "extracted_decision_count": 0
                },
                "unexpected": true
            }"#,
        )
        .expect_err("unknown fields should fail schema parsing");
        assert!(error.to_string().contains("unexpected"));
    }

    #[test]
    fn deserialization_rejects_missing_required_arrays() {
        let error = serde_json::from_str::<DigestProposal>(
            r#"{
                "schema": "darc.wiki.digest.proposal.v1",
                "project_id": "repo-abc123",
                "run_id": "cwrun_01proposal",
                "entries": [
                    {
                        "operation": "create",
                        "entry_type": "decision_trace",
                        "title": "Keep query protocol stable",
                        "category": "product",
                        "decision_date": "2026-04-13",
                        "context": "Context",
                        "options": [
                            {
                                "status": "chosen",
                                "description": "Stay additive"
                            }
                        ],
                        "final_decision": "Stay additive",
                        "rationale": "Desktop depends on it",
                        "consequences": "Protocol changes need migration",
                        "evidence": ["codex:session-1#0"]
                    }
                ],
                "run_summary": {
                    "title": "Summary",
                    "summary": "Summary",
                    "themes": [],
                    "extracted_decision_count": 1
                }
            }"#,
        )
        .expect_err("missing required arrays should fail schema parsing");
        assert!(error.to_string().contains("domains"));
    }
}
