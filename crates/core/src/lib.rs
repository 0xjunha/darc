mod active_project;
mod analytics;
pub mod config;
pub(crate) mod constants;
mod index;
mod init;
mod project;
mod sync;
pub(crate) mod versions;

pub use analytics::{
    ClaudeRolloutAnalyticsReport, ClaudeSchemaAnalytics, report_claude_rollout_analytics,
};
pub use config::SourceKind;
pub use darc_rollout::codex::CodexRollout;
pub use darc_rollout::model::{
    NormalizedTurn as CodexTurn, NormalizedTurnMessage as CodexTurnMessage,
    NormalizedTurnStatus as CodexTurnStatus, NormalizedTurnStep as CodexTurnStep,
};
pub use darc_rollout::{ParseDeterminism, claude, codex, model};
pub use darc_rollout_audit::claude::{
    ClaudeSchemaAuditOptions, ClaudeSchemaAuditOutcome, ClaudeSchemaAuditReport, ClaudeSchemaDrift,
    ClaudeSchemaDriftWindow, ClaudeSchemaSurveyMode, ClaudeSdkSchemaDrift, run_claude_schema_audit,
    run_claude_schema_audit_with_progress,
};
pub use darc_rollout_audit::codex::{
    CodexSchemaAuditOptions, CodexSchemaAuditOutcome, CodexSchemaAuditReport, CodexSchemaDrift,
    run_codex_schema_audit, run_codex_schema_audit_with_progress,
};
pub use darc_rollout_audit::{claude as claude_audit, codex as codex_audit};
pub use index::{
    IndexOptions, IndexReport, SkippedCodexRollout, SkippedRollout, index_project_codex_turns,
    index_project_sessions, parse_codex_rollout,
};
pub use init::{DetectedRolloutSource, InitDraft, default_root_path, prepare_init, write_init};
pub use project::{
    LinkReport, RefreshAllReport, RefreshOptions, RefreshReport, RemoveReport, RenameReport,
    link_project, refresh_all_projects, refresh_project, remove_project, rename_project,
};
pub use sync::{SyncOptions, SyncPlan, SyncReport, execute_sync, prepare_sync};
