use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    ProjectLayout, Result, RunId, WikiError,
    fs_utils::{read_dir_sorted, write_string_atomically},
};

/// Names the canonical TOML file used for durable run state.
pub const RUN_STATE_FILE_NAME: &str = "run.toml";

const RUN_SCHEMA_VERSION: u32 = 1;

/// Stores the durable lifecycle state for one digest run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    CancelRequested,
    Canceled,
    Interrupted,
}

/// Stores the current execution phase for one digest run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    PreparingContext,
    ReadingTurns,
    WaitingForAgent,
    ValidatingProposal,
    MergingEntries,
    WritingArtifacts,
}

/// Stores the full durable run state persisted in `run.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub run_id: RunId,
    pub project_id: String,
    pub status: RunStatus,
    pub phase: RunPhase,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_source: Option<String>,
    #[serde(default = "default_attempt")]
    pub attempt: u32,
    #[serde(default)]
    pub cancel_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_profile: Option<String>,
    #[serde(default)]
    pub selected_sessions: Vec<String>,
    #[serde(default)]
    pub target_categories: Vec<String>,
    #[serde(default)]
    pub target_domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_log_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_log_path: Option<String>,
    #[serde(default)]
    pub created_entry_ids: Vec<String>,
    #[serde(default)]
    pub updated_entry_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Stores the read-side summary for one durable digest run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: RunId,
    pub project_id: String,
    pub status: RunStatus,
    pub phase: RunPhase,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub run_dir: PathBuf,
    pub run_state_path: PathBuf,
}

impl RunSummary {
    /// Builds one run summary from a persisted run state and resolved paths.
    fn from_state(state: RunState, run_dir: PathBuf, run_state_path: PathBuf) -> Self {
        Self {
            run_id: state.run_id,
            project_id: state.project_id,
            status: state.status,
            phase: state.phase,
            created_at: state.created_at,
            updated_at: state.updated_at,
            heartbeat_at: state.heartbeat_at,
            finished_at: state.finished_at,
            headline: state.headline,
            pid: state.pid,
            run_dir,
            run_state_path,
        }
    }
}

/// Loads one durable run state from the canonical `run.toml` path.
pub fn load_run_state(layout: &ProjectLayout, run_id: &RunId) -> Result<RunState> {
    layout.validate_storage()?;
    let path = layout.run_state_path(run_id);
    let content = fs::read_to_string(&path).map_err(|source| WikiError::ReadFile {
        path: path.clone(),
        source,
    })?;
    let state = toml::from_str::<RunState>(&content).map_err(|source| WikiError::ParseToml {
        path: path.clone(),
        source,
    })?;
    validate_run_schema_version(&path, state.schema_version)?;
    validate_run_project(layout, &state)?;
    Ok(state)
}

/// Stores one durable run state into the canonical `run.toml` location.
pub fn store_run_state(layout: &ProjectLayout, state: &RunState) -> Result<()> {
    layout.ensure()?;
    let path = layout.run_state_path(&state.run_id);
    validate_run_schema_version(&path, state.schema_version)?;
    validate_run_project(layout, state)?;
    let run_dir = layout.run_dir(&state.run_id);
    fs::create_dir_all(&run_dir).map_err(|source| WikiError::CreateDir {
        path: run_dir.clone(),
        source,
    })?;
    let content = toml::to_string_pretty(state).map_err(|source| WikiError::SerializeToml {
        path: path.clone(),
        source,
    })?;
    write_string_atomically(&path, &content)
}

/// Lists every durable run summary for one project in deterministic order.
pub fn list_runs(layout: &ProjectLayout) -> Result<Vec<RunSummary>> {
    layout.validate_storage()?;
    if !layout.runs_dir.exists() {
        return Ok(Vec::new());
    }

    let mut runs = Vec::new();
    for entry in read_dir_sorted(&layout.runs_dir)? {
        let run_dir = entry.path();
        let file_type = entry.file_type().map_err(|source| WikiError::ReadDir {
            path: layout.runs_dir.clone(),
            source,
        })?;
        if !file_type.is_dir() {
            continue;
        }

        let run_state_path = run_dir.join(RUN_STATE_FILE_NAME);
        if !run_state_path.exists() {
            continue;
        }

        let state = load_run_state(
            layout,
            &RunId::new(entry.file_name().to_string_lossy().into_owned())?,
        )?;
        runs.push(RunSummary::from_state(state, run_dir, run_state_path));
    }
    runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
    Ok(runs)
}

/// Validates that one run state belongs to the same project as the target layout.
fn validate_run_project(layout: &ProjectLayout, state: &RunState) -> Result<()> {
    if state.project_id == layout.project_id {
        Ok(())
    } else {
        Err(WikiError::RunProjectMismatch {
            run_id: state.run_id.to_string(),
            expected_project_id: layout.project_id.clone(),
            actual_project_id: state.project_id.clone(),
        })
    }
}

/// Validates one persisted run schema version against the current implementation.
fn validate_run_schema_version(path: &std::path::Path, schema_version: u32) -> Result<()> {
    if schema_version == RUN_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(WikiError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            expected: RUN_SCHEMA_VERSION,
            actual: schema_version,
        })
    }
}

/// Returns the fixed run schema version.
fn default_schema_version() -> u32 {
    RUN_SCHEMA_VERSION
}

/// Returns the default attempt number for one new run state.
fn default_attempt() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::ContextWikiLayout;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_test_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!(
            "darc-wiki-{label}-{}-{nanos}-{counter}",
            std::process::id()
        ))
    }

    #[test]
    fn run_state_round_trips_and_lists_in_run_id_order() -> Result<()> {
        let darc_root = unique_test_dir("run-state");
        let layout = ContextWikiLayout::new(&darc_root).project_layout("repo-123")?;
        layout.ensure()?;

        let later = RunState {
            schema_version: RUN_SCHEMA_VERSION,
            run_id: RunId::new("cwrun_02later")?,
            project_id: "repo-123".to_owned(),
            status: RunStatus::Running,
            phase: RunPhase::WaitingForAgent,
            created_at: "2026-04-13T10:00:00Z".to_owned(),
            started_at: Some("2026-04-13T10:00:01Z".to_owned()),
            updated_at: "2026-04-13T10:00:02Z".to_owned(),
            finished_at: None,
            heartbeat_at: None,
            requested_by: Some("desktop".to_owned()),
            request_source: Some("darc-desktop/0.1.0".to_owned()),
            attempt: 1,
            cancel_requested: false,
            pid: Some(42),
            agent_id: Some("codex".to_owned()),
            runtime: Some("external_cli".to_owned()),
            model: Some("gpt-5.4".to_owned()),
            auth_profile: Some("openai/default".to_owned()),
            selected_sessions: vec!["codex:019d810f-570e".to_owned()],
            target_categories: vec!["architecture".to_owned()],
            target_domains: vec!["query-protocol".to_owned()],
            progress_percent: Some(30),
            headline: Some("Reading narrative turns".to_owned()),
            proposal_path: Some("proposal.json".to_owned()),
            result_path: None,
            events_path: Some("events.jsonl".to_owned()),
            stdout_log_path: Some("agent.stdout.log".to_owned()),
            stderr_log_path: Some("agent.stderr.log".to_owned()),
            created_entry_ids: Vec::new(),
            updated_entry_ids: Vec::new(),
            digest_id: None,
            error_code: None,
            error_message: None,
        };
        let earlier = RunState {
            run_id: RunId::new("cwrun_01early")?,
            created_at: "2026-04-13T09:59:00Z".to_owned(),
            updated_at: "2026-04-13T09:59:00Z".to_owned(),
            status: RunStatus::Queued,
            phase: RunPhase::PreparingContext,
            ..later.clone()
        };

        store_run_state(&layout, &later)?;
        store_run_state(&layout, &earlier)?;

        let loaded = load_run_state(&layout, &later.run_id)?;
        assert_eq!(loaded, later);

        let runs = list_runs(&layout)?;
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].run_id.as_str(), "cwrun_01early");
        assert_eq!(runs[1].run_id.as_str(), "cwrun_02later");

        fs::remove_dir_all(&darc_root).map_err(|source| WikiError::ReadDir {
            path: darc_root,
            source,
        })?;
        Ok(())
    }
}
