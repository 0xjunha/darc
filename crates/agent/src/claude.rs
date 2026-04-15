use std::{env, path::PathBuf};

use crate::{
    Result,
    runtime::{CLAUDE_BINARY_ENV_VAR, ProposalOutputSource, RuntimeCommand, RuntimeRequest},
};

/// Prepares one Claude CLI command for a digest proposal run.
pub fn build_claude_external_cli_command(request: &RuntimeRequest) -> Result<RuntimeCommand> {
    let program = env::var_os(CLAUDE_BINARY_ENV_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("claude"));
    let args = vec![
        "--print".to_owned(),
        "--model".to_owned(),
        request.model.clone(),
        "--input-format".to_owned(),
        "text".to_owned(),
        "--output-format".to_owned(),
        "json".to_owned(),
        "--json-schema".to_owned(),
        request.schema_json.clone(),
        "--permission-mode".to_owned(),
        "plan".to_owned(),
        "--tools".to_owned(),
        String::new(),
        "--no-session-persistence".to_owned(),
    ];
    Ok(RuntimeCommand {
        program,
        args,
        workdir: request.workdir.clone(),
        stdin: request.prompt.as_bytes().to_vec(),
        proposal_output: ProposalOutputSource::StdoutJsonField("structured_output".to_owned()),
        display_name: "Claude Code CLI".to_owned(),
    })
}
