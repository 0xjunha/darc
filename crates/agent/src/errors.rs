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
}

/// Aliases the crate-local result type for agent runtime preparation operations.
pub type Result<T> = std::result::Result<T, AgentError>;
