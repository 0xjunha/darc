use std::{
    io,
    num::ParseIntError,
    path::{Path, PathBuf},
};

use thiserror::Error;

/// Stores the typed result alias for Claude rollout operations.
pub type Result<T> = std::result::Result<T, ClaudeError>;

/// Describes failures while parsing a persisted Claude CLI version string.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClaudeCliVersionParseError {
    #[error("unsupported Claude CLI version format `{raw_version}`")]
    InvalidFormat { raw_version: String },
    #[error("invalid Claude CLI {label} version in `{raw_version}`: {source}")]
    InvalidNumericComponent {
        raw_version: String,
        label: &'static str,
        #[source]
        source: ParseIntError,
    },
}

/// Describes failures while parsing one archived Claude rollout file.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClaudeError {
    #[error("failed to open {}", .path.display())]
    OpenFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to read Claude rollout line {line_no} from {}", .path.display())]
    ReadLine {
        path: PathBuf,
        line_no: usize,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse Claude JSONL line {line_no} in {}", .path.display())]
    ParseJsonLine {
        path: PathBuf,
        line_no: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "Claude rollout line {line_no} in {} is not a JSON object",
        .path.display()
    )]
    JsonLineNotObject { path: PathBuf, line_no: usize },
    #[error("missing Claude cwd metadata in {}", .path.display())]
    MissingCwdMetadata { path: PathBuf },
    #[error(
        "mismatched Claude session ids in {}: archive expects `{expected_session_id}`, rollout reported `{reported_session_id}`",
        .path.display()
    )]
    MismatchedSessionId {
        path: PathBuf,
        expected_session_id: String,
        reported_session_id: String,
    },
    #[error(
        "mismatched Claude agent ids in {}: archive expects `{expected_agent_id}`, rollout reported `{reported_agent_id}`",
        .path.display()
    )]
    MismatchedAgentId {
        path: PathBuf,
        expected_agent_id: String,
        reported_agent_id: String,
    },
    #[error("missing Claude user message object in {}", .path.display())]
    MissingUserMessageObject { path: PathBuf },
    #[error("missing Claude assistant message object in {}", .path.display())]
    MissingAssistantMessageObject { path: PathBuf },
    #[error("failed to serialize {context}")]
    SerializeJson {
        context: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

impl ClaudeError {
    /// Builds one file-open error for one Claude rollout path.
    pub(crate) fn open_file(path: &Path, source: io::Error) -> Self {
        Self::OpenFile {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Builds one line-read error for one Claude rollout path.
    pub(crate) fn read_line(path: &Path, line_no: usize, source: io::Error) -> Self {
        Self::ReadLine {
            path: path.to_path_buf(),
            line_no,
            source,
        }
    }

    /// Builds one JSONL parse error for one Claude rollout path.
    pub(crate) fn parse_json_line(path: &Path, line_no: usize, source: serde_json::Error) -> Self {
        Self::ParseJsonLine {
            path: path.to_path_buf(),
            line_no,
            source,
        }
    }

    /// Builds one non-object JSONL error for one Claude rollout path.
    pub(crate) fn json_line_not_object(path: &Path, line_no: usize) -> Self {
        Self::JsonLineNotObject {
            path: path.to_path_buf(),
            line_no,
        }
    }

    /// Builds one missing-cwd error for one Claude rollout path.
    pub(crate) fn missing_cwd_metadata(path: &Path) -> Self {
        Self::MissingCwdMetadata {
            path: path.to_path_buf(),
        }
    }

    /// Builds one mismatched-session-id error for one Claude rollout path.
    pub(crate) fn mismatched_session_id(
        path: &Path,
        expected_session_id: &str,
        reported_session_id: &str,
    ) -> Self {
        Self::MismatchedSessionId {
            path: path.to_path_buf(),
            expected_session_id: expected_session_id.to_owned(),
            reported_session_id: reported_session_id.to_owned(),
        }
    }

    /// Builds one mismatched-agent-id error for one Claude rollout path.
    pub(crate) fn mismatched_agent_id(
        path: &Path,
        expected_agent_id: &str,
        reported_agent_id: &str,
    ) -> Self {
        Self::MismatchedAgentId {
            path: path.to_path_buf(),
            expected_agent_id: expected_agent_id.to_owned(),
            reported_agent_id: reported_agent_id.to_owned(),
        }
    }

    /// Builds one missing-user-message error for one Claude rollout path.
    pub(crate) fn missing_user_message_object(path: &Path) -> Self {
        Self::MissingUserMessageObject {
            path: path.to_path_buf(),
        }
    }

    /// Builds one missing-assistant-message error for one Claude rollout path.
    pub(crate) fn missing_assistant_message_object(path: &Path) -> Self {
        Self::MissingAssistantMessageObject {
            path: path.to_path_buf(),
        }
    }

    /// Builds one JSON serialization error while normalizing one Claude payload.
    pub(crate) fn serialize_json(context: &'static str, source: serde_json::Error) -> Self {
        Self::SerializeJson { context, source }
    }
}
