use std::{env, path::PathBuf};

use crate::{
    Result,
    runtime::{CODEX_BINARY_ENV_VAR, ProposalOutputSource, RuntimeCommand, RuntimeRequest},
};

/// Builds one Codex exec argv vector for a digest proposal run.
fn build_codex_exec_args(request: &RuntimeRequest) -> Vec<String> {
    vec![
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
    ]
}

/// Prepares one Codex CLI command for a digest proposal run.
pub fn build_codex_external_cli_command(request: &RuntimeRequest) -> Result<RuntimeCommand> {
    let program = env::var_os(CODEX_BINARY_ENV_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("codex"));
    let args = build_codex_exec_args(request);
    Ok(RuntimeCommand {
        program,
        args,
        workdir: request.workdir.clone(),
        stdin: request.prompt.as_bytes().to_vec(),
        proposal_output: ProposalOutputSource::File(request.proposal_path.clone()),
        display_name: "Codex CLI".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{build_codex_exec_args, build_codex_external_cli_command};
    use crate::runtime::{AgentId, RuntimeKind, RuntimeRequest};

    fn sample_request() -> RuntimeRequest {
        RuntimeRequest {
            agent: AgentId::Codex,
            runtime: RuntimeKind::ExternalCli,
            model: "gpt-5.4".to_owned(),
            auth_profile: None,
            prompt: "prompt".to_owned(),
            schema_json: "{\"type\":\"object\"}".to_owned(),
            workdir: PathBuf::from("/tmp/project-root"),
            schema_path: PathBuf::from("/tmp/run/proposal.schema.json"),
            proposal_path: PathBuf::from("/tmp/run/proposal.json"),
        }
    }

    #[test]
    fn build_codex_exec_args_matches_pr2_shape() {
        let request = sample_request();
        assert_eq!(
            build_codex_exec_args(&request),
            vec![
                "exec",
                "--model",
                "gpt-5.4",
                "--cd",
                "/tmp/project-root",
                "--skip-git-repo-check",
                "--ephemeral",
                "--sandbox",
                "read-only",
                "--output-schema",
                "/tmp/run/proposal.schema.json",
                "--output-last-message",
                "/tmp/run/proposal.json",
            ]
        );
    }

    #[test]
    fn build_codex_external_cli_command_uses_file_output_capture() {
        let command = build_codex_external_cli_command(&sample_request())
            .expect("codex runtime command should build");
        assert_eq!(command.display_name, "Codex CLI");
        assert_eq!(
            command.proposal_output,
            crate::runtime::ProposalOutputSource::File(PathBuf::from("/tmp/run/proposal.json"))
        );
    }
}
