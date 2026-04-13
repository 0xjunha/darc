use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    EntryId, ProjectLayout, Result, RunId, frontmatter::load_markdown_frontmatter,
    fs_utils::collect_markdown_files,
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

/// Lists every canonical entry document for one project in deterministic order.
pub fn list_entries(layout: &ProjectLayout) -> Result<Vec<EntrySummary>> {
    let mut entries = collect_markdown_files(&layout.entries_dir)?
        .into_iter()
        .map(|path| load_entry(&path).map(EntrySummary::from))
        .collect::<Result<Vec<_>>>()?;
    entries.sort_by(|left, right| left.entry_id.cmp(&right.entry_id));
    Ok(entries)
}

/// Returns the fixed entry schema version.
fn default_schema_version() -> u32 {
    ENTRY_SCHEMA_VERSION
}
