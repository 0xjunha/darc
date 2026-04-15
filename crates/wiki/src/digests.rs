use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    DigestId, ProjectLayout, Result, RunId,
    frontmatter::{load_markdown_frontmatter, load_markdown_frontmatter_and_body},
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

/// Represents one canonical digest document with its Markdown body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestDetailDocument {
    pub path: PathBuf,
    pub frontmatter: DigestFrontmatter,
    pub body_markdown: String,
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

/// Loads one canonical digest report plus its Markdown body from disk.
pub fn load_digest_detail(path: &Path) -> Result<DigestDetailDocument> {
    let (frontmatter, body_markdown) = load_markdown_frontmatter_and_body(path)?;
    Ok(DigestDetailDocument {
        path: path.to_path_buf(),
        frontmatter,
        body_markdown,
    })
}

/// Lists every canonical digest report for one project in deterministic order.
pub fn list_digests(layout: &ProjectLayout) -> Result<Vec<DigestSummary>> {
    layout.validate_storage()?;
    let mut digests = collect_markdown_files(&layout.digests_dir)?
        .into_iter()
        .map(|path| {
            let document = load_digest(&path)?;
            validate_digest_schema_version(&path, document.frontmatter.schema_version)?;
            validate_digest_project(layout, &document.frontmatter)?;
            Ok(DigestSummary::from(document))
        })
        .collect::<Result<Vec<_>>>()?;
    digests.sort_by(|left, right| left.digest_id.cmp(&right.digest_id));
    Ok(digests)
}

/// Returns the fixed digest report schema version.
fn default_schema_version() -> u32 {
    DIGEST_SCHEMA_VERSION
}

/// Validates one persisted digest schema version against the current implementation.
fn validate_digest_schema_version(path: &Path, schema_version: u32) -> Result<()> {
    if schema_version == DIGEST_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(crate::WikiError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            expected: DIGEST_SCHEMA_VERSION,
            actual: schema_version,
        })
    }
}

/// Validates that one stored digest belongs to the requested project layout.
fn validate_digest_project(layout: &ProjectLayout, frontmatter: &DigestFrontmatter) -> Result<()> {
    if frontmatter.project_id == layout.project_id {
        Ok(())
    } else {
        Err(crate::WikiError::DigestProjectMismatch {
            digest_id: frontmatter.digest_id.to_string(),
            expected_project_id: layout.project_id.clone(),
            actual_project_id: frontmatter.project_id.clone(),
        })
    }
}
