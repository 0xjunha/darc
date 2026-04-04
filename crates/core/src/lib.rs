mod active_project;
pub mod config;
pub(crate) mod constants;
mod index_db;
mod init;
mod parse;
mod project_paths;
mod rollout;
mod sync;
mod turn_metrics;
pub(crate) mod versions;

pub use config::SourceKind;
pub use init::{DetectedRolloutSource, InitDraft, default_root_path, prepare_init, write_init};
pub use parse::{
    CodexRollout, CodexTurn, CodexTurnMessage, CodexTurnStatus, CodexTurnStep, ParseOptions,
    ParseReport, SkippedCodexRollout, SkippedRollout, parse_codex_rollout,
    parse_project_codex_turns, parse_project_sessions,
};
pub use rollout::ParseDeterminism;
pub use rollout::codex::{
    CodexSchemaAuditOptions, CodexSchemaAuditOutcome, CodexSchemaAuditReport, CodexSchemaDrift,
    run_codex_schema_audit, run_codex_schema_audit_with_progress,
};
pub use sync::{SyncOptions, SyncPlan, SyncReport, execute_sync, prepare_sync};
