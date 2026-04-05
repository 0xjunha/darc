mod active_project;
mod analytics;
pub mod config;
pub(crate) mod constants;
mod index_db;
mod init;
mod parse;
mod project;
mod project_paths;
mod rollout;
mod sync;
mod turn_metrics;
pub(crate) mod versions;

pub use analytics::{
    ClaudeRolloutAnalyticsReport, ClaudeSchemaAnalytics, report_claude_rollout_analytics,
};
pub use config::SourceKind;
pub use init::{DetectedRolloutSource, InitDraft, default_root_path, prepare_init, write_init};
pub use parse::{
    CodexRollout, CodexTurn, CodexTurnMessage, CodexTurnStatus, CodexTurnStep, ParseOptions,
    ParseReport, SkippedCodexRollout, SkippedRollout, parse_codex_rollout,
    parse_project_codex_turns, parse_project_sessions,
};
pub use project::{
    LinkReport, RefreshAllReport, RefreshOptions, RefreshReport, RemoveReport, RenameReport,
    link_project, refresh_all_projects, refresh_project, remove_project, rename_project,
};
pub use rollout::ParseDeterminism;
pub use rollout::claude::{
    ClaudeSchemaAuditOptions, ClaudeSchemaAuditOutcome, ClaudeSchemaAuditReport, ClaudeSchemaDrift,
    ClaudeSchemaDriftWindow, ClaudeSchemaSurveyMode, ClaudeSdkSchemaDrift, run_claude_schema_audit,
    run_claude_schema_audit_with_progress,
};
pub use rollout::codex::{
    CodexSchemaAuditOptions, CodexSchemaAuditOutcome, CodexSchemaAuditReport, CodexSchemaDrift,
    run_codex_schema_audit, run_codex_schema_audit_with_progress,
};
pub use sync::{SyncOptions, SyncPlan, SyncReport, execute_sync, prepare_sync};
