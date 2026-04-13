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
