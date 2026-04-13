use std::{env, path::PathBuf};

use crate::{
    Result,
    runtime::{CODEX_BINARY_ENV_VAR, ProposalOutputSource, RuntimeCommand, RuntimeRequest},
};

/// Prepares one Codex CLI command for a digest proposal run.
pub fn build_codex_external_cli_command(request: &RuntimeRequest) -> Result<RuntimeCommand> {
    let program = env::var_os(CODEX_BINARY_ENV_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("codex"));
    let args = vec![
        "exec".to_owned(),
        "--model".to_owned(),
        request.model.clone(),
        "--cd".to_owned(),
        request.workdir.to_string_lossy().into_owned(),
        "--skip-git-repo-check".to_owned(),
        "--ephemeral".to_owned(),
        "--sandbox".to_owned(),
        "read-only".to_owned(),
        "--output-schema".to_owned(),
        request.schema_path.to_string_lossy().into_owned(),
        "--output-last-message".to_owned(),
        request.proposal_path.to_string_lossy().into_owned(),
    ];
    Ok(RuntimeCommand {
        program,
        args,
        workdir: request.workdir.clone(),
        stdin: request.prompt.as_bytes().to_vec(),
        proposal_output: ProposalOutputSource::File(request.proposal_path.clone()),
        display_name: "Codex CLI".to_owned(),
    })
}
