use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use super::version::{CodexSchemaId, resolve_codex_schema};
use crate::project_paths::normalize_project_path;
use crate::rollout::ParseDeterminism;

/// Stores the parsed session-level metadata needed before schema dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexRolloutHeader {
    pub(crate) session_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) cli_version: String,
    pub(crate) schema_id: CodexSchemaId,
    pub(crate) determinism: ParseDeterminism,
}

#[derive(Debug, Deserialize)]
struct RawHeaderLine {
    #[serde(rename = "type")]
    kind: String,
    payload: Value,
}

#[derive(Debug, Deserialize)]
struct RawSessionMetaPayload {
    id: String,
    cwd: String,
    #[serde(default)]
    cli_version: String,
}

/// Reads the first rollout line and extracts the shared Codex header metadata.
pub(crate) fn read_rollout_header(path: &Path) -> Result<Option<CodexRolloutHeader>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    parse_rollout_header_line(&line, path)
}

/// Parses one raw JSONL line into a Codex rollout header.
pub(crate) fn parse_rollout_header_line(
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

/// Parses one already-deserialized first rollout line into a Codex rollout header.
pub(crate) fn parse_rollout_header_parts(
    kind: &str,
    payload: Value,
    source_path: &Path,
) -> Result<Option<CodexRolloutHeader>> {
    if kind != "session_meta" {
        return Ok(None);
    }

    let payload: RawSessionMetaPayload = serde_json::from_value(payload).with_context(|| {
        format!(
            "failed to deserialize session_meta payload in {}",
            source_path.display()
        )
    })?;
    if payload.cli_version.trim().is_empty() {
        bail!("missing Codex cli_version in {}", source_path.display());
    }
    let resolution = resolve_codex_schema(&payload.cli_version).with_context(|| {
        format!(
            "unsupported Codex rollout schema for cli_version `{}` in {}",
            payload.cli_version,
            source_path.display()
        )
    })?;

    Ok(Some(CodexRolloutHeader {
        session_id: payload.id,
        cwd: normalize_project_path(Path::new(&payload.cwd)),
        cli_version: payload.cli_version,
        schema_id: resolution.schema_id,
        determinism: resolution.determinism,
    }))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::parse_rollout_header_line;
    use crate::rollout::ParseDeterminism;
    use crate::rollout::codex::version::CodexSchemaId;

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
}
