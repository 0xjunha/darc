use std::{io, path::PathBuf};

use thiserror::Error;

/// Represents one typed wiki storage error from the leaf wiki crate.
#[derive(Debug, Error)]
pub enum WikiError {
    /// Reports one invalid canonical wiki identifier.
    #[error("invalid wiki id `{value}`")]
    InvalidId { value: String },

    /// Reports one invalid project identifier before filesystem resolution.
    #[error("invalid project id `{value}`")]
    InvalidProjectId { value: String },

    /// Reports one invalid project-scoped registry category identifier.
    #[error("invalid registry category id `{value}` in {path}")]
    InvalidRegistryCategory { path: PathBuf, value: String },

    /// Reports one invalid project-scoped registry domain identifier.
    #[error("invalid registry domain id `{value}` in {path}")]
    InvalidRegistryDomain { path: PathBuf, value: String },

    /// Reports one missing Markdown frontmatter block.
    #[error("missing TOML frontmatter in {path}")]
    MissingFrontmatter { path: PathBuf },

    /// Reports one missing storage version marker in an existing wiki root.
    #[error("missing storage version marker at {path}")]
    MissingStorageVersion { path: PathBuf },

    /// Reports one unsupported storage layout version marker.
    #[error("unsupported storage version `{actual}` at {path}, expected `{expected}`")]
    UnsupportedStorageVersion {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    /// Reports one unsupported schema version in a persisted artifact file.
    #[error("unsupported schema version `{actual}` in {path}, expected `{expected}`")]
    UnsupportedSchemaVersion {
        path: PathBuf,
        expected: u32,
        actual: u32,
    },

    /// Reports one project mismatch between one stored entry and its enclosing project layout.
    #[error(
        "entry `{entry_id}` belongs to project `{actual_project_id}`, expected `{expected_project_id}`"
    )]
    EntryProjectMismatch {
        entry_id: String,
        expected_project_id: String,
        actual_project_id: String,
    },

    /// Reports one missing canonical entry within the requested project layout.
    #[error("entry `{entry_id}` was not found in project `{project_id}`")]
    EntryNotFound {
        entry_id: String,
        project_id: String,
    },

    /// Reports one unsupported lifecycle transition for an existing canonical entry.
    #[error("cannot change entry `{entry_id}` from `{current_status}` to `{target_status}`")]
    InvalidEntryStatusTransition {
        entry_id: String,
        current_status: String,
        target_status: String,
    },

    /// Reports one discarded entry that cannot be restored safely.
    #[error(
        "cannot restore entry `{entry_id}` because active entry `{conflicting_entry_id}` already has the same canonical identity"
    )]
    EntryRestoreConflict {
        entry_id: String,
        conflicting_entry_id: String,
    },

    /// Reports one entry whose canonical identity cannot be reconstructed for restore validation.
    #[error("cannot compute canonical identity for entry `{entry_id}`")]
    EntryIdentityUnavailable { entry_id: String },

    /// Reports one project mismatch between one stored digest and its enclosing project layout.
    #[error(
        "digest `{digest_id}` belongs to project `{actual_project_id}`, expected `{expected_project_id}`"
    )]
    DigestProjectMismatch {
        digest_id: String,
        expected_project_id: String,
        actual_project_id: String,
    },

    /// Reports one project mismatch between layout and persisted run state.
    #[error(
        "run `{run_id}` belongs to project `{actual_project_id}`, expected `{expected_project_id}`"
    )]
    RunProjectMismatch {
        run_id: String,
        expected_project_id: String,
        actual_project_id: String,
    },

    /// Reports one filesystem read error with path context.
    #[error("failed to read {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Reports one filesystem write error with path context.
    #[error("failed to write {path}: {source}")]
    WriteFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Reports one directory creation error with path context.
    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Reports one directory listing error with path context.
    #[error("failed to read directory {path}: {source}")]
    ReadDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// Reports one TOML parse error with path context.
    #[error("failed to parse TOML in {path}: {source}")]
    ParseToml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// Reports one TOML serialization error with path context.
    #[error("failed to serialize TOML for {path}: {source}")]
    SerializeToml {
        path: PathBuf,
        #[source]
        source: toml::ser::Error,
    },
}

/// Aliases the crate-local result type for wiki storage operations.
pub type Result<T> = std::result::Result<T, WikiError>;
