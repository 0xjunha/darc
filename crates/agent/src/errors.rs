use std::{io, path::PathBuf};

use thiserror::Error;

/// Represents one typed runtime preparation error from the leaf agent crate.
#[derive(Debug, Error)]
pub enum AgentError {
    /// Reports one unsupported digest agent identifier.
    #[error("unsupported digest agent `{value}`")]
    UnsupportedAgent { value: String },

    /// Reports one unsupported digest runtime identifier.
    #[error("unsupported digest runtime `{value}`")]
    UnsupportedRuntime { value: String },

    /// Reports one invalid runtime request field.
    #[error("invalid runtime request: {message}")]
    InvalidRequest { message: String },

    /// Reports one schema read failure for a runtime that expects inline schema text.
    #[error("failed to read runtime schema file {path}: {source}")]
    ReadSchema {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Aliases the crate-local result type for agent runtime preparation operations.
pub type Result<T> = std::result::Result<T, AgentError>;
