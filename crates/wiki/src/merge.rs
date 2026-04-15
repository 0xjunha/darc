use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use darc_paths::current_utc_timestamp;

use crate::{
    DigestFrontmatter, DigestId, DigestProposal, DigestProposalEntry, DigestProposalOption,
    DigestProposalOptionStatus, EntryFrontmatter, EntryId, EntryStatus, EntryType, ProjectLayout,
    Result, RunId, list_digests, list_entries, load_entry,
};
use crate::{
    digests::store_digest,
    entries::store_entry,
    proposal::{
        ProposalEntryIdentity, existing_entry_identity, normalize_identity_domains,
        proposal_entry_identity,
    },
};

static MERGE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Reports the canonical artifacts created or updated by one merged digest run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeDigestArtifacts {
    pub digest_id: DigestId,
    pub created_entry_ids: Vec<EntryId>,
    pub updated_entry_ids: Vec<EntryId>,
}

/// Records one merged decision trace for digest report rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MergedEntryRecord {
    entry_id: EntryId,
    display_id: String,
    title: String,
    action: MergeEntryAction,
}

/// Classifies whether one merged decision trace was newly created or reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeEntryAction {
    Created,
    Updated,
}

/// Merges one validated digest proposal into canonical entry and digest artifacts.
pub fn merge_digest_proposal(
    layout: &ProjectLayout,
    run_id: &RunId,
    proposal: &DigestProposal,
) -> Result<MergeDigestArtifacts> {
    layout.ensure()?;
    let now = current_utc_timestamp();
    let mut existing_entries = load_existing_entries_by_identity(layout)?;
    let mut occupied_entry_ids = list_entries(layout)?
        .into_iter()
        .map(|entry| entry.entry_id)
        .collect::<BTreeSet<_>>();
    let mut occupied_digest_ids = list_digests(layout)?
        .into_iter()
        .map(|digest| digest.digest_id)
        .collect::<BTreeSet<_>>();
    let mut next_display_number = next_display_number(layout)?;

    let mut created_entry_ids = Vec::new();
    let mut updated_entry_ids = Vec::new();
    let mut merged_entries = Vec::with_capacity(proposal.entries.len());
    for proposal_entry in &proposal.entries {
        let identity = proposal_entry_identity(proposal_entry);
        if let Some(mut existing) = existing_entries.remove(&identity) {
            let display_id =
                ensure_display_id(existing.display_id.take(), &mut next_display_number);
            let frontmatter = build_updated_entry_frontmatter(
                existing,
                proposal_entry,
                run_id,
                &now,
                display_id.clone(),
            );
            let body_markdown = render_entry_body(proposal_entry);
            store_entry(layout, &frontmatter, &body_markdown)?;
            updated_entry_ids.push(frontmatter.entry_id.clone());
            merged_entries.push(MergedEntryRecord {
                entry_id: frontmatter.entry_id,
                display_id,
                title: frontmatter.title,
                action: MergeEntryAction::Updated,
            });
        } else {
            let entry_id = next_entry_id(&mut occupied_entry_ids)?;
            let display_id = ensure_display_id(None, &mut next_display_number);
            let frontmatter = build_new_entry_frontmatter(
                layout,
                run_id,
                proposal_entry,
                entry_id.clone(),
                &now,
                display_id.clone(),
            );
            let body_markdown = render_entry_body(proposal_entry);
            store_entry(layout, &frontmatter, &body_markdown)?;
            created_entry_ids.push(entry_id.clone());
            merged_entries.push(MergedEntryRecord {
                entry_id,
                display_id,
                title: frontmatter.title,
                action: MergeEntryAction::Created,
            });
        }
    }

    let digest_id = next_digest_id(&mut occupied_digest_ids)?;
    let digest_frontmatter = DigestFrontmatter {
        schema_version: 1,
        digest_id: digest_id.clone(),
        project_id: layout.project_id.clone(),
        run_id: run_id.clone(),
        title: proposal.run_summary.title.trim().to_owned(),
        created_at: now.clone(),
        updated_at: now,
        extracted_decision_count: proposal.run_summary.extracted_decision_count,
    };
    let digest_body = render_digest_body(proposal, &merged_entries);
    store_digest(layout, &digest_frontmatter, &digest_body)?;

    Ok(MergeDigestArtifacts {
        digest_id,
        created_entry_ids,
        updated_entry_ids,
    })
}

/// Loads existing decision-trace frontmatter keyed by merge identity.
fn load_existing_entries_by_identity(
    layout: &ProjectLayout,
) -> Result<BTreeMap<ProposalEntryIdentity, EntryFrontmatter>> {
    let mut entries = BTreeMap::new();
    for frontmatter in list_entries(layout)?
        .into_iter()
        .map(|summary| load_entry(&summary.path).map(|document| document.frontmatter))
        .collect::<Result<Vec<_>>>()?
    {
        if let Some(identity) = existing_entry_identity(&frontmatter) {
            entries.entry(identity).or_insert(frontmatter);
        }
    }
    Ok(entries)
}

/// Builds one new canonical entry frontmatter from a validated proposal row.
fn build_new_entry_frontmatter(
    layout: &ProjectLayout,
    run_id: &RunId,
    proposal_entry: &DigestProposalEntry,
    entry_id: EntryId,
    now: &str,
    display_id: String,
) -> EntryFrontmatter {
    EntryFrontmatter {
        schema_version: 1,
        entry_id,
        entry_type: EntryType::DecisionTrace,
        display_id: Some(display_id),
        project_id: layout.project_id.clone(),
        title: proposal_entry.title.trim().to_owned(),
        category: proposal_entry.category.trim().to_owned(),
        domains: normalize_identity_domains(&proposal_entry.domains),
        status: EntryStatus::Active,
        created_at: now.to_owned(),
        updated_at: now.to_owned(),
        decision_date: Some(proposal_entry.decision_date.trim().to_owned()),
        evidence: normalize_trimmed_list(&proposal_entry.evidence),
        created_by_run_id: run_id.clone(),
        updated_by_run_id: run_id.clone(),
        supersedes: Vec::new(),
    }
}

/// Builds one updated canonical entry frontmatter by preserving immutable identity fields.
fn build_updated_entry_frontmatter(
    mut existing: EntryFrontmatter,
    proposal_entry: &DigestProposalEntry,
    run_id: &RunId,
    now: &str,
    display_id: String,
) -> EntryFrontmatter {
    existing.entry_type = EntryType::DecisionTrace;
    existing.display_id = Some(display_id);
    existing.title = proposal_entry.title.trim().to_owned();
    existing.category = proposal_entry.category.trim().to_owned();
    existing.domains = normalize_identity_domains(&proposal_entry.domains);
    existing.status = EntryStatus::Active;
    existing.updated_at = now.to_owned();
    existing.decision_date = Some(proposal_entry.decision_date.trim().to_owned());
    existing.evidence = normalize_trimmed_list(&proposal_entry.evidence);
    existing.updated_by_run_id = run_id.clone();
    existing.supersedes.clear();
    existing
}

/// Renders one canonical decision-trace Markdown body from a validated proposal row.
fn render_entry_body(proposal_entry: &DigestProposalEntry) -> String {
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

/// Renders one canonical digest summary Markdown body from the merged proposal artifacts.
fn render_digest_body(proposal: &DigestProposal, merged_entries: &[MergedEntryRecord]) -> String {
    format!(
        concat!(
            "## Summary\n\n",
            "{summary}\n\n",
            "## Themes\n\n",
            "{themes}\n\n",
            "## Decision Trace Entries\n\n",
            "{entries}\n"
        ),
        summary = proposal.run_summary.summary.trim(),
        themes = render_digest_themes(&proposal.run_summary.themes),
        entries = render_digest_entries(merged_entries),
    )
}

/// Renders the numbered options section for one decision trace.
fn render_options_markdown(options: &[DigestProposalOption]) -> String {
    options
        .iter()
        .enumerate()
        .map(|(index, option)| {
            format!(
                "{}. {}: {}",
                index + 1,
                option_status_label(option.status),
                option.description.trim()
            )
        })
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

/// Renders the digest theme list or a zero-theme placeholder.
fn render_digest_themes(themes: &[String]) -> String {
    let themes = normalize_trimmed_list(themes);
    if themes.is_empty() {
        "No dominant cross-session themes were extracted.".to_owned()
    } else {
        themes
            .iter()
            .map(|theme| format!("- {theme}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Renders the digest entry list or a zero-entry placeholder.
fn render_digest_entries(entries: &[MergedEntryRecord]) -> String {
    if entries.is_empty() {
        "No durable decision-trace entries were created or updated for this run.".to_owned()
    } else {
        entries
            .iter()
            .map(|entry| {
                format!(
                    "- {}: {} (`{}`; {})",
                    entry.display_id,
                    entry.title,
                    entry.entry_id,
                    merge_action_label(entry.action)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Returns the human-readable label for one proposal option status.
fn option_status_label(status: DigestProposalOptionStatus) -> &'static str {
    match status {
        DigestProposalOptionStatus::Chosen => "Chosen",
        DigestProposalOptionStatus::Rejected => "Rejected",
    }
}

/// Returns the human-readable label for one merge action.
fn merge_action_label(action: MergeEntryAction) -> &'static str {
    match action {
        MergeEntryAction::Created => "created",
        MergeEntryAction::Updated => "updated",
    }
}

/// Returns one display id, allocating the next `DT-N` number when necessary.
fn ensure_display_id(existing: Option<String>, next_display_number: &mut u64) -> String {
    existing.unwrap_or_else(|| {
        let display_id = format!("DT-{next_display_number}");
        *next_display_number += 1;
        display_id
    })
}

/// Returns the next available `DT-N` number for one project layout.
fn next_display_number(layout: &ProjectLayout) -> Result<u64> {
    Ok(list_entries(layout)?
        .into_iter()
        .filter_map(|entry| entry.display_id)
        .filter_map(|display_id| parse_display_number(&display_id))
        .max()
        .unwrap_or(0)
        + 1)
}

/// Parses one `DT-N` display id into its numeric suffix.
fn parse_display_number(value: &str) -> Option<u64> {
    value.strip_prefix("DT-")?.parse::<u64>().ok()
}

/// Allocates the next globally unique canonical entry id.
fn next_entry_id(occupied_ids: &mut BTreeSet<EntryId>) -> Result<EntryId> {
    loop {
        let candidate = EntryId::new(generate_prefixed_id("cw_"))?;
        if occupied_ids.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
}

/// Allocates the next globally unique canonical digest id.
fn next_digest_id(occupied_ids: &mut BTreeSet<DigestId>) -> Result<DigestId> {
    loop {
        let candidate = DigestId::new(generate_prefixed_id("dg_"))?;
        if occupied_ids.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
}

/// Generates one time-scoped ASCII identifier for the requested wiki prefix.
fn generate_prefixed_id(prefix: &str) -> String {
    let counter = MERGE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{prefix}{:08x}{:08x}{:04x}",
        now.as_secs(),
        now.subsec_nanos(),
        counter as u16
    )
}

/// Normalizes one trimmed list while preserving first-seen order.
fn normalize_trimmed_list(values: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() || normalized.iter().any(|existing| existing == value) {
            continue;
        }
        normalized.push(value.to_owned());
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::{
        ContextWikiLayout, DigestProposalOption, DigestProposalRunSummary, ProposalEntryOperation,
        load_digest_detail, load_entry_detail,
    };

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_test_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!(
            "darc-wiki-merge-{label}-{}-{nanos}-{counter}",
            std::process::id()
        ))
    }

    fn build_layout(label: &str) -> Result<(PathBuf, ProjectLayout)> {
        let darc_root = unique_test_dir(label);
        let layout = ContextWikiLayout::new(&darc_root).project_layout("repo-123")?;
        layout.ensure()?;
        Ok((darc_root, layout))
    }

    fn build_proposal(run_id: &RunId, entries: Vec<DigestProposalEntry>) -> DigestProposal {
        DigestProposal {
            schema: crate::DIGEST_PROPOSAL_SCHEMA.to_owned(),
            project_id: "repo-123".to_owned(),
            run_id: run_id.to_string(),
            run_summary: DigestProposalRunSummary {
                title: "Digest summary".to_owned(),
                summary: "The run captured durable decisions.".to_owned(),
                themes: vec!["query stability".to_owned()],
                extracted_decision_count: entries.len(),
            },
            entries,
        }
    }

    fn build_entry(title: &str) -> DigestProposalEntry {
        DigestProposalEntry {
            operation: ProposalEntryOperation::Create,
            entry_type: EntryType::DecisionTrace,
            title: title.to_owned(),
            category: "product".to_owned(),
            domains: vec!["query-protocol".to_owned()],
            decision_date: "2026-04-13".to_owned(),
            context: "The session discussed stable contracts.".to_owned(),
            options: vec![
                DigestProposalOption {
                    status: DigestProposalOptionStatus::Chosen,
                    description: "Keep the protocol additive.".to_owned(),
                },
                DigestProposalOption {
                    status: DigestProposalOptionStatus::Rejected,
                    description: "Ship breaking protocol changes.".to_owned(),
                },
            ],
            final_decision: "Keep the protocol additive.".to_owned(),
            rationale: "Desktop already depends on the current read-side shape.".to_owned(),
            consequences: "Future changes require additive migrations.".to_owned(),
            evidence: vec!["codex:session-1#0".to_owned()],
        }
    }

    #[test]
    fn merge_creates_canonical_entry_and_digest_documents() -> Result<()> {
        let (darc_root, layout) = build_layout("create")?;
        let run_id = RunId::new("cwrun_01mergecreate")?;
        let proposal = build_proposal(
            &run_id,
            vec![build_entry("Keep the query protocol additive")],
        );

        let merge = merge_digest_proposal(&layout, &run_id, &proposal)?;

        assert_eq!(merge.created_entry_ids.len(), 1);
        assert!(merge.updated_entry_ids.is_empty());

        let entry = load_entry_detail(&layout.entry_path("product", &merge.created_entry_ids[0]))?;
        assert_eq!(entry.frontmatter.display_id.as_deref(), Some("DT-1"));
        assert!(entry.body_markdown.contains("## Final Decision"));
        assert!(entry.body_markdown.contains("## Evidence"));

        let digest = load_digest_detail(&layout.digest_path(&merge.digest_id))?;
        assert_eq!(digest.frontmatter.extracted_decision_count, 1);
        assert!(digest.body_markdown.contains("## Decision Trace Entries"));
        assert!(digest.body_markdown.contains("created"));

        fs::remove_dir_all(&darc_root).expect("temporary test root should be removable");
        Ok(())
    }

    #[test]
    fn merge_reuses_existing_identity_instead_of_creating_a_second_entry() -> Result<()> {
        let (darc_root, layout) = build_layout("reuse")?;
        let first_run_id = RunId::new("cwrun_01mergefirst")?;
        let second_run_id = RunId::new("cwrun_01mergesecond")?;
        let first = merge_digest_proposal(
            &layout,
            &first_run_id,
            &build_proposal(
                &first_run_id,
                vec![build_entry("Keep the query protocol additive")],
            ),
        )?;

        let second = merge_digest_proposal(
            &layout,
            &second_run_id,
            &build_proposal(
                &second_run_id,
                vec![build_entry("Keep the query protocol additive")],
            ),
        )?;

        assert_eq!(first.created_entry_ids.len(), 1);
        assert!(second.created_entry_ids.is_empty());
        assert_eq!(second.updated_entry_ids, first.created_entry_ids);
        assert_eq!(list_entries(&layout)?.len(), 1);

        let entry = load_entry_detail(&layout.entry_path("product", &second.updated_entry_ids[0]))?;
        assert_eq!(entry.frontmatter.display_id.as_deref(), Some("DT-1"));
        assert_eq!(entry.frontmatter.updated_by_run_id, second_run_id);

        fs::remove_dir_all(&darc_root).expect("temporary test root should be removable");
        Ok(())
    }

    #[test]
    fn merge_still_writes_a_digest_when_no_decision_traces_are_proposed() -> Result<()> {
        let (darc_root, layout) = build_layout("zero")?;
        let run_id = RunId::new("cwrun_01mergezero")?;
        let proposal = build_proposal(&run_id, Vec::new());

        let merge = merge_digest_proposal(&layout, &run_id, &proposal)?;

        assert!(merge.created_entry_ids.is_empty());
        assert!(merge.updated_entry_ids.is_empty());
        assert!(list_entries(&layout)?.is_empty());

        let digest = load_digest_detail(&layout.digest_path(&merge.digest_id))?;
        assert_eq!(digest.frontmatter.extracted_decision_count, 0);
        assert!(
            digest
                .body_markdown
                .contains("No durable decision-trace entries")
        );

        fs::remove_dir_all(&darc_root).expect("temporary test root should be removable");
        Ok(())
    }
}
