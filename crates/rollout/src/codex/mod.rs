mod error;
mod header;
mod parser;
#[cfg(test)]
mod tests;
mod version;

pub use error::{
    CodexCliVersionParseError, CodexError, CodexSchemaError, ParseIntoError, ParseIntoResult,
};
pub use header::{
    CodexRolloutHeader, CodexRolloutSessionMeta, parse_rollout_file_session_id,
    parse_rollout_session_meta_line, read_first_rollout_line_bytes, read_rollout_header,
    read_rollout_session_meta, reconcile_rollout_session_id,
};
pub use parser::{
    CodexRollout, CodexRolloutSink, compare_rollout_priority, parse_rollout_file,
    parse_rollout_file_into,
};
pub use version::{
    CodexCliVersion, latest_exact_supported_codex_cli_version, resolve_codex_parse_determinism,
};
