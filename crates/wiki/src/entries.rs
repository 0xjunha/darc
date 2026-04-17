use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
};

use darc_paths::current_utc_timestamp;
use serde::{Deserialize, Serialize};

use crate::{
    EntryId, ProjectLayout, Result, RunId, WikiError,
    frontmatter::{
        load_markdown_frontmatter, load_markdown_frontmatter_and_body, render_markdown_document,
    },
    fs_utils::{collect_markdown_files, write_string_atomically},
    proposal::existing_entry_identity,
};

const ENTRY_SCHEMA_VERSION: u32 = 1;

/// Stores the canonical entry type supported by the current wiki MVP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    DecisionTrace,
}

/// Stores the lifecycle state for one canonical wiki entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryStatus {
    Active,
    Discarded,
    Superseded,
}

/// Stores the TOML frontmatter metadata for one canonical wiki entry file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryFrontmatter {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub entry_id: EntryId,
    pub entry_type: EntryType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_id: Option<String>,
    pub project_id: String,
    pub title: String,
    pub category: String,
    #[serde(default)]
    pub domains: Vec<String>,
    pub status: EntryStatus,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_date: Option<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_fingerprint: Option<String>,
    pub created_by_run_id: RunId,
    pub updated_by_run_id: RunId,
    #[serde(default)]
    pub supersedes: Vec<EntryId>,
}

/// Represents one canonical entry document on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryDocument {
    pub path: PathBuf,
    pub frontmatter: EntryFrontmatter,
}

/// Represents one canonical entry document with its Markdown body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryDetailDocument {
    pub path: PathBuf,
    pub frontmatter: EntryFrontmatter,
    pub body_markdown: String,
}

/// Stores the read-side summary for one canonical entry document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntrySummary {
    pub entry_id: EntryId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_id: Option<String>,
    pub entry_type: EntryType,
    pub project_id: String,
    pub title: String,
    pub category: String,
    #[serde(default)]
    pub domains: Vec<String>,
    pub status: EntryStatus,
    pub created_at: String,
    pub updated_at: String,
    pub path: PathBuf,
}

/// Reports one canonical entry status mutation applied through the write-side API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryStatusChange {
    pub entry_id: EntryId,
    pub previous_status: EntryStatus,
    pub status: EntryStatus,
    pub updated_at: String,
    pub changed: bool,
}

impl From<EntryDocument> for EntrySummary {
    fn from(document: EntryDocument) -> Self {
        Self {
            entry_id: document.frontmatter.entry_id,
            display_id: document.frontmatter.display_id,
            entry_type: document.frontmatter.entry_type,
            project_id: document.frontmatter.project_id,
            title: document.frontmatter.title,
            category: document.frontmatter.category,
            domains: document.frontmatter.domains,
            status: document.frontmatter.status,
            created_at: document.frontmatter.created_at,
            updated_at: document.frontmatter.updated_at,
            path: document.path,
        }
    }
}

/// Loads one canonical entry document from disk.
pub fn load_entry(path: &Path) -> Result<EntryDocument> {
    Ok(EntryDocument {
        path: path.to_path_buf(),
        frontmatter: load_markdown_frontmatter(path)?,
    })
}

/// Loads one canonical entry document plus its Markdown body from disk.
pub fn load_entry_detail(path: &Path) -> Result<EntryDetailDocument> {
    let (frontmatter, body_markdown) = load_markdown_frontmatter_and_body(path)?;
    Ok(EntryDetailDocument {
        path: path.to_path_buf(),
        frontmatter,
        body_markdown,
    })
}

/// Lists every canonical entry document for one project in deterministic order.
pub fn list_entries(layout: &ProjectLayout) -> Result<Vec<EntrySummary>> {
    layout.validate_storage()?;
    let mut entries = collect_markdown_files(&layout.entries_dir)?
        .into_iter()
        .map(|path| {
            let document = load_entry(&path)?;
            validate_entry_schema_version(&path, document.frontmatter.schema_version)?;
            validate_entry_project(layout, &document.frontmatter)?;
            Ok(EntrySummary::from(document))
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort_by(|left, right| left.entry_id.cmp(&right.entry_id));
    Ok(entries)
}

/// Lists every canonical entry document plus body content in deterministic order.
pub fn list_entry_details(layout: &ProjectLayout) -> Result<Vec<EntryDetailDocument>> {
    layout.validate_storage()?;
    let mut entries = collect_markdown_files(&layout.entries_dir)?
        .into_iter()
        .map(|path| {
            let document = load_entry_detail(&path)?;
            validate_entry_schema_version(&path, document.frontmatter.schema_version)?;
            validate_entry_project(layout, &document.frontmatter)?;
            Ok(document)
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort_by(|left, right| left.frontmatter.entry_id.cmp(&right.frontmatter.entry_id));
    Ok(entries)
}

/// Marks one active canonical entry as discarded without deleting its durable artifact.
pub fn discard_entry(layout: &ProjectLayout, entry_id: &EntryId) -> Result<EntryStatusChange> {
    mutate_entry_status(layout, entry_id, EntryStatus::Discarded)
}

/// Restores one discarded canonical entry to active when no active duplicate already exists.
pub fn restore_entry(layout: &ProjectLayout, entry_id: &EntryId) -> Result<EntryStatusChange> {
    mutate_entry_status(layout, entry_id, EntryStatus::Active)
}

/// Returns the fixed entry schema version.
fn default_schema_version() -> u32 {
    ENTRY_SCHEMA_VERSION
}

/// Validates one persisted entry schema version against the current implementation.
fn validate_entry_schema_version(path: &Path, schema_version: u32) -> Result<()> {
    if schema_version == ENTRY_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(crate::WikiError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            expected: ENTRY_SCHEMA_VERSION,
            actual: schema_version,
        })
    }
}

/// Validates that one stored entry belongs to the requested project layout.
fn validate_entry_project(layout: &ProjectLayout, frontmatter: &EntryFrontmatter) -> Result<()> {
    if frontmatter.project_id == layout.project_id {
        Ok(())
    } else {
        Err(crate::WikiError::EntryProjectMismatch {
            entry_id: frontmatter.entry_id.to_string(),
            expected_project_id: layout.project_id.clone(),
            actual_project_id: frontmatter.project_id.clone(),
        })
    }
}

/// Applies one validated lifecycle status change to an existing canonical entry.
fn mutate_entry_status(
    layout: &ProjectLayout,
    entry_id: &EntryId,
    target_status: EntryStatus,
) -> Result<EntryStatusChange> {
    layout.ensure()?;
    let _lock = lock_project_canonical_mutation(layout)?;
    let mut document = load_entry_detail_by_id(layout, entry_id)?;
    let previous_status = document.frontmatter.status;
    if previous_status == target_status {
        return Ok(EntryStatusChange {
            entry_id: document.frontmatter.entry_id,
            previous_status,
            status: target_status,
            updated_at: document.frontmatter.updated_at,
            changed: false,
        });
    }

    validate_status_transition(entry_id, previous_status, target_status)?;
    if target_status == EntryStatus::Active {
        ensure_restore_is_conflict_free(layout, &document)?;
    }

    let updated_at = current_utc_timestamp();
    document.frontmatter.status = target_status;
    document.frontmatter.updated_at = updated_at.clone();
    // Manual lifecycle commands do not create synthetic digest runs, so the last
    // content-writing `updated_by_run_id` remains the most recent merge provenance.
    write_entry_document(
        &document.path,
        &document.frontmatter,
        &document.body_markdown,
    )?;

    Ok(EntryStatusChange {
        entry_id: document.frontmatter.entry_id,
        previous_status,
        status: target_status,
        updated_at,
        changed: true,
    })
}

/// Loads one canonical entry document detail by its immutable entry identifier.
fn load_entry_detail_by_id(
    layout: &ProjectLayout,
    entry_id: &EntryId,
) -> Result<EntryDetailDocument> {
    let summary = list_entries(layout)?
        .into_iter()
        .find(|entry| entry.entry_id == *entry_id)
        .ok_or_else(|| WikiError::EntryNotFound {
            entry_id: entry_id.to_string(),
            project_id: layout.project_id.clone(),
        })?;
    load_entry_detail(&summary.path)
}

/// Validates that the requested entry lifecycle mutation is supported.
fn validate_status_transition(
    entry_id: &EntryId,
    current_status: EntryStatus,
    target_status: EntryStatus,
) -> Result<()> {
    let is_valid = matches!(
        (current_status, target_status),
        (EntryStatus::Active, EntryStatus::Discarded)
            | (EntryStatus::Discarded, EntryStatus::Active)
    );
    if is_valid {
        Ok(())
    } else {
        Err(WikiError::InvalidEntryStatusTransition {
            entry_id: entry_id.to_string(),
            current_status: entry_status_name(current_status).to_owned(),
            target_status: entry_status_name(target_status).to_owned(),
        })
    }
}

/// Rejects restores that would reactivate a duplicate canonical identity.
fn ensure_restore_is_conflict_free(
    layout: &ProjectLayout,
    document: &EntryDetailDocument,
) -> Result<()> {
    let restored_identity = existing_entry_identity(&document.frontmatter, &document.body_markdown)
        .ok_or_else(|| WikiError::EntryIdentityUnavailable {
            entry_id: document.frontmatter.entry_id.to_string(),
        })?;
    for summary in list_entries(layout)? {
        if summary.entry_id == document.frontmatter.entry_id
            || summary.status != EntryStatus::Active
        {
            continue;
        }
        let active_document = load_entry_detail(&summary.path)?;
        if existing_entry_identity(&active_document.frontmatter, &active_document.body_markdown)
            .as_ref()
            == Some(&restored_identity)
        {
            return Err(WikiError::EntryRestoreConflict {
                entry_id: document.frontmatter.entry_id.to_string(),
                conflicting_entry_id: active_document.frontmatter.entry_id.to_string(),
            });
        }
    }
    Ok(())
}

/// Writes one canonical entry document back to disk after frontmatter mutation.
fn write_entry_document(
    path: &Path,
    frontmatter: &EntryFrontmatter,
    body_markdown: &str,
) -> Result<()> {
    let content = render_markdown_document(path, frontmatter, body_markdown)?;
    write_string_atomically(path, &content)
}

/// Locks project-scoped canonical entry mutations against concurrent digest merges.
fn lock_project_canonical_mutation(layout: &ProjectLayout) -> Result<std::fs::File> {
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

/// Returns the stable serialized label for one canonical entry status.
fn entry_status_name(status: EntryStatus) -> &'static str {
    match status {
        EntryStatus::Active => "active",
        EntryStatus::Discarded => "discarded",
        EntryStatus::Superseded => "superseded",
    }
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
    use crate::ContextWikiLayout;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_test_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!(
            "darc-wiki-entry-{label}-{}-{nanos}-{counter}",
            std::process::id()
        ))
    }

    fn build_layout(label: &str) -> Result<(PathBuf, ProjectLayout)> {
        let darc_root = unique_test_dir(label);
        let layout = ContextWikiLayout::new(&darc_root).project_layout("repo-123")?;
        layout.ensure()?;
        Ok((darc_root, layout))
    }

    fn write_entry_fixture(
        layout: &ProjectLayout,
        entry_id: &str,
        display_id: &str,
        status: EntryStatus,
    ) -> Result<EntryId> {
        let entry_id = EntryId::new(entry_id)?;
        let frontmatter = EntryFrontmatter {
            schema_version: ENTRY_SCHEMA_VERSION,
            entry_id: entry_id.clone(),
            entry_type: EntryType::DecisionTrace,
            display_id: Some(display_id.to_owned()),
            project_id: layout.project_id.clone(),
            title: "Keep the query protocol additive".to_owned(),
            category: "product".to_owned(),
            domains: vec!["query-protocol".to_owned()],
            status,
            created_at: "2026-04-13T10:00:00Z".to_owned(),
            updated_at: "2026-04-13T10:00:00Z".to_owned(),
            decision_date: Some("2026-04-13".to_owned()),
            evidence: vec!["codex:session-1#0".to_owned()],
            content_fingerprint: None,
            created_by_run_id: RunId::new("cwrun_01entrytests")?,
            updated_by_run_id: RunId::new("cwrun_01entrytests")?,
            supersedes: Vec::new(),
        };
        write_entry_document(
            &layout.entry_path("product", &entry_id),
            &frontmatter,
            canonical_entry_body(),
        )?;
        Ok(entry_id)
    }

    fn canonical_entry_body() -> &'static str {
        concat!(
            "## Context\n\n",
            "Desktop already consumes the current query surface.\n\n",
            "## Options Considered\n\n",
            "1. Chosen: Keep the protocol additive.\n",
            "2. Rejected: Ship breaking changes.\n\n",
            "## Final Decision\n\n",
            "Keep the protocol additive.\n\n",
            "## Rationale\n\n",
            "Downstream clients already depend on the current shape.\n\n",
            "## Consequences\n\n",
            "Future protocol work must stay additive.\n\n",
            "## Evidence\n\n",
            "- `codex:session-1#0`\n"
        )
    }

    #[test]
    fn discard_and_restore_update_entry_status_in_place() -> Result<()> {
        let (darc_root, layout) = build_layout("round-trip")?;
        let entry_id =
            write_entry_fixture(&layout, "cw_01entryroundtrip", "DT-1", EntryStatus::Active)?;

        let discarded = discard_entry(&layout, &entry_id)?;
        assert_eq!(discarded.previous_status, EntryStatus::Active);
        assert_eq!(discarded.status, EntryStatus::Discarded);
        assert!(discarded.changed);

        let discarded_entry = load_entry_detail(&layout.entry_path("product", &entry_id))?;
        assert_eq!(discarded_entry.frontmatter.status, EntryStatus::Discarded);
        assert_eq!(
            discarded_entry.frontmatter.updated_by_run_id,
            RunId::new("cwrun_01entrytests")?
        );

        let restored = restore_entry(&layout, &entry_id)?;
        assert_eq!(restored.previous_status, EntryStatus::Discarded);
        assert_eq!(restored.status, EntryStatus::Active);
        assert!(restored.changed);

        let restored_entry = load_entry_detail(&layout.entry_path("product", &entry_id))?;
        assert_eq!(restored_entry.frontmatter.status, EntryStatus::Active);

        fs::remove_dir_all(&darc_root).expect("temporary test root should be removable");
        Ok(())
    }

    #[test]
    fn restore_rejects_duplicate_active_identity() -> Result<()> {
        let (darc_root, layout) = build_layout("restore-conflict")?;
        let active_entry_id =
            write_entry_fixture(&layout, "cw_01entryactive", "DT-1", EntryStatus::Active)?;
        let discarded_entry_id = write_entry_fixture(
            &layout,
            "cw_01entrydiscarded",
            "DT-2",
            EntryStatus::Discarded,
        )?;

        let error = restore_entry(&layout, &discarded_entry_id).unwrap_err();
        assert!(matches!(
            error,
            WikiError::EntryRestoreConflict {
                conflicting_entry_id,
                ..
            } if conflicting_entry_id == active_entry_id.to_string()
        ));

        fs::remove_dir_all(&darc_root).expect("temporary test root should be removable");
        Ok(())
    }

    #[test]
    fn discard_rejects_superseded_entries() -> Result<()> {
        let (darc_root, layout) = build_layout("discard-superseded")?;
        let entry_id = write_entry_fixture(
            &layout,
            "cw_01entrysuperseded",
            "DT-1",
            EntryStatus::Superseded,
        )?;

        let error = discard_entry(&layout, &entry_id).unwrap_err();
        assert!(matches!(
            error,
            WikiError::InvalidEntryStatusTransition { .. }
        ));

        fs::remove_dir_all(&darc_root).expect("temporary test root should be removable");
        Ok(())
    }
}
