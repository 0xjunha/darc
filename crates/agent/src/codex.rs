use std::{env, path::PathBuf};

use crate::{
    AgentError, Result,
    runtime::{CODEX_BINARY_ENV_VAR, ProposalOutputSource, RuntimeCommand, RuntimeRequest},
};

/// Names the opt-in environment variable that re-enables the ungated Codex digest runtime.
pub const CODEX_UNSAFE_ENABLE_ENV_VAR: &str = "DARC_WIKI_UNSAFE_ENABLE_CODEX";

/// Returns whether the caller explicitly enabled the ungated Codex digest runtime.
pub fn codex_runtime_is_explicitly_enabled() -> bool {
    codex_runtime_is_explicitly_enabled_with(|name| env::var_os(name))
}

/// Returns the user-facing message for the current Codex digest runtime gate.
pub fn codex_runtime_gate_message() -> String {
    format!(
        "Codex digest runtime is disabled by default because `codex exec` does not yet expose documented MCP-isolation controls. Set `{CODEX_UNSAFE_ENABLE_ENV_VAR}=1` to opt in at your own risk."
    )
}

/// Returns whether one environment lookup explicitly enables the ungated Codex runtime.
fn codex_runtime_is_explicitly_enabled_with<F>(lookup_env: F) -> bool
where
    F: for<'a> Fn(&'a str) -> Option<std::ffi::OsString>,
{
    lookup_env(CODEX_UNSAFE_ENABLE_ENV_VAR)
        .and_then(|value| value.into_string().ok())
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

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
    build_codex_external_cli_command_with(request, codex_runtime_is_explicitly_enabled)
}

/// Prepares one Codex CLI command for a digest proposal run using the provided gate predicate.
fn build_codex_external_cli_command_with<F>(
    request: &RuntimeRequest,
    codex_enabled: F,
) -> Result<RuntimeCommand>
where
    F: FnOnce() -> bool,
{
    if !codex_enabled() {
        return Err(AgentError::InvalidRequest {
            message: codex_runtime_gate_message(),
        });
    }
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
    use std::{ffi::OsString, path::PathBuf};

    use super::{
        CODEX_UNSAFE_ENABLE_ENV_VAR, build_codex_exec_args, build_codex_external_cli_command,
        build_codex_external_cli_command_with, codex_runtime_gate_message,
        codex_runtime_is_explicitly_enabled_with,
    };
    use crate::runtime::{AgentId, RuntimeKind, RuntimeRequest};

    fn sample_request() -> RuntimeRequest {
        RuntimeRequest {
            agent: AgentId::Codex,
            runtime: RuntimeKind::ExternalCli,
            model: "gpt-5.4".to_owned(),
            auth_profile: None,
            prompt: "prompt".to_owned(),
            schema_json: "{\"type\":\"object\"}".to_owned(),
            darc_root: PathBuf::from("/tmp/darc-root"),
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
        let command = build_codex_external_cli_command_with(&sample_request(), || true)
            .expect("codex runtime command should build");
        assert_eq!(command.display_name, "Codex CLI");
        assert_eq!(
            command.proposal_output,
            crate::runtime::ProposalOutputSource::File(PathBuf::from("/tmp/run/proposal.json"))
        );
    }

    #[test]
    fn build_codex_external_cli_command_requires_explicit_gate() {
        let error = build_codex_external_cli_command(&sample_request())
            .expect_err("codex runtime command should be gated by default");
        assert!(error.to_string().contains(CODEX_UNSAFE_ENABLE_ENV_VAR));
    }

    #[test]
    fn codex_runtime_gate_is_disabled_by_default() {
        assert!(!codex_runtime_is_explicitly_enabled_with(|_| None));
    }

    #[test]
    fn codex_runtime_gate_accepts_truthy_override() {
        assert!(codex_runtime_is_explicitly_enabled_with(|name| {
            (name == CODEX_UNSAFE_ENABLE_ENV_VAR).then(|| OsString::from("true"))
        }));
    }

    #[test]
    fn codex_runtime_gate_message_names_opt_in_env_var() {
        assert!(
            codex_runtime_gate_message().contains(CODEX_UNSAFE_ENABLE_ENV_VAR),
            "gate message should name the override env var"
        );
    }
}
