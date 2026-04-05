use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use super::version::{CodexSchemaId, resolve_codex_schema};
use crate::ParseDeterminism;
use crate::path_util::normalize_project_path;

/// Stores the tolerant session-level metadata needed for rollout discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRolloutSessionMeta {
    pub session_id: String,
    pub cwd: PathBuf,
    pub cli_version: Option<String>,
}

/// Stores the parsed session-level metadata needed before schema dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRolloutHeader {
    pub session_id: String,
    pub cwd: PathBuf,
    pub cli_version: String,
    pub schema_id: CodexSchemaId,
    pub determinism: ParseDeterminism,
}

#[derive(Debug, Deserialize)]
struct RawHeaderLine {
    #[serde(rename = "type")]
    kind: String,
    payload: Value,
}

/// Stores the raw `session_meta` payload before strict schema validation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedSessionMeta {
    session_id: String,
    cwd: PathBuf,
    cli_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawSessionMetaPayload {
    id: String,
    cwd: String,
    #[serde(default)]
    cli_version: Option<String>,
}

/// Extracts the logical Codex session id from one rollout filename.
pub fn parse_rollout_file_session_id(file_name: &str) -> Option<String> {
    let trimmed = file_name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let start = trimmed.len().checked_sub(36)?;
    (start > 0 && trimmed.as_bytes().get(start - 1) == Some(&b'-'))
        .then(|| trimmed[start..].to_owned())
}

/// Reconciles one logical session id from rollout filename and payload metadata.
pub fn reconcile_rollout_session_id(
    source_path: &Path,
    file_name: Option<&str>,
    payload_session_id: Option<&str>,
) -> Result<Option<String>> {
    let filename_session_id = file_name.and_then(parse_rollout_file_session_id);

    match (filename_session_id, payload_session_id) {
        (Some(filename_session_id), Some(payload_session_id))
            if filename_session_id != payload_session_id =>
        {
            bail!(
                "mismatched Codex session ids in {}: filename={} payload={}",
                source_path.display(),
                filename_session_id,
                payload_session_id
            );
        }
        (Some(filename_session_id), _) => Ok(Some(filename_session_id)),
        (None, Some(payload_session_id)) => Ok(Some(payload_session_id.to_owned())),
        (None, None) => Ok(None),
    }
}

/// Reads the first rollout line and extracts the strict header metadata for parsing.
pub fn read_rollout_header(path: &Path) -> Result<Option<CodexRolloutHeader>> {
    let Some(line) = read_first_rollout_line(path)? else {
        return Ok(None);
    };
    parse_rollout_header_line(&line, path)
}

/// Reads the first rollout line and extracts tolerant metadata for session discovery.
pub fn read_rollout_session_meta(path: &Path) -> Result<Option<CodexRolloutSessionMeta>> {
    let Some(line) = read_first_rollout_line(path)? else {
        return Ok(None);
    };
    parse_rollout_session_meta_line(&line, path)
}

/// Reads the first non-empty rollout line bytes from one JSONL file.
pub fn read_first_rollout_line_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

/// Reads the first non-empty rollout line from one JSONL file.
fn read_first_rollout_line(path: &Path) -> Result<Option<String>> {
    let Some(line) = read_first_rollout_line_bytes(path)? else {
        return Ok(None);
    };
    String::from_utf8(line).map(Some).with_context(|| {
        format!(
            "failed to decode the first JSONL line in {}",
            path.display()
        )
    })
}

/// Parses one raw JSONL line into a Codex rollout header.
pub fn parse_rollout_header_line(
    line: &str,
    source_path: &Path,
) -> Result<Option<CodexRolloutHeader>> {
    let raw: RawHeaderLine = serde_json::from_str(line).with_context(|| {
        format!(
            "failed to deserialize the first JSONL line in {}",
            source_path.display()
        )
    })?;
    parse_rollout_header_parts(&raw.kind, raw.payload, source_path)
}

/// Parses one raw JSONL line into tolerant Codex session metadata.
pub fn parse_rollout_session_meta_line(
    line: &str,
    source_path: &Path,
) -> Result<Option<CodexRolloutSessionMeta>> {
    let raw: RawHeaderLine = serde_json::from_str(line).with_context(|| {
        format!(
            "failed to deserialize the first JSONL line in {}",
            source_path.display()
        )
    })?;
    parse_rollout_session_meta_parts(&raw.kind, raw.payload, source_path).map(|meta| {
        meta.map(|meta| CodexRolloutSessionMeta {
            session_id: meta.session_id,
            cwd: meta.cwd,
            cli_version: meta.cli_version,
        })
    })
}

/// Parses one already-deserialized first rollout line into a Codex rollout header.
pub(crate) fn parse_rollout_header_parts(
    kind: &str,
    payload: Value,
    source_path: &Path,
) -> Result<Option<CodexRolloutHeader>> {
    let Some(meta) = parse_rollout_session_meta_parts(kind, payload, source_path)? else {
        return Ok(None);
    };
    let Some(cli_version) = meta.cli_version else {
        bail!("missing Codex cli_version in {}", source_path.display());
    };
    let resolution = resolve_codex_schema(&cli_version).with_context(|| {
        format!(
            "unsupported Codex rollout schema for cli_version `{}` in {}",
            cli_version,
            source_path.display()
        )
    })?;

    Ok(Some(CodexRolloutHeader {
        session_id: meta.session_id,
        cwd: meta.cwd,
        cli_version,
        schema_id: resolution.schema_id,
        determinism: resolution.determinism,
    }))
}

/// Parses one already-deserialized first rollout line into tolerant session metadata.
fn parse_rollout_session_meta_parts(
    kind: &str,
    payload: Value,
    source_path: &Path,
) -> Result<Option<ParsedSessionMeta>> {
    if kind != "session_meta" {
        return Ok(None);
    }

    let payload: RawSessionMetaPayload = serde_json::from_value(payload).with_context(|| {
        format!(
            "failed to deserialize session_meta payload in {}",
            source_path.display()
        )
    })?;

    Ok(Some(ParsedSessionMeta {
        session_id: payload.id,
        cwd: normalize_project_path(Path::new(&payload.cwd)),
        cli_version: payload.cli_version.and_then(non_empty_text),
    }))
}

/// Trims optional header text and drops empty values.
fn non_empty_text(text: String) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        parse_rollout_file_session_id, parse_rollout_header_line, parse_rollout_session_meta_line,
        reconcile_rollout_session_id,
    };
    use crate::{ParseDeterminism, codex::version::CodexSchemaId};

    #[test]
    fn parses_rollout_header_and_resolves_schema() {
        let header = parse_rollout_header_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/repo","cli_version":"0.118.0"}}"#,
            Path::new("fixture.jsonl"),
        )
        .unwrap()
        .unwrap();

        assert_eq!(header.session_id, "session-1");
        assert_eq!(header.cli_version, "0.118.0");
        assert_eq!(header.schema_id, CodexSchemaId::TurnLifecycle);
        assert_eq!(header.determinism, ParseDeterminism::Exact);
    }

    #[test]
    fn parses_tolerant_session_meta_without_cli_version() {
        let meta = parse_rollout_session_meta_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/repo"}}"#,
            Path::new("fixture.jsonl"),
        )
        .unwrap()
        .unwrap();

        assert_eq!(meta.session_id, "session-1");
        assert_eq!(meta.cwd, PathBuf::from("/tmp/repo"));
        assert_eq!(meta.cli_version, None);
    }

    #[test]
    fn strict_header_parse_requires_cli_version() {
        let error = parse_rollout_header_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"session-1","cwd":"/tmp/repo"}}"#,
            Path::new("fixture.jsonl"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("missing Codex cli_version"));
    }

    #[test]
    fn parses_rollout_file_session_ids() {
        assert_eq!(
            parse_rollout_file_session_id(
                "rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"
            ),
            Some("019d3415-0b9c-7dc3-88e0-e9cb7a789e3f".to_owned())
        );
        assert_eq!(parse_rollout_file_session_id("rollout-invalid.jsonl"), None);
    }

    #[test]
    fn reconciles_rollout_session_ids_from_payload_only_filename() {
        let session_id = reconcile_rollout_session_id(
            Path::new("rollout-invalid.jsonl"),
            Some("rollout-invalid.jsonl"),
            Some("session-1"),
        )
        .unwrap();

        assert_eq!(session_id.as_deref(), Some("session-1"));
    }

    #[test]
    fn rejects_mismatched_rollout_session_ids() {
        let error = reconcile_rollout_session_id(
            Path::new("rollout.jsonl"),
            Some("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
            Some("019d3415-0b9c-7dc3-88e0-e9cb7a789e40"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("mismatched Codex session ids"));
    }
}
