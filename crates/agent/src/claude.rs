use std::{env, ffi::OsString, path::PathBuf};

use crate::{
    Result,
    runtime::{CLAUDE_BINARY_ENV_VAR, ProposalOutputSource, RuntimeCommand, RuntimeRequest},
};

/// Prepares one Claude CLI command for a digest proposal run.
pub fn build_claude_external_cli_command(request: &RuntimeRequest) -> Result<RuntimeCommand> {
    let program = env::var_os(CLAUDE_BINARY_ENV_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("claude"));
    let mut args = vec!["--print".to_owned()];
    if supports_claude_bare_mode() {
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
        String::new(),
        "--disable-slash-commands".to_owned(),
        "--no-session-persistence".to_owned(),
    ]);
    Ok(RuntimeCommand {
        program,
        args,
        workdir: request.workdir.clone(),
        stdin: request.prompt.as_bytes().to_vec(),
        proposal_output: ProposalOutputSource::StdoutJsonField("structured_output".to_owned()),
        display_name: "Claude Code CLI".to_owned(),
    })
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::supports_claude_bare_mode_with;

    #[test]
    fn supports_claude_bare_mode_with_api_key() {
        assert!(supports_claude_bare_mode_with(|name| {
            (name == "ANTHROPIC_API_KEY").then(|| OsString::from("test-key"))
        }));
    }

    #[test]
    fn supports_claude_bare_mode_with_provider_toggle() {
        assert!(supports_claude_bare_mode_with(|name| {
            (name == "CLAUDE_CODE_USE_VERTEX").then(|| OsString::from("1"))
        }));
    }

    #[test]
    fn supports_claude_bare_mode_without_supported_auth_is_false() {
        assert!(!supports_claude_bare_mode_with(|_| None));
    }
}
