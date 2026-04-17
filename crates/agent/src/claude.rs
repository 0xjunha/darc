use std::{env, ffi::OsString, path::PathBuf};

use crate::{
    Result,
    runtime::{CLAUDE_BINARY_ENV_VAR, ProposalOutputSource, RuntimeCommand, RuntimeRequest},
};

const CLAUDE_TOOLS: &str = "";

/// Builds one Claude argv vector for a digest proposal run.
fn build_claude_args(request: &RuntimeRequest, include_bare: bool) -> Vec<String> {
    let mut args = vec!["--print".to_owned()];
    if include_bare {
        args.push("--bare".to_owned());
    }
    args.extend([
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
        CLAUDE_TOOLS.to_owned(),
        "--strict-mcp-config".to_owned(),
        "--disable-slash-commands".to_owned(),
        "--no-session-persistence".to_owned(),
        "--no-chrome".to_owned(),
    ]);
    args
}

/// Returns whether the current environment supports Claude bare mode without OAuth fallback.
fn supports_claude_bare_mode() -> bool {
    supports_claude_bare_mode_with(|name| env::var_os(name))
}

/// Returns whether one environment lookup exposes bare-mode-compatible Claude auth.
fn supports_claude_bare_mode_with<F>(lookup_env: F) -> bool
where
    F: for<'a> Fn(&'a str) -> Option<OsString>,
{
    [
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ]
    .into_iter()
    .any(|name| lookup_env(name).is_some())
}

/// Prepares one Claude CLI command for a digest proposal run.
pub fn build_claude_external_cli_command(request: &RuntimeRequest) -> Result<RuntimeCommand> {
    let program = env::var_os(CLAUDE_BINARY_ENV_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("claude"));
    let args = build_claude_args(request, supports_claude_bare_mode());
    Ok(RuntimeCommand {
        program,
        args,
        workdir: request.workdir.clone(),
        stdin: request.prompt.as_bytes().to_vec(),
        proposal_output: ProposalOutputSource::StdoutJsonField("structured_output".to_owned()),
        display_name: "Claude Code CLI".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

    use super::{
        CLAUDE_TOOLS, build_claude_args, build_claude_external_cli_command,
        supports_claude_bare_mode_with,
    };
    use crate::runtime::{AgentId, RuntimeKind, RuntimeRequest};

    fn sample_request() -> RuntimeRequest {
        RuntimeRequest {
            agent: AgentId::Claude,
            runtime: RuntimeKind::ExternalCli,
            model: "claude-sonnet-4-6".to_owned(),
            auth_profile: None,
            prompt: "prompt".to_owned(),
            schema_json: "{\"type\":\"object\"}".to_owned(),
            workdir: PathBuf::from("/tmp/project-root"),
            schema_path: PathBuf::from("/tmp/run/proposal.schema.json"),
            proposal_path: PathBuf::from("/tmp/run/proposal.json"),
        }
    }

    #[test]
    fn build_claude_args_without_bare_keeps_bundle_safe_shape() {
        let request = sample_request();
        assert_eq!(
            build_claude_args(&request, false),
            vec![
                "--print",
                "--model",
                "claude-sonnet-4-6",
                "--input-format",
                "text",
                "--output-format",
                "json",
                "--json-schema",
                "{\"type\":\"object\"}",
                "--permission-mode",
                "plan",
                "--tools",
                CLAUDE_TOOLS,
                "--strict-mcp-config",
                "--disable-slash-commands",
                "--no-session-persistence",
                "--no-chrome",
            ]
        );
    }

    #[test]
    fn build_claude_args_with_bare_when_supported() {
        let request = sample_request();
        assert_eq!(
            build_claude_args(&request, true),
            vec![
                "--print",
                "--bare",
                "--model",
                "claude-sonnet-4-6",
                "--input-format",
                "text",
                "--output-format",
                "json",
                "--json-schema",
                "{\"type\":\"object\"}",
                "--permission-mode",
                "plan",
                "--tools",
                CLAUDE_TOOLS,
                "--strict-mcp-config",
                "--disable-slash-commands",
                "--no-session-persistence",
                "--no-chrome",
            ]
        );
    }

    #[test]
    fn supports_claude_bare_mode_with_api_key() {
        assert!(supports_claude_bare_mode_with(|name| {
            (name == "ANTHROPIC_API_KEY").then(|| OsString::from("test-key"))
        }));
    }

    #[test]
    fn supports_claude_bare_mode_without_supported_auth_is_false() {
        assert!(!supports_claude_bare_mode_with(|_| None));
    }

    #[test]
    fn build_claude_external_cli_command_uses_structured_output_capture() {
        let command = build_claude_external_cli_command(&sample_request())
            .expect("claude runtime command should build");
        assert_eq!(command.display_name, "Claude Code CLI");
        assert_eq!(
            command.proposal_output,
            crate::runtime::ProposalOutputSource::StdoutJsonField("structured_output".to_owned())
        );
    }
}
