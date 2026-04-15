use std::path::PathBuf;

use darc_agent::ProposalOutputSource;
use darc_paths::current_utc_timestamp;
use darc_wiki::{
    EntryId, EntryStatus, ProjectLayout, ProjectRegistry, ProposalValidationError, RunId, RunPhase,
    RunStatus,
};
use serde::{Deserialize, Serialize};

use super::{RUN_EVENT_LEVEL_INFO, RUN_EVENT_LEVEL_WARN};
use crate::query::{SessionSummary, TurnDetail};

/// Collects the empty or populated read-side wiki payload for one configured project.
#[derive(Debug, Clone)]
pub struct ProjectWikiData {
    pub project_id: String,
    pub layout: ProjectLayout,
    pub registry: ProjectRegistry,
    pub entries: Vec<darc_wiki::EntrySummary>,
    pub digests: Vec<darc_wiki::DigestSummary>,
    pub runs: Vec<darc_wiki::RunSummary>,
}

/// Stores one start request for a new digest run.
#[derive(Debug, Clone)]
pub struct DigestStartOptions {
    pub session_refs: Vec<String>,
    pub agent_id: String,
    pub runtime: String,
    pub model: String,
    pub auth_profile: Option<String>,
    pub requested_by: Option<String>,
    pub request_source: Option<String>,
    pub target_categories: Vec<String>,
    pub target_domains: Vec<String>,
}

/// Stores one prepared digest run before the CLI spawns the hidden worker.
#[derive(Debug, Clone, Serialize)]
pub struct PreparedDigestRun {
    pub project_id: String,
    pub run_id: RunId,
    pub status: RunStatus,
    pub phase: RunPhase,
    pub stdout_log_path: PathBuf,
    pub stderr_log_path: PathBuf,
}

/// Reports one started digest run back to the CLI.
#[derive(Debug, Clone, Serialize)]
pub struct DigestStartReport {
    pub project_id: String,
    pub run_id: RunId,
    pub status: RunStatus,
    pub phase: RunPhase,
    pub pid: u32,
}

/// Reports one canceled or already-finished digest run back to the CLI.
#[derive(Debug, Clone, Serialize)]
pub struct DigestCancelReport {
    pub project_id: String,
    pub run_id: RunId,
    pub status: RunStatus,
    pub phase: RunPhase,
    pub cancel_requested: bool,
    pub pid: Option<u32>,
}

/// Reports one canonical wiki entry lifecycle mutation back to the CLI.
#[derive(Debug, Clone, Serialize)]
pub struct EntryMutationReport {
    pub project_id: String,
    pub entry_id: EntryId,
    pub previous_status: EntryStatus,
    pub status: EntryStatus,
    pub updated_at: String,
    pub changed: bool,
}

/// Stores one persisted digest start request artifact.
#[derive(Debug, Serialize)]
pub(super) struct DigestRequestArtifact {
    pub schema: String,
    pub project_id: String,
    pub run_id: String,
    pub selected_sessions: Vec<String>,
    pub target_categories: Vec<String>,
    pub target_domains: Vec<String>,
    pub agent_id: String,
    pub runtime: String,
    pub model: String,
    pub auth_profile: Option<String>,
    pub requested_by: String,
    pub request_source: String,
    pub created_at: String,
}

/// Stores one persisted digest context artifact.
#[derive(Debug, Serialize)]
pub(super) struct DigestContextArtifact {
    pub schema: String,
    pub project_id: String,
    pub run_id: String,
    pub selected_sessions: Vec<String>,
    pub target_categories: Vec<String>,
    pub target_domains: Vec<String>,
    pub registry: ProjectRegistry,
    pub sessions: Vec<DigestContextSession>,
    pub generated_at: String,
}

/// Stores one selected session plus narrative turn details in the digest context artifact.
#[derive(Debug, Clone, Serialize)]
pub(super) struct DigestContextSession {
    pub session: SessionSummary,
    pub turns: Vec<TurnDetail>,
}

/// Stores the runtime execution metadata captured for one digest run.
#[derive(Debug, Clone)]
pub(super) struct RuntimeExecution {
    pub display_name: String,
    pub proposal_source: ProposalOutputSource,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub proposal_bytes: Option<Vec<u8>>,
}

/// Stores one durable result artifact for runtime and validation reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DigestResultArtifact {
    pub schema: String,
    pub project_id: String,
    pub run_id: String,
    pub status: RunStatus,
    pub completed_at: String,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub runtime: DigestRuntimeArtifact,
    pub validation: DigestValidationArtifact,
    pub note: Option<String>,
}

/// Stores the runtime execution details embedded in `result.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DigestRuntimeArtifact {
    pub agent_id: Option<String>,
    pub runtime: Option<String>,
    pub model: Option<String>,
    pub auth_profile: Option<String>,
    pub display_name: Option<String>,
    pub exit_code: Option<i32>,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub proposal_source: Option<String>,
    pub proposal_captured: bool,
}

/// Stores the proposal validation summary embedded in `result.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct DigestValidationArtifact {
    pub attempted: bool,
    pub valid: bool,
    pub entry_count: Option<usize>,
    pub run_summary_title: Option<String>,
    pub extracted_decision_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ProposalValidationError>,
}

/// Stores one JSONL progress event for one digest run.
#[derive(Debug, Serialize)]
pub(super) struct RunEvent {
    pub ts: String,
    pub level: String,
    pub phase: RunPhase,
    pub message: String,
}

impl RunEvent {
    /// Builds one informational run event.
    pub(super) fn info(phase: RunPhase, message: String) -> Self {
        Self {
            ts: current_utc_timestamp(),
            level: RUN_EVENT_LEVEL_INFO.to_owned(),
            phase,
            message,
        }
    }

    /// Builds one warning run event.
    pub(super) fn warn(phase: RunPhase, message: String) -> Self {
        Self {
            ts: current_utc_timestamp(),
            level: RUN_EVENT_LEVEL_WARN.to_owned(),
            phase,
            message,
        }
    }
}
