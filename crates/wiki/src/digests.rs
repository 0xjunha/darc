use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    DigestId, ProjectLayout, Result, RunId, frontmatter::load_markdown_frontmatter,
    fs_utils::collect_markdown_files,
};

const DIGEST_SCHEMA_VERSION: u32 = 1;

/// Stores the TOML frontmatter metadata for one canonical digest report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestFrontmatter {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub digest_id: DigestId,
    pub project_id: String,
    pub run_id: RunId,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub extracted_decision_count: usize,
}

/// Represents one canonical digest report document on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestDocument {
    pub path: PathBuf,
    pub frontmatter: DigestFrontmatter,
}

/// Stores the read-side summary for one canonical digest report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestSummary {
    pub digest_id: DigestId,
    pub project_id: String,
    pub run_id: RunId,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub extracted_decision_count: usize,
    pub path: PathBuf,
}

impl From<DigestDocument> for DigestSummary {
    fn from(document: DigestDocument) -> Self {
        Self {
            digest_id: document.frontmatter.digest_id,
            project_id: document.frontmatter.project_id,
            run_id: document.frontmatter.run_id,
            title: document.frontmatter.title,
            created_at: document.frontmatter.created_at,
            updated_at: document.frontmatter.updated_at,
            extracted_decision_count: document.frontmatter.extracted_decision_count,
            path: document.path,
        }
    }
}

/// Loads one canonical digest report from disk.
pub fn load_digest(path: &Path) -> Result<DigestDocument> {
    Ok(DigestDocument {
        path: path.to_path_buf(),
        frontmatter: load_markdown_frontmatter(path)?,
    })
}

/// Lists every canonical digest report for one project in deterministic order.
pub fn list_digests(layout: &ProjectLayout) -> Result<Vec<DigestSummary>> {
    let mut digests = collect_markdown_files(&layout.digests_dir)?
        .into_iter()
        .map(|path| load_digest(&path).map(DigestSummary::from))
        .collect::<Result<Vec<_>>>()?;
    digests.sort_by(|left, right| left.digest_id.cmp(&right.digest_id));
    Ok(digests)
}

/// Returns the fixed digest report schema version.
fn default_schema_version() -> u32 {
    DIGEST_SCHEMA_VERSION
}
