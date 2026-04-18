mod claude;
mod codex;
mod errors;
mod external_cli;
mod runtime;

pub use codex::codex_provider_auth_unsupported_message;
pub use errors::{AgentError, Result};
pub use runtime::{
    AgentId, ProposalOutputSource, RuntimeCommand, RuntimeKind, RuntimeRequest,
    build_runtime_command,
};
