use std::{env, path::PathBuf};

use crate::{
    Result,
    runtime::{CLAUDE_BINARY_ENV_VAR, RuntimeCommand, RuntimeOutputSource, RuntimeRequest},
};

const CLAUDE_TOOLS: &str = "Bash,Read";
const CLAUDE_ALLOWED_TOOLS: &str = concat!(
    "Read,",
    "Bash(darc list:*),",
    "Bash(darc show:*),",
    "Bash(darc search:*),",
    "Bash(darc stats:*),",
    "Bash(darc resolve:*),",
    "Bash(rg:*),",
    "Bash(git log:*),",
    "Bash(git show:*),",
    "Bash(git diff:*)",
);
const CLAUDE_PROVIDER_AUTH_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// Builds one Claude argv vector for a structured runtime invocation.
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
        "--allowed-tools".to_owned(),
        CLAUDE_ALLOWED_TOOLS.to_owned(),
        "--strict-mcp-config".to_owned(),
        "--add-dir".to_owned(),
        request.darc_root.to_string_lossy().into_owned(),
        "--disable-slash-commands".to_owned(),
        "--no-session-persistence".to_owned(),
        "--no-chrome".to_owned(),
    ]);
    args
}

/// Prepares one Claude CLI command for a structured runtime invocation.
pub fn build_claude_external_cli_command(request: &RuntimeRequest) -> Result<RuntimeCommand> {
    let program = env::var_os(CLAUDE_BINARY_ENV_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("claude"));
    let args = build_claude_args(request, request.use_provider_auth);
    let env_remove = if request.use_provider_auth {
        Vec::new()
    } else {
        CLAUDE_PROVIDER_AUTH_ENV_VARS
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    };
    Ok(RuntimeCommand {
        program,
        args,
        env_remove,
        workdir: request.workdir.clone(),
        stdin: request.prompt.as_bytes().to_vec(),
        output_source: RuntimeOutputSource::StdoutJsonField("structured_output".to_owned()),
        display_name: "Claude Code CLI".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        CLAUDE_ALLOWED_TOOLS, CLAUDE_PROVIDER_AUTH_ENV_VARS, CLAUDE_TOOLS, build_claude_args,
        build_claude_external_cli_command,
    };
    use crate::runtime::{AgentId, RuntimeKind, RuntimeRequest};

    fn sample_request(use_provider_auth: bool) -> RuntimeRequest {
        RuntimeRequest {
            agent: AgentId::Claude,
            runtime: RuntimeKind::ExternalCli,
            model: "claude-sonnet-4-6".to_owned(),
            auth_profile: None,
            use_provider_auth,
            prompt: "prompt".to_owned(),
            schema_json: "{\"type\":\"object\"}".to_owned(),
            darc_root: PathBuf::from("/tmp/darc-root"),
            workdir: PathBuf::from("/tmp/project-root"),
            schema_path: PathBuf::from("/tmp/run/output.schema.json"),
            output_path: PathBuf::from("/tmp/run/output.json"),
        }
    }

    #[test]
    fn build_claude_args_without_bare_uses_tool_runtime_shape() {
        let request = sample_request(false);
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
                "--allowed-tools",
                CLAUDE_ALLOWED_TOOLS,
                "--strict-mcp-config",
                "--add-dir",
                "/tmp/darc-root",
                "--disable-slash-commands",
                "--no-session-persistence",
                "--no-chrome",
            ]
        );
    }

    #[test]
    fn build_claude_args_with_bare_in_provider_mode() {
        let request = sample_request(true);
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
                "--allowed-tools",
                CLAUDE_ALLOWED_TOOLS,
                "--strict-mcp-config",
                "--add-dir",
                "/tmp/darc-root",
                "--disable-slash-commands",
                "--no-session-persistence",
                "--no-chrome",
            ]
        );
    }

    #[test]
    fn build_claude_external_cli_command_uses_structured_output_capture() {
        let command = build_claude_external_cli_command(&sample_request(false))
            .expect("claude runtime command should build");
        assert_eq!(command.display_name, "Claude Code CLI");
        assert_eq!(
            command.output_source,
            crate::runtime::RuntimeOutputSource::StdoutJsonField("structured_output".to_owned())
        );
        assert_eq!(
            command.env_remove,
            CLAUDE_PROVIDER_AUTH_ENV_VARS
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn build_claude_external_cli_command_only_adds_bare_in_provider_mode() {
        let default_command = build_claude_external_cli_command(&sample_request(false))
            .expect("default claude runtime command should build");
        assert!(
            !default_command.args.iter().any(|arg| arg == "--bare"),
            "default Claude runs should not force bare mode"
        );

        let provider_command = build_claude_external_cli_command(&sample_request(true))
            .expect("provider-auth claude runtime command should build");
        assert!(
            provider_command.args.iter().any(|arg| arg == "--bare"),
            "provider-auth Claude runs should force bare mode"
        );
        assert!(
            provider_command.env_remove.is_empty(),
            "provider-auth Claude runs should preserve provider auth env vars"
        );
    }
}
