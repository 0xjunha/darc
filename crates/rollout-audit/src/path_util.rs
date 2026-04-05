use std::path::Path;

/// Encodes a project path using Claude's directory naming rule.
pub(crate) fn encode_path_for_claude(path: &Path) -> String {
    path.to_string_lossy().replace('/', "-")
}
