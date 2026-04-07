use std::{
    convert::Infallible,
    io,
    num::ParseIntError,
    path::{Path, PathBuf},
    string::FromUtf8Error,
};

use thiserror::Error;

/// Stores the typed result alias for Codex rollout operations.
pub type Result<T> = std::result::Result<T, CodexError>;
/// Stores the typed result alias for Codex parse-into operations that may also fail in a sink.
pub type ParseIntoResult<T, E> = std::result::Result<T, ParseIntoError<E>>;

/// Describes failures while parsing a persisted Codex CLI version string.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CodexCliVersionParseError {
    #[error("unsupported Codex CLI version format `{raw_version}`")]
    InvalidFormat { raw_version: String },
    #[error("unsupported Codex CLI prerelease format `{raw_version}`")]
    InvalidPrereleaseFormat { raw_version: String },
    #[error("invalid Codex CLI {label} version in `{raw_version}`: {source}")]
    InvalidNumericComponent {
        raw_version: String,
        label: &'static str,
        #[source]
        source: ParseIntError,
    },
    #[error("unsupported Codex CLI alpha prerelease `{raw_version}`: {source}")]
    InvalidAlphaPrerelease {
        raw_version: String,
        #[source]
        source: ParseIntError,
    },
}

/// Describes failures while mapping a Codex CLI version onto a supported schema family.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CodexSchemaError {
    #[error(transparent)]
    ParseVersion(#[from] CodexCliVersionParseError),
    #[error("unsupported Codex CLI version `{cli_version}`")]
    UnsupportedVersion { cli_version: String },
}

/// Describes failures while parsing one Codex rollout file.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CodexError {
    #[error("missing collected rollout header")]
    MissingCollectedHeader,
    #[error("missing session_meta line in {}", .path.display())]
    MissingSessionMetaLine { path: PathBuf },
    #[error("failed to open {}", .path.display())]
    OpenFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to read line {line_no} in {}", .path.display())]
    ReadLine {
        path: PathBuf,
        line_no: usize,
        #[source]
        source: io::Error,
    },
    #[error("failed to decode the first JSONL line in {}", .path.display())]
    DecodeFirstLine {
        path: PathBuf,
        #[source]
        source: FromUtf8Error,
    },
    #[error("failed to deserialize {context} in {}", .path.display())]
    DeserializeHeaderJson {
        path: PathBuf,
        context: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to deserialize {context} on line {line_no} in {}", .path.display())]
    DeserializeJsonLine {
        path: PathBuf,
        line_no: usize,
        context: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize {context} on line {line_no} in {}", .path.display())]
    SerializeJsonLine {
        path: PathBuf,
        line_no: usize,
        context: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "mismatched Codex session ids in {}: filename={filename_session_id} payload={payload_session_id}",
        .path.display()
    )]
    MismatchedSessionIds {
        path: PathBuf,
        filename_session_id: String,
        payload_session_id: String,
    },
    #[error("missing Codex cli_version in {}", .path.display())]
    MissingCliVersion { path: PathBuf },
    #[error(
        "unsupported Codex rollout schema for cli_version `{cli_version}` in {}",
        .path.display()
    )]
    ResolveSchema {
        path: PathBuf,
        cli_version: String,
        #[source]
        source: CodexSchemaError,
    },
    #[error("failed to parse Codex cli_version `{cli_version}` in {}", .path.display())]
    ParseCliVersion {
        path: PathBuf,
        cli_version: String,
        #[source]
        source: CodexCliVersionParseError,
    },
    #[error("encountered unsupported {feature_name} on line {line_no} in {}", .path.display())]
    UnsupportedFeature {
        path: PathBuf,
        line_no: usize,
        feature_name: &'static str,
    },
    #[error(
        "unsupported response_item `{item_kind}` on line {line_no} for cli_version `{cli_version}` in {}",
        .path.display()
    )]
    UnsupportedResponseItem {
        path: PathBuf,
        line_no: usize,
        item_kind: String,
        cli_version: String,
    },
    #[error(
        "unsupported rollout item `{item_kind}` on line {line_no} for schema {schema_id} in {}",
        .path.display()
    )]
    UnsupportedRolloutItem {
        path: PathBuf,
        line_no: usize,
        item_kind: String,
        schema_id: String,
    },
    #[error("unsupported message content shape on line {line_no} in {}", .path.display())]
    UnsupportedMessageContentShape { path: PathBuf, line_no: usize },
    #[error("unsupported {field_name} shape on line {line_no} in {}", .path.display())]
    UnsupportedFieldShape {
        path: PathBuf,
        line_no: usize,
        field_name: &'static str,
    },
    #[error("unsupported tool output shape on line {line_no} in {}", .path.display())]
    UnsupportedToolOutputShape { path: PathBuf, line_no: usize },
}

/// Describes failures while parsing a Codex rollout into one caller-provided sink.
#[derive(Debug, Error)]
pub enum ParseIntoError<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    #[error(transparent)]
    Parse(#[from] CodexError),
    #[error(transparent)]
    Sink(E),
}

impl From<ParseIntoError<Infallible>> for CodexError {
    fn from(value: ParseIntoError<Infallible>) -> Self {
        match value {
            ParseIntoError::Parse(error) => error,
            ParseIntoError::Sink(never) => match never {},
        }
    }
}

impl CodexError {
    /// Builds one missing-header error for in-memory rollout collection.
    pub(crate) fn missing_collected_header() -> Self {
        Self::MissingCollectedHeader
    }

    /// Builds one missing-session-meta error for one rollout path.
    pub(crate) fn missing_session_meta_line(path: &Path) -> Self {
        Self::MissingSessionMetaLine {
            path: path.to_path_buf(),
        }
    }

    /// Builds one file-open error for one rollout path.
    pub(crate) fn open_file(path: &Path, source: io::Error) -> Self {
        Self::OpenFile {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Builds one line-read error for one rollout path and line number.
    pub(crate) fn read_line(path: &Path, line_no: usize, source: io::Error) -> Self {
        Self::ReadLine {
            path: path.to_path_buf(),
            line_no,
            source,
        }
    }

    /// Builds one first-line UTF-8 decode error for one rollout path.
    pub(crate) fn decode_first_line(path: &Path, source: FromUtf8Error) -> Self {
        Self::DecodeFirstLine {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Builds one header-level JSON deserialization error for one rollout path.
    pub(crate) fn deserialize_header_json(
        path: &Path,
        context: &'static str,
        source: serde_json::Error,
    ) -> Self {
        Self::DeserializeHeaderJson {
            path: path.to_path_buf(),
            context,
            source,
        }
    }

    /// Builds one line-scoped JSON deserialization error for one rollout path.
    pub(crate) fn deserialize_json_line(
        path: &Path,
        line_no: usize,
        context: &'static str,
        source: serde_json::Error,
    ) -> Self {
        Self::DeserializeJsonLine {
            path: path.to_path_buf(),
            line_no,
            context,
            source,
        }
    }

    /// Builds one line-scoped JSON serialization error for one rollout path.
    pub(crate) fn serialize_json_line(
        path: &Path,
        line_no: usize,
        context: &'static str,
        source: serde_json::Error,
    ) -> Self {
        Self::SerializeJsonLine {
            path: path.to_path_buf(),
            line_no,
            context,
            source,
        }
    }

    /// Builds one mismatched-session-id error for one rollout path.
    pub(crate) fn mismatched_session_ids(
        path: &Path,
        filename_session_id: &str,
        payload_session_id: &str,
    ) -> Self {
        Self::MismatchedSessionIds {
            path: path.to_path_buf(),
            filename_session_id: filename_session_id.to_owned(),
            payload_session_id: payload_session_id.to_owned(),
        }
    }

    /// Builds one missing-cli-version error for one rollout path.
    pub(crate) fn missing_cli_version(path: &Path) -> Self {
        Self::MissingCliVersion {
            path: path.to_path_buf(),
        }
    }

    /// Builds one schema-resolution error for one rollout path.
    pub(crate) fn resolve_schema(path: &Path, cli_version: &str, source: CodexSchemaError) -> Self {
        Self::ResolveSchema {
            path: path.to_path_buf(),
            cli_version: cli_version.to_owned(),
            source,
        }
    }

    /// Builds one CLI-version parse error for one rollout path.
    pub(crate) fn parse_cli_version(
        path: &Path,
        cli_version: &str,
        source: CodexCliVersionParseError,
    ) -> Self {
        Self::ParseCliVersion {
            path: path.to_path_buf(),
            cli_version: cli_version.to_owned(),
            source,
        }
    }

    /// Builds one unsupported-feature error for one rollout path.
    pub(crate) fn unsupported_feature(
        path: &Path,
        line_no: usize,
        feature_name: &'static str,
    ) -> Self {
        Self::UnsupportedFeature {
            path: path.to_path_buf(),
            line_no,
            feature_name,
        }
    }

    /// Builds one unsupported response item error for one rollout path.
    pub(crate) fn unsupported_response_item(
        path: &Path,
        line_no: usize,
        item_kind: &str,
        cli_version: &str,
    ) -> Self {
        Self::UnsupportedResponseItem {
            path: path.to_path_buf(),
            line_no,
            item_kind: item_kind.to_owned(),
            cli_version: cli_version.to_owned(),
        }
    }

    /// Builds one unsupported rollout item error for one rollout path.
    pub(crate) fn unsupported_rollout_item(
        path: &Path,
        line_no: usize,
        item_kind: &str,
        schema_id: &str,
    ) -> Self {
        Self::UnsupportedRolloutItem {
            path: path.to_path_buf(),
            line_no,
            item_kind: item_kind.to_owned(),
            schema_id: schema_id.to_owned(),
        }
    }

    /// Builds one unsupported message-content-shape error for one rollout path.
    pub(crate) fn unsupported_message_content_shape(path: &Path, line_no: usize) -> Self {
        Self::UnsupportedMessageContentShape {
            path: path.to_path_buf(),
            line_no,
        }
    }

    /// Builds one unsupported-field-shape error for one rollout path.
    pub(crate) fn unsupported_field_shape(
        path: &Path,
        line_no: usize,
        field_name: &'static str,
    ) -> Self {
        Self::UnsupportedFieldShape {
            path: path.to_path_buf(),
            line_no,
            field_name,
        }
    }

    /// Builds one unsupported-tool-output-shape error for one rollout path.
    pub(crate) fn unsupported_tool_output_shape(path: &Path, line_no: usize) -> Self {
        Self::UnsupportedToolOutputShape {
            path: path.to_path_buf(),
            line_no,
        }
    }
}
