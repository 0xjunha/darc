use std::{io, path::PathBuf};

use thiserror::Error;

/// Represents one typed wiki storage error from the leaf wiki crate.
#[derive(Debug, Error)]
pub enum WikiError {
    /// Reports one invalid canonical wiki identifier.
    #[error("invalid wiki id `{value}`")]
    InvalidId { value: String },

    /// Reports one missing Markdown frontmatter block.
    #[error("missing TOML frontmatter in {path}")]
    MissingFrontmatter { path: PathBuf },

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
