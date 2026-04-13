use std::{env, fs, path::PathBuf};

use crate::{
    AgentError, Result,
    runtime::{CLAUDE_BINARY_ENV_VAR, ProposalOutputSource, RuntimeCommand, RuntimeRequest},
};

/// Prepares one Claude CLI command for a digest proposal run.
pub fn build_claude_external_cli_command(request: &RuntimeRequest) -> Result<RuntimeCommand> {
    let schema =
        fs::read_to_string(&request.schema_path).map_err(|source| AgentError::ReadSchema {
            path: request.schema_path.clone(),
            source,
        })?;
    let program = env::var_os(CLAUDE_BINARY_ENV_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("claude"));
    let args = vec![
        "--print".to_owned(),
        "--model".to_owned(),
        request.model.clone(),
        "--output-format".to_owned(),
        "text".to_owned(),
        "--json-schema".to_owned(),
        schema,
        "--permission-mode".to_owned(),
        "plan".to_owned(),
        "--tools".to_owned(),
        String::new(),
        "--no-session-persistence".to_owned(),
        request.prompt.clone(),
    ];
    Ok(RuntimeCommand {
        program,
        args,
        workdir: request.workdir.clone(),
        proposal_output: ProposalOutputSource::Stdout,
        display_name: "Claude Code CLI".to_owned(),
    })
}
