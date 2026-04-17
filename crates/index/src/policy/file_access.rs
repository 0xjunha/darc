use std::{collections::BTreeSet, path::Path};

use darc_paths::SourceKind;
use darc_rollout::model::NormalizedTurnStep;
use serde_json::Value;

use super::shell::{derive_shell_file_accesses, is_shell_tool_name};

/// Stores one normalized tool-call record derived from one turn's steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallRecord {
    pub project_id: String,
    pub provider: SourceKind,
    pub session_id: String,
    pub turn_ordinal: u64,
    pub call_ordinal: u64,
    pub call_id: String,
    pub timestamp: String,
    pub tool_name: Option<String>,
    pub arguments_text: Option<String>,
    pub output_text: Option<String>,
    pub status: Option<String>,
    pub is_error: bool,
}

/// Stores one aggregated patch-derived code-change summary for one turn or tool call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodeChangeSummary {
    pub changed_file_count: u32,
    pub added_line_count: u32,
    pub removed_line_count: u32,
}

impl CodeChangeSummary {
    /// Adds another code-change summary using saturating arithmetic.
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            changed_file_count: self
                .changed_file_count
                .saturating_add(other.changed_file_count),
            added_line_count: self.added_line_count.saturating_add(other.added_line_count),
            removed_line_count: self
                .removed_line_count
                .saturating_add(other.removed_line_count),
        }
    }
}

/// Stores one normalized file-access record derived from one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAccessRecord {
    pub project_id: String,
    pub provider: SourceKind,
    pub session_id: String,
    pub turn_ordinal: u64,
    pub call_ordinal: u64,
    pub call_id: String,
    pub timestamp: String,
    pub tool_name: String,
    pub access_type: ToolAccessKind,
    pub path: String,
    pub repo_relative_path: Option<String>,
    pub file_name: Option<String>,
}

/// Stores the coarse file-access bucket inferred from one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolAccessKind {
    Read,
    Write,
    Edit,
    List,
    Other,
}

/// Stores one patch-level file change parsed from one apply-patch payload.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ApplyPatchChange {
    access_type: ToolAccessKind,
    path: String,
    added_line_count: u32,
    removed_line_count: u32,
}

impl ToolAccessKind {
    /// Returns the stable SQLite string value for one access kind.
    pub const fn as_sql_text(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::List => "list",
            Self::Other => "other",
        }
    }
}

/// Extracts normalized tool-call records from one turn's normalized steps.
pub fn extract_tool_call_records(
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    turn_ordinal: u64,
    steps: &[NormalizedTurnStep],
) -> Vec<ToolCallRecord> {
    let mut records = Vec::<ToolCallRecord>::new();

    for step in steps {
        match step {
            NormalizedTurnStep::ToolCall {
                timestamp,
                call_id,
                name,
                arguments,
            } => {
                if let Some(record) = records
                    .iter_mut()
                    .rev()
                    .find(|record| record.call_id == call_id.as_str() && record.tool_name.is_none())
                {
                    record.timestamp = timestamp.clone();
                    record.tool_name = Some(name.clone());
                    record.arguments_text = Some(arguments.clone());
                    continue;
                }

                records.push(ToolCallRecord {
                    project_id: project_id.to_owned(),
                    provider,
                    session_id: session_id.to_owned(),
                    turn_ordinal,
                    call_ordinal: u64::try_from(records.len()).unwrap_or(u64::MAX),
                    call_id: call_id.clone(),
                    timestamp: timestamp.clone(),
                    tool_name: Some(name.clone()),
                    arguments_text: Some(arguments.clone()),
                    output_text: None,
                    status: None,
                    is_error: false,
                });
            }
            NormalizedTurnStep::ToolCallOutput {
                timestamp,
                call_id,
                output,
            } => {
                let (status, is_error) = infer_tool_call_outcome(output);
                if let Some(record) = records.iter_mut().rev().find(|record| {
                    record.call_id == call_id.as_str() && record.output_text.is_none()
                }) {
                    record.output_text = Some(output.clone());
                    record.status = status;
                    record.is_error = is_error;
                    continue;
                }

                records.push(ToolCallRecord {
                    project_id: project_id.to_owned(),
                    provider,
                    session_id: session_id.to_owned(),
                    turn_ordinal,
                    call_ordinal: u64::try_from(records.len()).unwrap_or(u64::MAX),
                    call_id: call_id.clone(),
                    timestamp: timestamp.clone(),
                    tool_name: None,
                    arguments_text: None,
                    output_text: Some(output.clone()),
                    status,
                    is_error,
                });
            }
            _ => {}
        }
    }

    records
}

/// Derives normalized file-access records from normalized tool-call records.
pub fn derive_file_access_records(tool_calls: &[ToolCallRecord]) -> Vec<FileAccessRecord> {
    let mut records = Vec::new();

    for tool_call in tool_calls {
        let Some(tool_name) = &tool_call.tool_name else {
            continue;
        };
        let Some(arguments_text) = &tool_call.arguments_text else {
            continue;
        };

        let accesses = if is_shell_tool_name(tool_name) {
            derive_shell_file_accesses(arguments_text)
        } else if tool_name == "apply_patch" {
            derive_apply_patch_file_accesses(arguments_text)
        } else {
            derive_explicit_tool_file_accesses(tool_name, arguments_text)
        };

        records.extend(build_file_access_records(tool_call, tool_name, &accesses));
    }

    records
}

/// Extracts candidate file paths from one tool-call arguments payload.
pub fn extract_tool_paths(arguments: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return Vec::new();
    };
    let Some(object) = value.as_object() else {
        return Vec::new();
    };

    let mut paths = BTreeSet::new();
    for key in ["file_path", "path", "file"] {
        if let Some(value) = object.get(key) {
            collect_tool_paths(value, &mut paths);
        }
    }
    paths.into_iter().collect()
}

/// Extracts one candidate file path from one tool-call arguments payload.
pub fn extract_tool_path(arguments: &str) -> Option<String> {
    extract_tool_paths(arguments).into_iter().next()
}

/// Classifies one tool name into a provisional coarse file-access bucket.
pub fn classify_tool_access(name: &str) -> ToolAccessKind {
    let normalized = name.to_ascii_lowercase();
    if normalized.contains("glob") || normalized.contains("list") {
        ToolAccessKind::List
    } else if normalized.contains("edit")
        || normalized.contains("replace")
        || normalized.contains("patch")
    {
        ToolAccessKind::Edit
    } else if normalized.contains("write") {
        ToolAccessKind::Write
    } else if normalized.contains("read")
        || normalized.contains("view")
        || normalized.contains("grep")
    {
        ToolAccessKind::Read
    } else {
        ToolAccessKind::Other
    }
}

/// Infers tool-call status and error state from one serialized tool output.
fn infer_tool_call_outcome(output: &str) -> (Option<String>, bool) {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return (None, false);
    };

    let mut status = value
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let has_error_field = value.get("error").is_some_and(|value| !value.is_null());
    let is_error_flag = value
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let exit_code = value.pointer("/metadata/exit_code").and_then(Value::as_i64);
    let status_is_error = status.as_deref().is_some_and(is_error_status);
    let is_error = is_error_flag
        || has_error_field
        || status_is_error
        || exit_code.is_some_and(|code| code != 0);

    if status.is_none() && is_error {
        status = Some("error".to_owned());
    }

    (status, is_error)
}

/// Derives file accesses from tools that already expose file-like paths directly.
fn derive_explicit_tool_file_accesses(
    tool_name: &str,
    arguments_text: &str,
) -> Vec<(ToolAccessKind, String)> {
    let access_type = classify_tool_access(tool_name);
    if access_type == ToolAccessKind::Other {
        return Vec::new();
    }

    extract_tool_paths(arguments_text)
        .into_iter()
        .map(|path| (access_type, path))
        .collect()
}

/// Derives file accesses from one apply-patch payload.
pub(super) fn derive_apply_patch_file_accesses(text: &str) -> Vec<(ToolAccessKind, String)> {
    parse_apply_patch_changes(text)
        .into_iter()
        .map(|change| (change.access_type, change.path))
        .collect()
}

/// Summarizes one apply-patch payload into stable file-count and line-count statistics.
pub fn summarize_apply_patch_changes(text: &str) -> CodeChangeSummary {
    let mut changed_paths = BTreeSet::new();
    let mut summary = CodeChangeSummary::default();
    for change in parse_apply_patch_changes(text) {
        changed_paths.insert(change.path);
        summary.added_line_count = summary
            .added_line_count
            .saturating_add(change.added_line_count);
        summary.removed_line_count = summary
            .removed_line_count
            .saturating_add(change.removed_line_count);
    }
    summary.changed_file_count = changed_paths.len().try_into().unwrap_or(u32::MAX);
    summary
}

/// Returns the distinct changed file paths observed in one apply-patch payload.
pub fn apply_patch_changed_paths(text: &str) -> Vec<String> {
    parse_apply_patch_changes(text)
        .into_iter()
        .map(|change| change.path)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Returns whether one extracted access path still looks like a file path worth indexing.
fn should_index_access_path(access_type: ToolAccessKind, path: &str) -> bool {
    access_type != ToolAccessKind::List || !path_looks_directory_like(path)
}

/// Returns whether one extracted path looks like a directory root instead of a file path.
pub(super) fn path_looks_directory_like(path: &str) -> bool {
    let path = path
        .trim()
        .trim_matches(['"', '\''])
        .trim()
        .trim_end_matches('/');
    if path.is_empty() {
        return false;
    }

    let path = Path::new(path);
    if path.extension().is_some() {
        return false;
    }

    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if matches!(file_name, "." | "..") {
        return true;
    }
    if file_name.starts_with('.') {
        return true;
    }

    file_name
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '_' | '-'))
}

/// Builds concrete file-access rows from raw `(kind, path)` pairs.
fn build_file_access_records(
    tool_call: &ToolCallRecord,
    tool_name: &str,
    accesses: &[(ToolAccessKind, String)],
) -> Vec<FileAccessRecord> {
    let unique = accesses
        .iter()
        .filter_map(|(access_type, path)| {
            sanitize_access_path(path)
                .filter(|path| should_index_access_path(*access_type, path))
                .map(|path| (*access_type, path))
        })
        .collect::<BTreeSet<_>>();

    unique
        .into_iter()
        .map(|(access_type, path)| FileAccessRecord {
            project_id: tool_call.project_id.clone(),
            provider: tool_call.provider,
            session_id: tool_call.session_id.clone(),
            turn_ordinal: tool_call.turn_ordinal,
            call_ordinal: tool_call.call_ordinal,
            call_id: tool_call.call_id.clone(),
            timestamp: tool_call.timestamp.clone(),
            tool_name: tool_name.to_owned(),
            repo_relative_path: repo_relative_path(&path),
            file_name: path_file_name(&path),
            access_type,
            path,
        })
        .collect()
}

/// Appends one sanitized access path to the accumulated pair list.
pub(super) fn push_access(
    accesses: &mut Vec<(ToolAccessKind, String)>,
    access_type: ToolAccessKind,
    path: &str,
) {
    if let Some(path) = sanitize_access_path(path) {
        accesses.push((access_type, path));
    }
}

/// Parses one apply-patch payload into per-file access and line-change records.
fn parse_apply_patch_changes(text: &str) -> Vec<ApplyPatchChange> {
    let patch = text
        .find("*** Begin Patch")
        .map(|index| &text[index..])
        .unwrap_or(text);
    let mut changes = Vec::new();
    let mut current_path = None::<String>;
    let mut current_access_type = ToolAccessKind::Edit;
    let mut current_added_line_count = 0_u32;
    let mut current_removed_line_count = 0_u32;

    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            push_apply_patch_change(
                &mut changes,
                &mut current_path,
                &mut current_added_line_count,
                &mut current_removed_line_count,
                current_access_type,
            );
            if let Some(path) = sanitize_access_path(path) {
                current_access_type = ToolAccessKind::Write;
                current_path = Some(path);
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            push_apply_patch_change(
                &mut changes,
                &mut current_path,
                &mut current_added_line_count,
                &mut current_removed_line_count,
                current_access_type,
            );
            if let Some(path) = sanitize_access_path(path) {
                current_access_type = ToolAccessKind::Edit;
                current_path = Some(path);
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            push_apply_patch_change(
                &mut changes,
                &mut current_path,
                &mut current_added_line_count,
                &mut current_removed_line_count,
                current_access_type,
            );
            if let Some(path) = sanitize_access_path(path) {
                current_access_type = ToolAccessKind::Edit;
                current_path = Some(path);
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Move to: ") {
            push_apply_patch_change(
                &mut changes,
                &mut current_path,
                &mut current_added_line_count,
                &mut current_removed_line_count,
                current_access_type,
            );
            if let Some(path) = sanitize_access_path(path) {
                current_access_type = ToolAccessKind::Write;
                current_path = Some(path);
            }
            continue;
        }
        if line.starts_with("*** End") {
            push_apply_patch_change(
                &mut changes,
                &mut current_path,
                &mut current_added_line_count,
                &mut current_removed_line_count,
                current_access_type,
            );
            continue;
        }
        if current_path.is_none() {
            continue;
        }
        if line.starts_with('+') {
            current_added_line_count = current_added_line_count.saturating_add(1);
        } else if line.starts_with('-') {
            current_removed_line_count = current_removed_line_count.saturating_add(1);
        }
    }

    push_apply_patch_change(
        &mut changes,
        &mut current_path,
        &mut current_added_line_count,
        &mut current_removed_line_count,
        current_access_type,
    );
    changes
}

/// Pushes one in-progress apply-patch file change and resets the counters.
fn push_apply_patch_change(
    changes: &mut Vec<ApplyPatchChange>,
    current_path: &mut Option<String>,
    current_added_line_count: &mut u32,
    current_removed_line_count: &mut u32,
    current_access_type: ToolAccessKind,
) {
    let Some(path) = current_path.take() else {
        *current_added_line_count = 0;
        *current_removed_line_count = 0;
        return;
    };
    changes.push(ApplyPatchChange {
        access_type: current_access_type,
        path,
        added_line_count: *current_added_line_count,
        removed_line_count: *current_removed_line_count,
    });
    *current_added_line_count = 0;
    *current_removed_line_count = 0;
}

/// Sanitizes one candidate access path extracted from shell syntax or JSON arguments.
fn sanitize_access_path(path: &str) -> Option<String> {
    let path = path.trim().trim_matches(['"', '\'']).trim();
    if path.is_empty()
        || matches!(
            path,
            "." | ".." | "-" | "EOF" | "PATCH" | "[" | "]" | "{" | "}" | "(" | ")"
        )
        || path == "/dev/null"
        || path.contains("$(")
        || path.contains("${")
        || path.contains('*')
        || path.contains('?')
    {
        return None;
    }
    if path.starts_with('$') && !path.contains('/') {
        return None;
    }
    Some(path.to_owned())
}

/// Appends any string-like path values from one JSON value into the set.
fn collect_tool_paths(value: &Value, paths: &mut BTreeSet<String>) {
    match value {
        Value::String(path) => insert_tool_path(path, paths),
        Value::Array(values) => {
            for value in values {
                if let Value::String(path) = value {
                    insert_tool_path(path, paths);
                }
            }
        }
        _ => {}
    }
}

/// Inserts one non-empty trimmed path into the path set.
fn insert_tool_path(path: &str, paths: &mut BTreeSet<String>) {
    let path = path.trim();
    if !path.is_empty() {
        paths.insert(path.to_owned());
    }
}

/// Returns whether one output status string implies an error.
fn is_error_status(status: &str) -> bool {
    let normalized = status.to_ascii_lowercase();
    normalized.contains("error")
        || normalized.contains("fail")
        || normalized.contains("abort")
        || normalized.contains("denied")
}

/// Returns the repo-relative path when the extracted path is already relative.
fn repo_relative_path(path: &str) -> Option<String> {
    Path::new(path).is_relative().then(|| path.to_owned())
}

/// Returns the basename for one normalized access path when it is valid UTF-8.
fn path_file_name(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::to_owned)
}
