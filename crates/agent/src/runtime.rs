use std::path::PathBuf;

use crate::{AgentError, Result, external_cli::build_external_cli_command};

/// Names the environment variable used to override the Codex CLI binary path.
pub const CODEX_BINARY_ENV_VAR: &str = "DARC_WIKI_CODEX_BIN";

/// Names the environment variable used to override the Claude CLI binary path.
pub const CLAUDE_BINARY_ENV_VAR: &str = "DARC_WIKI_CLAUDE_BIN";

/// Identifies the supported digest agent families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentId {
    Claude,
    Codex,
}

impl AgentId {
    /// Parses one persisted agent identifier into the typed runtime enum.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            _ => Err(AgentError::UnsupportedAgent {
                value: value.to_owned(),
            }),
        }
    }

    /// Returns the stable persisted identifier for one agent family.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// Returns the human-readable runtime label for one agent family.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code CLI",
            Self::Codex => "Codex CLI",
        }
    }
}

/// Identifies the supported digest runtime kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    ExternalCli,
}

impl RuntimeKind {
    /// Parses one persisted runtime identifier into the typed runtime enum.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "external_cli" => Ok(Self::ExternalCli),
            _ => Err(AgentError::UnsupportedRuntime {
                value: value.to_owned(),
            }),
        }
    }

    /// Returns the stable persisted identifier for one runtime kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExternalCli => "external_cli",
        }
    }
}

/// Identifies where the runtime emits its final proposal artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposalOutputSource {
    Stdout,
    StdoutJsonField(String),
    File(PathBuf),
}

impl ProposalOutputSource {
    /// Returns whether the runtime proposal artifact is derived from stdout capture.
    pub fn captures_stdout(&self) -> bool {
        matches!(self, Self::Stdout | Self::StdoutJsonField(_))
    }
}

/// Stores the digest runtime invocation inputs required across adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRequest {
    pub agent: AgentId,
    pub runtime: RuntimeKind,
    pub model: String,
    pub auth_profile: Option<String>,
    pub prompt: String,
    pub schema_json: String,
    pub darc_root: PathBuf,
    pub workdir: PathBuf,
    pub schema_path: PathBuf,
    pub proposal_path: PathBuf,
}

/// Stores the prepared external command and proposal capture strategy for one runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub workdir: PathBuf,
    pub stdin: Vec<u8>,
    pub proposal_output: ProposalOutputSource,
    pub display_name: String,
}

/// Prepares one runtime command for the selected agent family and runtime kind.
pub fn build_runtime_command(request: &RuntimeRequest) -> Result<RuntimeCommand> {
    if request.model.trim().is_empty() {
        return Err(AgentError::InvalidRequest {
            message: "model must not be empty".to_owned(),
        });
    }
    if request.prompt.trim().is_empty() {
        return Err(AgentError::InvalidRequest {
            message: "prompt must not be empty".to_owned(),
        });
    }
    if request.schema_json.trim().is_empty() {
        return Err(AgentError::InvalidRequest {
            message: "schema_json must not be empty".to_owned(),
        });
    }

    match request.runtime {
        RuntimeKind::ExternalCli => build_external_cli_command(request),
    }
}
