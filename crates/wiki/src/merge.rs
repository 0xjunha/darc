use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use darc_paths::current_utc_timestamp;
use serde::Serialize;

use crate::{
    DigestFrontmatter, DigestId, DigestProposal, DigestProposalEntry, EntryFrontmatter, EntryId,
    EntryStatus, EntryType, ProjectLayout, Result, RunId, WikiError, list_digests, list_entries,
    load_entry_detail,
};
use crate::{
    frontmatter::render_markdown_document,
    fs_utils::write_string_atomically,
    proposal::{
        ProposalEntryIdentity, existing_entry_identity, normalize_identity_domains,
        proposal_entry_identity,
    },
    render::{normalize_trimmed_list, render_decision_trace_body},
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

/// Classifies whether one merged decision trace was newly created or rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeEntryAction {
    Created,
    Updated,
}

/// Stores the project-scoped entry snapshot needed for one merge pass.
struct EntryMergeSnapshot {
    entries_by_identity: BTreeMap<ProposalEntryIdentity, ExistingCanonicalEntry>,
    occupied_entry_ids: BTreeSet<EntryId>,
    next_display_number: u64,
}

/// Stores one active canonical entry eligible for merge-time reuse.
struct ExistingCanonicalEntry {
    frontmatter: EntryFrontmatter,
}

/// Stores one staged Markdown write with rollback information.
struct PendingWrite {
    path: PathBuf,
    content: String,
    previous_content: Option<String>,
}

/// Merges one validated digest proposal into canonical entry and digest artifacts.
pub fn merge_digest_proposal(
    layout: &ProjectLayout,
    run_id: &RunId,
    proposal: &DigestProposal,
) -> Result<MergeDigestArtifacts> {
    layout.ensure()?;
    let _lock = lock_project_merge(layout)?;
    let now = current_utc_timestamp();
    let mut snapshot = load_entry_merge_snapshot(layout)?;
    let mut occupied_digest_ids = list_digests(layout)?
        .into_iter()
        .map(|digest| digest.digest_id)
        .collect::<BTreeSet<_>>();

    let mut created_entry_ids = Vec::new();
    let mut updated_entry_ids = Vec::new();
    let mut merged_entries = Vec::with_capacity(proposal.entries.len());
    let mut pending_writes = Vec::with_capacity(proposal.entries.len() + 1);

    for proposal_entry in &proposal.entries {
        let identity = proposal_entry_identity(proposal_entry);
        let body_markdown = render_decision_trace_body(proposal_entry);
        if let Some(existing) = snapshot.entries_by_identity.remove(&identity) {
            let display_id = ensure_display_id(
                existing.frontmatter.display_id.clone(),
                &mut snapshot.next_display_number,
            );
            let frontmatter = build_updated_entry_frontmatter(
                existing.frontmatter,
                proposal_entry,
                run_id,
                &now,
                display_id.clone(),
            );
            pending_writes.push(build_markdown_write(
                layout.entry_path(&frontmatter.category, &frontmatter.entry_id),
                &frontmatter,
                &body_markdown,
            )?);
            updated_entry_ids.push(frontmatter.entry_id.clone());
            merged_entries.push(MergedEntryRecord {
                entry_id: frontmatter.entry_id,
                display_id,
                title: frontmatter.title,
                action: MergeEntryAction::Updated,
            });
        } else {
            let entry_id = next_entry_id(&mut snapshot.occupied_entry_ids)?;
            let display_id = ensure_display_id(None, &mut snapshot.next_display_number);
            let frontmatter = build_new_entry_frontmatter(
                layout,
                run_id,
                proposal_entry,
                entry_id.clone(),
                &now,
                display_id.clone(),
            );
            pending_writes.push(build_markdown_write(
                layout.entry_path(&frontmatter.category, &frontmatter.entry_id),
                &frontmatter,
                &body_markdown,
            )?);
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
    pending_writes.push(build_markdown_write(
        layout.digest_path(&digest_id),
        &digest_frontmatter,
        &render_digest_body(proposal, &merged_entries),
    )?);
    apply_pending_writes(&pending_writes)?;

    Ok(MergeDigestArtifacts {
        digest_id,
        created_entry_ids,
        updated_entry_ids,
    })
}

/// Locks project-scoped canonical merge work so concurrent digest runs cannot interleave writes.
fn lock_project_merge(layout: &ProjectLayout) -> Result<std::fs::File> {
    let path = layout.project_merge_lock_path();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| WikiError::WriteFile {
            path: path.clone(),
            source,
        })?;
    file.lock()
        .map_err(|source| WikiError::WriteFile { path, source })?;
    Ok(file)
}

/// Loads one project-scoped snapshot of active canonical entries and display allocation state.
fn load_entry_merge_snapshot(layout: &ProjectLayout) -> Result<EntryMergeSnapshot> {
    let mut entries_by_identity = BTreeMap::new();
    let mut occupied_entry_ids = BTreeSet::new();
    let mut next_display_number = 1_u64;

    for entry in list_entries(layout)? {
        let document = load_entry_detail(&entry.path)?;
        occupied_entry_ids.insert(document.frontmatter.entry_id.clone());
        if let Some(display_number) = document
            .frontmatter
            .display_id
            .as_deref()
            .and_then(parse_display_number)
        {
            next_display_number = next_display_number.max(display_number + 1);
        }
        if document.frontmatter.status != EntryStatus::Active {
            continue;
        }
        if let Some(identity) =
            existing_entry_identity(&document.frontmatter, &document.body_markdown)
        {
            entries_by_identity
                .entry(identity)
                .or_insert(ExistingCanonicalEntry {
                    frontmatter: document.frontmatter,
                });
        }
    }

    Ok(EntryMergeSnapshot {
        entries_by_identity,
        occupied_entry_ids,
        next_display_number,
    })
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

/// Builds one updated canonical entry frontmatter without reviving discarded lineage.
fn build_updated_entry_frontmatter(
    mut existing: EntryFrontmatter,
    proposal_entry: &DigestProposalEntry,
    run_id: &RunId,
    now: &str,
    display_id: String,
) -> EntryFrontmatter {
    existing.display_id = Some(display_id);
    existing.title = proposal_entry.title.trim().to_owned();
    existing.category = proposal_entry.category.trim().to_owned();
    existing.domains = normalize_identity_domains(&proposal_entry.domains);
    existing.updated_at = now.to_owned();
    existing.decision_date = Some(proposal_entry.decision_date.trim().to_owned());
    existing.evidence = normalize_trimmed_list(&proposal_entry.evidence);
    existing.updated_by_run_id = run_id.clone();
    existing
}

/// Builds one staged Markdown write and snapshots any previous file content for rollback.
fn build_markdown_write<T>(
    path: PathBuf,
    frontmatter: &T,
    body_markdown: &str,
) -> Result<PendingWrite>
where
    T: Serialize,
{
    let previous_content = if path.exists() {
        Some(
            fs::read_to_string(&path).map_err(|source| WikiError::ReadFile {
                path: path.clone(),
                source,
            })?,
        )
    } else {
        None
    };
    Ok(PendingWrite {
        content: render_markdown_document(&path, frontmatter, body_markdown)?,
        path,
        previous_content,
    })
}

/// Applies staged Markdown writes and restores the previous on-disk state if any write fails.
fn apply_pending_writes(writes: &[PendingWrite]) -> Result<()> {
    let mut applied = Vec::with_capacity(writes.len());
    for write in writes {
        match write_string_atomically(&write.path, &write.content) {
            Ok(()) => applied.push(write),
            Err(error) => {
                rollback_pending_writes(&applied, &error)?;
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Restores the previous file contents for every already-applied staged write.
fn rollback_pending_writes(applied: &[&PendingWrite], original_error: &WikiError) -> Result<()> {
    for write in applied.iter().rev() {
        let rollback = match &write.previous_content {
            Some(previous_content) => write_string_atomically(&write.path, previous_content),
            None => remove_new_file(&write.path),
        };
        if let Err(rollback_error) = rollback {
            return Err(merge_rollback_error(
                &write.path,
                original_error,
                rollback_error,
            ));
        }
    }
    Ok(())
}

/// Removes one newly created file during merge rollback.
fn remove_new_file(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    fs::remove_file(path).map_err(|source| WikiError::WriteFile {
        path: path.clone(),
        source,
    })
}

/// Wraps one rollback failure with the original merge write error context.
fn merge_rollback_error(
    path: &std::path::Path,
    original_error: &WikiError,
    rollback_error: WikiError,
) -> WikiError {
    let message = format!(
        "merge write failed with `{original_error}` and rollback failed with `{rollback_error}`"
    );
    WikiError::WriteFile {
        path: path.to_path_buf(),
        source: io::Error::other(message),
    }
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
                    status: crate::DigestProposalOptionStatus::Chosen,
                    description: "Keep the protocol additive.".to_owned(),
                },
                DigestProposalOption {
                    status: crate::DigestProposalOptionStatus::Rejected,
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
    fn merge_creates_new_entry_when_matching_title_has_different_content() -> Result<()> {
        let (darc_root, layout) = build_layout("distinct-content")?;
        let first_run_id = RunId::new("cwrun_01mergebodya")?;
        let second_run_id = RunId::new("cwrun_01mergebodyb")?;
        let mut revised_entry = build_entry("Keep the query protocol additive");
        revised_entry.rationale =
            "The wire contract is already consumed by downstream tools.".to_owned();

        let _first = merge_digest_proposal(
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
            &build_proposal(&second_run_id, vec![revised_entry]),
        )?;

        assert_eq!(second.created_entry_ids.len(), 1);
        assert!(second.updated_entry_ids.is_empty());
        assert_eq!(list_entries(&layout)?.len(), 2);

        fs::remove_dir_all(&darc_root).expect("temporary test root should be removable");
        Ok(())
    }

    #[test]
    fn merge_does_not_revive_discarded_entries() -> Result<()> {
        let (darc_root, layout) = build_layout("discarded")?;
        let first_run_id = RunId::new("cwrun_01mergediscarda")?;
        let second_run_id = RunId::new("cwrun_01mergediscardb")?;
        let first = merge_digest_proposal(
            &layout,
            &first_run_id,
            &build_proposal(
                &first_run_id,
                vec![build_entry("Keep the query protocol additive")],
            ),
        )?;
        let mut discarded =
            load_entry_detail(&layout.entry_path("product", &first.created_entry_ids[0]))?;
        discarded.frontmatter.status = EntryStatus::Discarded;
        let discarded_write = build_markdown_write(
            layout.entry_path("product", &discarded.frontmatter.entry_id),
            &discarded.frontmatter,
            &discarded.body_markdown,
        )?;
        apply_pending_writes(&[discarded_write])?;

        let second = merge_digest_proposal(
            &layout,
            &second_run_id,
            &build_proposal(
                &second_run_id,
                vec![build_entry("Keep the query protocol additive")],
            ),
        )?;

        assert_eq!(second.created_entry_ids.len(), 1);
        assert!(second.updated_entry_ids.is_empty());
        assert_eq!(list_entries(&layout)?.len(), 2);

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
