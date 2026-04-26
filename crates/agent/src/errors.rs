use thiserror::Error;

/// Represents one typed runtime preparation error from the leaf agent crate.
#[derive(Debug, Error)]
pub enum AgentError {
    /// Reports one unsupported agent identifier.
    #[error("unsupported agent `{value}`")]
    UnsupportedAgent { value: String },

    /// Reports one unsupported runtime identifier.
    #[error("unsupported runtime `{value}`")]
    UnsupportedRuntime { value: String },

    /// Reports one invalid runtime request field.
    #[error("invalid runtime request: {message}")]
    InvalidRequest { message: String },
}

/// Aliases the crate-local result type for agent runtime preparation operations.
pub type Result<T> = std::result::Result<T, AgentError>;
