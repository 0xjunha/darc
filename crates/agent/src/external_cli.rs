use crate::{
    Result,
    claude::build_claude_external_cli_command,
    codex::build_codex_external_cli_command,
    runtime::{AgentId, RuntimeCommand, RuntimeRequest},
};

/// Prepares one external-CLI runtime command for the selected agent family.
pub fn build_external_cli_command(request: &RuntimeRequest) -> Result<RuntimeCommand> {
    match request.agent {
        AgentId::Claude => build_claude_external_cli_command(request),
        AgentId::Codex => build_codex_external_cli_command(request),
    }
}
