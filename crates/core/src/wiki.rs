use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use darc_agent::{
    AgentId, ProposalOutputSource, RuntimeKind, RuntimeRequest, build_runtime_command,
};
use darc_paths::{SourceKind, current_utc_timestamp, parse_utc_timestamp};
use darc_wiki::{
    ContextWikiLayout, DIGEST_PROPOSAL_OUTPUT_SCHEMA_JSON, DIGEST_PROPOSAL_SCHEMA, DigestProposal,
    ProjectLayout, ProjectRegistry, ProposalValidationError, ProposalValidationOptions, RunId,
    RunPhase, RunState, RunStatus, ensure_registry, is_valid_domain_id, list_digests, list_entries,
    list_runs, load_registry, load_run_state, store_run_state, validate_digest_proposal,
};
use serde::Serialize;

use crate::{
    config::ProjectConfig,
    default_root_path,
    project::registered_projects,
    query::{
        SessionSummary, TurnDetail, TurnDetailOptions, query_sessions, query_turn, query_turns,
    },
};

static RUN_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

const RUN_REQUEST_SCHEMA: &str = "darc.wiki.digest.request.v1";
const RUN_CONTEXT_SCHEMA: &str = "darc.wiki.digest.context.v1";
const RUN_RESULT_SCHEMA: &str = "darc.wiki.digest.result.v1";
const RUN_EVENT_LEVEL_INFO: &str = "info";
const RUN_EVENT_LEVEL_WARN: &str = "warn";
const DEFAULT_REQUESTED_BY: &str = "cli";
const RUN_POLL_INTERVAL: Duration = Duration::from_millis(200);
const RUN_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const RUN_STALE_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(2);
const PROPOSAL_SCHEMA_FILE_NAME: &str = "proposal.schema.json";

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

/// Ensures the per-project wiki directory tree exists for one configured project.
pub fn ensure_project_wiki(root: Option<PathBuf>, project_id: &str) -> Result<ProjectLayout> {
    let layout = resolve_project_layout(root, project_id)?;
    ensure_registry(&layout).context("failed to initialize project wiki registry")?;
    Ok(layout)
}

/// Loads the read-side wiki payload after ensuring the project layout exists.
pub fn load_project_wiki(root: Option<PathBuf>, project_id: &str) -> Result<ProjectWikiData> {
    let layout = ensure_project_wiki(root, project_id)?;
    Ok(ProjectWikiData {
        project_id: project_id.to_owned(),
        registry: load_registry(&layout).context("failed to load project wiki registry")?,
        entries: list_entries(&layout).context("failed to list wiki entries")?,
        digests: list_digests(&layout).context("failed to list wiki digests")?,
        runs: load_visible_run_summaries(&layout).context("failed to list wiki runs")?,
        layout,
    })
}

/// Loads one durable wiki run state for one configured project.
pub fn load_project_wiki_run(
    root: Option<PathBuf>,
    project_id: &str,
    run_id: &RunId,
) -> Result<RunState> {
    let layout = ensure_project_wiki(root, project_id)?;
    repair_run_if_stale(&layout, run_id).context("failed to repair stale wiki run")?;
    load_run_state(&layout, run_id).context("failed to load wiki run state")
}

/// Stores one durable wiki run state for one configured project.
pub fn store_project_wiki_run(
    root: Option<PathBuf>,
    project_id: &str,
    run_state: &RunState,
) -> Result<()> {
    let layout = ensure_project_wiki(root, project_id)?;
    store_run_state(&layout, run_state).context("failed to store wiki run state")
}

/// Prepares one new digest run and writes the initial artifacts before worker spawn.
pub fn prepare_project_wiki_digest_start(
    root: Option<PathBuf>,
    project_id: &str,
    options: &DigestStartOptions,
) -> Result<PreparedDigestRun> {
    validate_digest_start_options(options)?;
    let layout = ensure_project_wiki(root, project_id)?;
    validate_digest_targets(&layout, options)?;
    let now = current_utc_timestamp();
    let run_id = next_run_id(&layout)?;
    let run_dir = layout.run_dir(&run_id);
    fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create {}", run_dir.display()))?;

    let requested_by = options
        .requested_by
        .clone()
        .unwrap_or_else(|| DEFAULT_REQUESTED_BY.to_owned());
    let request_source = options
        .request_source
        .clone()
        .unwrap_or_else(|| format!("darc/{}", env!("CARGO_PKG_VERSION")));
    let request = DigestRequestArtifact {
        schema: RUN_REQUEST_SCHEMA.to_owned(),
        project_id: project_id.to_owned(),
        run_id: run_id.to_string(),
        selected_sessions: options.session_refs.clone(),
        target_categories: options.target_categories.clone(),
        target_domains: options.target_domains.clone(),
        agent_id: options.agent_id.clone(),
        runtime: options.runtime.clone(),
        model: options.model.clone(),
        auth_profile: options.auth_profile.clone(),
        requested_by: requested_by.clone(),
        request_source: request_source.clone(),
        created_at: now.clone(),
    };
    let context = DigestContextArtifact {
        schema: RUN_CONTEXT_SCHEMA.to_owned(),
        project_id: project_id.to_owned(),
        run_id: run_id.to_string(),
        selected_sessions: options.session_refs.clone(),
        target_categories: options.target_categories.clone(),
        target_domains: options.target_domains.clone(),
        registry: load_registry(&layout)?,
        sessions: Vec::new(),
        generated_at: now.clone(),
    };
    write_json_artifact(&layout.run_request_path(&run_id), &request)?;
    write_json_artifact(&layout.run_context_path(&run_id), &context)?;
    touch_file(&layout.run_events_path(&run_id))?;
    touch_file(&layout.run_stdout_log_path(&run_id))?;
    touch_file(&layout.run_stderr_log_path(&run_id))?;

    let state = RunState {
        schema_version: 1,
        run_id: run_id.clone(),
        project_id: project_id.to_owned(),
        status: RunStatus::Queued,
        phase: RunPhase::PreparingContext,
        created_at: now.clone(),
        started_at: None,
        updated_at: now.clone(),
        finished_at: None,
        heartbeat_at: None,
        requested_by: Some(requested_by),
        request_source: Some(request_source),
        attempt: 1,
        cancel_requested: false,
        pid: None,
        agent_id: Some(options.agent_id.clone()),
        runtime: Some(options.runtime.clone()),
        model: Some(options.model.clone()),
        auth_profile: options.auth_profile.clone(),
        selected_sessions: options.session_refs.clone(),
        target_categories: options.target_categories.clone(),
        target_domains: options.target_domains.clone(),
        progress_percent: Some(0),
        headline: Some("Queued digest worker".to_owned()),
        proposal_path: Some(relative_artifact_name(layout.run_proposal_path(&run_id))),
        result_path: Some(relative_artifact_name(layout.run_result_path(&run_id))),
        events_path: Some(relative_artifact_name(layout.run_events_path(&run_id))),
        stdout_log_path: Some(relative_artifact_name(layout.run_stdout_log_path(&run_id))),
        stderr_log_path: Some(relative_artifact_name(layout.run_stderr_log_path(&run_id))),
        created_entry_ids: Vec::new(),
        updated_entry_ids: Vec::new(),
        digest_id: None,
        error_code: None,
        error_message: None,
    };
    store_run_state(&layout, &state)?;
    append_run_event(
        &layout,
        &run_id,
        RunEvent::info(
            RunPhase::PreparingContext,
            format!(
                "Queued digest run for {} selected session(s)",
                state.selected_sessions.len()
            ),
        ),
    )?;

    Ok(PreparedDigestRun {
        project_id: project_id.to_owned(),
        run_id,
        status: state.status,
        phase: state.phase,
        stdout_log_path: layout.run_stdout_log_path(&state.run_id),
        stderr_log_path: layout.run_stderr_log_path(&state.run_id),
    })
}

/// Marks one prepared digest run as actively started after the CLI spawns the worker.
pub fn mark_project_wiki_digest_started(
    root: Option<PathBuf>,
    project_id: &str,
    run_id: &RunId,
    pid: u32,
) -> Result<DigestStartReport> {
    let layout = ensure_project_wiki(root, project_id)?;
    let mut state = load_run_state(&layout, run_id)?;
    let started_at = current_utc_timestamp();
    state.status = RunStatus::Running;
    state.started_at = Some(started_at.clone());
    state.updated_at = started_at.clone();
    state.heartbeat_at = Some(started_at.clone());
    state.pid = Some(pid);
    state.headline = Some("Preparing digest context".to_owned());
    store_run_state(&layout, &state)?;
    append_run_event(
        &layout,
        run_id,
        RunEvent::info(
            RunPhase::PreparingContext,
            format!("Started digest worker process with pid {pid}"),
        ),
    )?;

    Ok(DigestStartReport {
        project_id: project_id.to_owned(),
        run_id: run_id.clone(),
        status: state.status,
        phase: state.phase,
        pid,
    })
}

/// Marks one prepared digest run as failed because the worker could not be spawned.
pub fn fail_project_wiki_digest_start(
    root: Option<PathBuf>,
    project_id: &str,
    run_id: &RunId,
    error_message: &str,
) -> Result<()> {
    let layout = ensure_project_wiki(root, project_id)?;
    let mut state = load_run_state(&layout, run_id)?;
    let failed_at = current_utc_timestamp();
    state.status = RunStatus::Failed;
    state.updated_at = failed_at.clone();
    state.finished_at = Some(failed_at.clone());
    state.heartbeat_at = Some(failed_at);
    state.headline = Some("Failed to spawn digest worker".to_owned());
    state.error_code = Some("worker_spawn_failed".to_owned());
    state.error_message = Some(error_message.to_owned());
    store_run_state(&layout, &state)?;
    append_run_event(
        &layout,
        run_id,
        RunEvent::warn(
            RunPhase::PreparingContext,
            "Failed to spawn digest worker".to_owned(),
        ),
    )?;
    Ok(())
}

/// Cancels one existing digest run and finalizes durable state when the worker exits.
pub fn cancel_project_wiki_digest(
    root: Option<PathBuf>,
    project_id: &str,
    run_id: &RunId,
) -> Result<DigestCancelReport> {
    let layout = ensure_project_wiki(root, project_id)?;
    repair_run_if_stale(&layout, run_id).context("failed to repair stale wiki run")?;
    let mut state = load_run_state(&layout, run_id)?;
    if is_finished_status(state.status) {
        return Ok(report_from_run_state(&state));
    }

    let cancel_flag_path = layout.run_cancel_flag_path(run_id);
    touch_file(&cancel_flag_path)?;
    let now = current_utc_timestamp();
    state.cancel_requested = true;
    if matches!(state.status, RunStatus::Queued | RunStatus::Running) {
        state.status = RunStatus::CancelRequested;
    }
    state.updated_at = now.clone();
    state.heartbeat_at = Some(now.clone());
    state.headline = Some("Cancel requested".to_owned());
    store_run_state(&layout, &state)?;
    append_run_event(
        &layout,
        run_id,
        RunEvent::info(RunPhase::WaitingForAgent, "Cancel requested".to_owned()),
    )?;

    Ok(report_from_run_state(&state))
}

/// Runs the hidden digest worker loop for one existing run.
pub fn run_project_wiki_digest_worker(
    root: Option<PathBuf>,
    project_id: &str,
    run_id: &RunId,
) -> Result<()> {
    let root = root.unwrap_or_else(default_root_path);
    let layout = ensure_project_wiki(Some(root.clone()), project_id)?;
    let mut state = wait_for_worker_registration(&layout, run_id)?;
    if is_finished_status(state.status) {
        return Ok(());
    }

    let registry = load_registry(&layout).context("failed to load project wiki registry")?;
    transition_worker_state(
        &layout,
        run_id,
        RunPhase::ReadingTurns,
        Some(10),
        "Preparing context bundle",
    )?;
    let session_summaries = match query_sessions(Some(root.clone()), project_id, None, None)
        .context("failed to load indexed session summaries for digest context")
    {
        Ok(data) => data.sessions,
        Err(error) => {
            let finished_at = current_utc_timestamp();
            write_digest_result(
                &layout,
                run_id,
                &build_result_artifact(
                    &state,
                    finished_at.clone(),
                    None,
                    DigestValidationArtifact::default(),
                    Some("context_build_failed".to_owned()),
                    Some(error.to_string()),
                    None,
                ),
            )?;
            finalize_run_failed(
                &layout,
                run_id,
                RunPhase::ReadingTurns,
                "Context build failed",
                "context_build_failed",
                &error.to_string(),
            )?;
            append_run_event(
                &layout,
                run_id,
                RunEvent::warn(
                    RunPhase::ReadingTurns,
                    "Failed to build digest context".to_owned(),
                ),
            )?;
            return Ok(());
        }
    };
    append_run_event(
        &layout,
        run_id,
        RunEvent::info(
            RunPhase::ReadingTurns,
            format!(
                "Resolved {} selected session reference(s)",
                state.selected_sessions.len()
            ),
        ),
    )?;

    let selected_sessions = state.selected_sessions.clone();
    let total_sessions = selected_sessions.len().max(1);
    let mut total_turns = 0_usize;
    let mut context_sessions = Vec::with_capacity(selected_sessions.len());
    for (index, session_ref) in selected_sessions.iter().enumerate() {
        transition_worker_state(
            &layout,
            run_id,
            RunPhase::ReadingTurns,
            Some(10 + (((index + 1) * 25) / total_sessions) as u8),
            &format!(
                "Reading narrative turns for session {} of {}",
                index + 1,
                total_sessions
            ),
        )?;
        let session =
            match load_selected_session_context(&root, project_id, session_ref, &session_summaries)
            {
                Ok(session) => session,
                Err(error) => {
                    let finished_at = current_utc_timestamp();
                    write_digest_result(
                        &layout,
                        run_id,
                        &build_result_artifact(
                            &state,
                            finished_at.clone(),
                            None,
                            DigestValidationArtifact::default(),
                            Some("context_build_failed".to_owned()),
                            Some(error.to_string()),
                            None,
                        ),
                    )?;
                    finalize_run_failed(
                        &layout,
                        run_id,
                        RunPhase::ReadingTurns,
                        "Context build failed",
                        "context_build_failed",
                        &error.to_string(),
                    )?;
                    append_run_event(
                        &layout,
                        run_id,
                        RunEvent::warn(
                            RunPhase::ReadingTurns,
                            format!("Failed to load session context for `{session_ref}`"),
                        ),
                    )?;
                    return Ok(());
                }
            };
        total_turns += session.turns.len();
        append_run_event(
            &layout,
            run_id,
            RunEvent::info(
                RunPhase::ReadingTurns,
                format!(
                    "Loaded {} narrative turn(s) from `{session_ref}`",
                    session.turns.len()
                ),
            ),
        )?;
        context_sessions.push(session);
        state = load_run_state(&layout, run_id)?;
        if cancel_requested(&layout, run_id, &state)? {
            finalize_run_canceled(
                &layout,
                run_id,
                RunPhase::ReadingTurns,
                "Digest run canceled",
            )?;
            append_run_event(
                &layout,
                run_id,
                RunEvent::info(RunPhase::ReadingTurns, "Digest run canceled".to_owned()),
            )?;
            return Ok(());
        }
    }
    let context = DigestContextArtifact {
        schema: RUN_CONTEXT_SCHEMA.to_owned(),
        project_id: project_id.to_owned(),
        run_id: run_id.to_string(),
        selected_sessions: state.selected_sessions.clone(),
        target_categories: state.target_categories.clone(),
        target_domains: state.target_domains.clone(),
        registry: registry.clone(),
        sessions: context_sessions,
        generated_at: current_utc_timestamp(),
    };
    write_json_artifact(&layout.run_context_path(run_id), &context)?;
    append_run_event(
        &layout,
        run_id,
        RunEvent::info(
            RunPhase::ReadingTurns,
            format!(
                "Loaded {total_turns} narrative turn(s) across {} session(s)",
                state.selected_sessions.len()
            ),
        ),
    )?;
    state = load_run_state(&layout, run_id)?;
    if cancel_requested(&layout, run_id, &state)? {
        finalize_run_canceled(
            &layout,
            run_id,
            RunPhase::ReadingTurns,
            "Digest run canceled",
        )?;
        append_run_event(
            &layout,
            run_id,
            RunEvent::info(RunPhase::ReadingTurns, "Digest run canceled".to_owned()),
        )?;
        return Ok(());
    }

    transition_worker_state(
        &layout,
        run_id,
        RunPhase::WaitingForAgent,
        Some(40),
        "Preparing agent runtime",
    )?;
    let proposal_schema_path = layout.run_dir(run_id).join(PROPOSAL_SCHEMA_FILE_NAME);
    write_text_artifact(&proposal_schema_path, DIGEST_PROPOSAL_OUTPUT_SCHEMA_JSON)?;
    let prompt = build_digest_runtime_prompt(&context)?;
    let runtime_request = match build_runtime_request(
        &root,
        project_id,
        &layout,
        run_id,
        &state,
        &prompt,
        &proposal_schema_path,
    ) {
        Ok(request) => request,
        Err(error) => {
            let finished_at = current_utc_timestamp();
            write_digest_result(
                &layout,
                run_id,
                &build_result_artifact(
                    &state,
                    finished_at.clone(),
                    None,
                    DigestValidationArtifact::default(),
                    Some("runtime_prepare_failed".to_owned()),
                    Some(error.to_string()),
                    None,
                ),
            )?;
            finalize_run_failed(
                &layout,
                run_id,
                RunPhase::WaitingForAgent,
                "Agent runtime preparation failed",
                "runtime_prepare_failed",
                &error.to_string(),
            )?;
            append_run_event(
                &layout,
                run_id,
                RunEvent::warn(
                    RunPhase::WaitingForAgent,
                    "Failed to prepare digest runtime command".to_owned(),
                ),
            )?;
            return Ok(());
        }
    };
    let runtime_command = match build_runtime_command(&runtime_request) {
        Ok(command) => command,
        Err(error) => {
            let finished_at = current_utc_timestamp();
            write_digest_result(
                &layout,
                run_id,
                &build_result_artifact(
                    &state,
                    finished_at.clone(),
                    None,
                    DigestValidationArtifact::default(),
                    Some("runtime_prepare_failed".to_owned()),
                    Some(error.to_string()),
                    None,
                ),
            )?;
            finalize_run_failed(
                &layout,
                run_id,
                RunPhase::WaitingForAgent,
                "Agent runtime preparation failed",
                "runtime_prepare_failed",
                &error.to_string(),
            )?;
            append_run_event(
                &layout,
                run_id,
                RunEvent::warn(
                    RunPhase::WaitingForAgent,
                    "Failed to prepare digest runtime command".to_owned(),
                ),
            )?;
            return Ok(());
        }
    };
    transition_worker_state(
        &layout,
        run_id,
        RunPhase::WaitingForAgent,
        Some(50),
        &format!("Invoking {}", runtime_command.display_name),
    )?;
    append_run_event(
        &layout,
        run_id,
        RunEvent::info(
            RunPhase::WaitingForAgent,
            format!("Started {}", runtime_command.display_name),
        ),
    )?;
    let runtime_execution = match execute_runtime_command(&layout, run_id, runtime_command) {
        Ok(execution) => execution,
        Err(error) => {
            let finished_at = current_utc_timestamp();
            write_digest_result(
                &layout,
                run_id,
                &build_result_artifact(
                    &state,
                    finished_at.clone(),
                    None,
                    DigestValidationArtifact::default(),
                    Some("runtime_invocation_failed".to_owned()),
                    Some(error.to_string()),
                    None,
                ),
            )?;
            finalize_run_failed(
                &layout,
                run_id,
                RunPhase::WaitingForAgent,
                "Agent runtime invocation failed",
                "runtime_invocation_failed",
                &error.to_string(),
            )?;
            append_run_event(
                &layout,
                run_id,
                RunEvent::warn(
                    RunPhase::WaitingForAgent,
                    "Agent runtime invocation failed".to_owned(),
                ),
            )?;
            return Ok(());
        }
    };
    state = load_run_state(&layout, run_id)?;
    if cancel_requested(&layout, run_id, &state)? {
        let finished_at = current_utc_timestamp();
        let note = runtime_execution.proposal_bytes.is_some().then_some(
            "Proposal capture completed but validation was skipped after cancel request".to_owned(),
        );
        write_digest_result(
            &layout,
            run_id,
            &build_result_artifact(
                &state,
                finished_at.clone(),
                Some(&runtime_execution),
                DigestValidationArtifact::default(),
                None,
                None,
                note.clone(),
            ),
        )?;
        finalize_run_canceled(
            &layout,
            run_id,
            RunPhase::WaitingForAgent,
            "Digest run canceled",
        )?;
        append_run_event(
            &layout,
            run_id,
            RunEvent::info(RunPhase::WaitingForAgent, "Digest run canceled".to_owned()),
        )?;
        return Ok(());
    }
    if runtime_execution.exit_code != Some(0) {
        let message = format!(
            "{} exited with code {}",
            runtime_execution.display_name,
            runtime_execution
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        );
        let finished_at = current_utc_timestamp();
        write_digest_result(
            &layout,
            run_id,
            &build_result_artifact(
                &state,
                finished_at.clone(),
                Some(&runtime_execution),
                DigestValidationArtifact::default(),
                Some("runtime_invocation_failed".to_owned()),
                Some(message.clone()),
                None,
            ),
        )?;
        finalize_run_failed(
            &layout,
            run_id,
            RunPhase::WaitingForAgent,
            "Agent runtime invocation failed",
            "runtime_invocation_failed",
            &message,
        )?;
        append_run_event(
            &layout,
            run_id,
            RunEvent::warn(
                RunPhase::WaitingForAgent,
                "Agent runtime exited unsuccessfully".to_owned(),
            ),
        )?;
        return Ok(());
    }
    let Some(proposal_bytes) = runtime_execution.proposal_bytes.as_ref() else {
        let message = "agent runtime did not produce a proposal artifact".to_owned();
        let finished_at = current_utc_timestamp();
        write_digest_result(
            &layout,
            run_id,
            &build_result_artifact(
                &state,
                finished_at.clone(),
                Some(&runtime_execution),
                DigestValidationArtifact::default(),
                Some("proposal_missing".to_owned()),
                Some(message.clone()),
                None,
            ),
        )?;
        finalize_run_failed(
            &layout,
            run_id,
            RunPhase::WaitingForAgent,
            "Proposal artifact missing",
            "proposal_missing",
            &message,
        )?;
        append_run_event(
            &layout,
            run_id,
            RunEvent::warn(
                RunPhase::WaitingForAgent,
                "Agent runtime did not produce a proposal artifact".to_owned(),
            ),
        )?;
        return Ok(());
    };
    if matches!(
        runtime_execution.proposal_source,
        ProposalOutputSource::Stdout
    ) {
        write_bytes_artifact(&layout.run_proposal_path(run_id), proposal_bytes)?;
    }

    transition_worker_state(
        &layout,
        run_id,
        RunPhase::ValidatingProposal,
        Some(80),
        "Validating proposal artifact",
    )?;
    let proposal_text = match String::from_utf8(proposal_bytes.clone()) {
        Ok(text) => text,
        Err(error) => {
            let finished_at = current_utc_timestamp();
            write_digest_result(
                &layout,
                run_id,
                &build_result_artifact(
                    &state,
                    finished_at.clone(),
                    Some(&runtime_execution),
                    DigestValidationArtifact::default(),
                    Some("proposal_not_utf8".to_owned()),
                    Some(error.to_string()),
                    None,
                ),
            )?;
            finalize_run_failed(
                &layout,
                run_id,
                RunPhase::ValidatingProposal,
                "Proposal artifact is not UTF-8",
                "proposal_not_utf8",
                &error.to_string(),
            )?;
            append_run_event(
                &layout,
                run_id,
                RunEvent::warn(
                    RunPhase::ValidatingProposal,
                    "Proposal artifact is not valid UTF-8".to_owned(),
                ),
            )?;
            return Ok(());
        }
    };
    let proposal = match serde_json::from_str::<DigestProposal>(&proposal_text) {
        Ok(proposal) => proposal,
        Err(error) => {
            let finished_at = current_utc_timestamp();
            write_digest_result(
                &layout,
                run_id,
                &build_result_artifact(
                    &state,
                    finished_at.clone(),
                    Some(&runtime_execution),
                    DigestValidationArtifact {
                        attempted: true,
                        ..DigestValidationArtifact::default()
                    },
                    Some("proposal_json_invalid".to_owned()),
                    Some(error.to_string()),
                    None,
                ),
            )?;
            finalize_run_failed(
                &layout,
                run_id,
                RunPhase::ValidatingProposal,
                "Proposal artifact is invalid JSON",
                "proposal_json_invalid",
                &error.to_string(),
            )?;
            append_run_event(
                &layout,
                run_id,
                RunEvent::warn(
                    RunPhase::ValidatingProposal,
                    "Proposal artifact could not be parsed as JSON".to_owned(),
                ),
            )?;
            return Ok(());
        }
    };
    let allowed_domains = build_allowed_domains(&registry, &state);
    let validation = match validate_digest_proposal(
        &proposal,
        &ProposalValidationOptions {
            project_id,
            run_id: run_id.as_str(),
            allowed_categories: &registry.categories,
            allowed_domains: &allowed_domains,
            selected_sessions: &state.selected_sessions,
        },
    ) {
        Ok(summary) => DigestValidationArtifact {
            attempted: true,
            valid: true,
            entry_count: Some(summary.entry_count),
            run_summary_title: Some(summary.run_summary_title),
            extracted_decision_count: Some(summary.extracted_decision_count),
            errors: Vec::new(),
        },
        Err(errors) => {
            let validation = DigestValidationArtifact {
                attempted: true,
                valid: false,
                entry_count: Some(proposal.entries.len()),
                run_summary_title: Some(proposal.run_summary.title.clone()),
                extracted_decision_count: Some(proposal.run_summary.extracted_decision_count),
                errors: errors.into_errors(),
            };
            let finished_at = current_utc_timestamp();
            write_digest_result(
                &layout,
                run_id,
                &build_result_artifact(
                    &state,
                    finished_at.clone(),
                    Some(&runtime_execution),
                    validation.clone(),
                    Some("proposal_validation_failed".to_owned()),
                    Some("proposal artifact failed validation".to_owned()),
                    None,
                ),
            )?;
            finalize_run_failed(
                &layout,
                run_id,
                RunPhase::ValidatingProposal,
                "Proposal validation failed",
                "proposal_validation_failed",
                "proposal artifact failed validation",
            )?;
            append_run_event(
                &layout,
                run_id,
                RunEvent::warn(
                    RunPhase::ValidatingProposal,
                    "Proposal artifact failed validation".to_owned(),
                ),
            )?;
            return Ok(());
        }
    };

    transition_worker_state(
        &layout,
        run_id,
        RunPhase::WritingArtifacts,
        Some(100),
        "Writing validation result",
    )?;
    let finished_at = current_utc_timestamp();
    let note = Some(
        "Canonical merge is deferred; this run succeeded after proposal validation only".to_owned(),
    );
    write_digest_result(
        &layout,
        run_id,
        &build_result_artifact(
            &state,
            finished_at.clone(),
            Some(&runtime_execution),
            validation.clone(),
            None,
            None,
            note,
        ),
    )?;
    finalize_run_succeeded(
        &layout,
        run_id,
        RunPhase::WritingArtifacts,
        "Validated proposal artifacts",
    )?;
    append_run_event(
        &layout,
        run_id,
        RunEvent::info(
            RunPhase::WritingArtifacts,
            format!(
                "Validated proposal with {} decision trace(s)",
                validation.extracted_decision_count.unwrap_or_default()
            ),
        ),
    )?;
    Ok(())
}

/// Resolves one validated project wiki layout from the configured Darc root.
fn resolve_project_layout(root: Option<PathBuf>, project_id: &str) -> Result<ProjectLayout> {
    let root = root.unwrap_or_else(default_root_path);
    let project = registered_projects(&root)?
        .into_iter()
        .find(|project| project.id == project_id)
        .with_context(|| format!("project id `{project_id}` was not found in the shared config"))?;
    ContextWikiLayout::new(root)
        .project_layout(project.id)
        .context("failed to resolve project wiki layout")
}

/// Validates the new digest start request before any artifact is written.
fn validate_digest_start_options(options: &DigestStartOptions) -> Result<()> {
    if options.session_refs.is_empty() {
        bail!("at least one --session-ref is required");
    }
    for session_ref in &options.session_refs {
        validate_session_ref(session_ref)?;
    }
    AgentId::parse(&options.agent_id).context("agent id is not supported")?;
    RuntimeKind::parse(&options.runtime).context("runtime is not supported")?;
    if options.model.trim().is_empty() {
        bail!("model must not be empty");
    }
    Ok(())
}

/// Validates one `provider:session-id` reference used to select wiki digest sessions.
fn validate_session_ref(session_ref: &str) -> Result<()> {
    let (_, session_id) = parse_session_ref(session_ref)?;
    if session_id.trim().is_empty() {
        bail!("session ref `{session_ref}` must include a non-empty session id");
    }
    Ok(())
}

/// Validates the target categories and domains recorded for one digest request.
fn validate_digest_targets(layout: &ProjectLayout, options: &DigestStartOptions) -> Result<()> {
    let registry = load_registry(layout)?;
    for category in &options.target_categories {
        if !registry.categories.contains(category) {
            bail!("target category `{category}` is not defined in the project registry");
        }
    }
    for domain in &options.target_domains {
        if !is_valid_domain_id(domain) {
            bail!("target domain `{domain}` must use lowercase slug format");
        }
    }
    Ok(())
}

/// Parses one selected session reference into its typed source kind and session id.
fn parse_session_ref(session_ref: &str) -> Result<(SourceKind, &str)> {
    let Some((provider, session_id)) = session_ref.split_once(':') else {
        bail!("session ref `{session_ref}` must use the `<provider>:<session-id>` format");
    };
    let provider = match provider {
        "claude" => SourceKind::Claude,
        "codex" => SourceKind::Codex,
        _ => bail!("session ref `{session_ref}` must start with `claude:` or `codex:`"),
    };
    Ok((provider, session_id))
}

/// Generates the next unused run id for one project layout.
fn next_run_id(layout: &ProjectLayout) -> Result<RunId> {
    loop {
        let counter = RUN_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let candidate = format!(
            "cwrun_{:08x}{:08x}{:04x}",
            now.as_secs(),
            now.subsec_nanos(),
            counter as u16
        );
        let run_id = RunId::new(candidate)?;
        if !layout.run_dir(&run_id).exists() {
            return Ok(run_id);
        }
    }
}

/// Writes one JSON artifact file with pretty formatting.
fn write_json_artifact<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let content =
        serde_json::to_string_pretty(value).context("failed to serialize JSON artifact")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, format!("{content}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Writes one UTF-8 text artifact file after ensuring its parent directory exists.
fn write_text_artifact(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))
}

/// Writes one opaque byte artifact file after ensuring its parent directory exists.
fn write_bytes_artifact(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

/// Creates one empty file after ensuring its parent directory exists.
fn touch_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    Ok(())
}

/// Returns the basename string for one run artifact path.
fn relative_artifact_name(path: PathBuf) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .expect("artifact paths should have a filename")
}

/// Appends one JSONL run event for progress reporting and debugging.
fn append_run_event(layout: &ProjectLayout, run_id: &RunId, event: RunEvent) -> Result<()> {
    let path = layout.run_events_path(run_id);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let line = serde_json::to_string(&event).context("failed to serialize run event")?;
    writeln!(file, "{line}").with_context(|| format!("failed to append {}", path.display()))
}

/// Applies the next worker-visible phase/progress transition to one run state.
fn transition_worker_state(
    layout: &ProjectLayout,
    run_id: &RunId,
    phase: RunPhase,
    progress_percent: Option<u8>,
    headline: &str,
) -> Result<()> {
    update_run_state(layout, run_id, |state| {
        let now = current_utc_timestamp();
        state.status = if state.cancel_requested {
            RunStatus::CancelRequested
        } else {
            RunStatus::Running
        };
        state.phase = phase;
        state.updated_at = now.clone();
        state.heartbeat_at = Some(now);
        state.progress_percent = progress_percent;
        state.headline = Some(headline.to_owned());
    })?;
    Ok(())
}

/// Returns whether one cancel signal is visible for the provided run.
fn cancel_requested(
    layout: &ProjectLayout,
    run_id: &RunId,
    current_state: &RunState,
) -> Result<bool> {
    if current_state.cancel_requested || layout.run_cancel_flag_path(run_id).exists() {
        return Ok(true);
    }
    let state = load_run_state(layout, run_id)?;
    Ok(state.cancel_requested || layout.run_cancel_flag_path(run_id).exists())
}

/// Repairs one in-flight run to `interrupted` when its heartbeat is stale.
fn repair_run_if_stale(layout: &ProjectLayout, run_id: &RunId) -> Result<()> {
    let state = match load_run_state(layout, run_id) {
        Ok(state) => state,
        Err(_) => return Ok(()),
    };
    if !is_run_state_stale(&state, SystemTime::now()) {
        return Ok(());
    }

    update_run_state(layout, run_id, |state| {
        let now = current_utc_timestamp();
        state.status = RunStatus::Interrupted;
        state.updated_at = now.clone();
        state.finished_at = Some(now.clone());
        state.heartbeat_at = Some(now.clone());
        state.headline = Some("Digest worker interrupted".to_owned());
        state.error_code = Some("worker_interrupted".to_owned());
        state.error_message = Some("digest worker heartbeat is stale".to_owned());
    })?;
    append_run_event(
        layout,
        run_id,
        RunEvent::warn(
            state.phase,
            "Recovered stale run as interrupted because the worker heartbeat is stale".to_owned(),
        ),
    )?;
    Ok(())
}

/// Returns whether one run status is already terminal.
fn is_finished_status(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Succeeded | RunStatus::Failed | RunStatus::Canceled | RunStatus::Interrupted
    )
}

/// Converts one run state into the external cancel response shape.
fn report_from_run_state(state: &RunState) -> DigestCancelReport {
    DigestCancelReport {
        project_id: state.project_id.clone(),
        run_id: state.run_id.clone(),
        status: state.status,
        phase: state.phase,
        cancel_requested: state.cancel_requested,
        pid: state.pid,
    }
}

/// Updates one run state by loading the latest persisted struct, applying the callback, and storing it.
fn update_run_state<F>(layout: &ProjectLayout, run_id: &RunId, mut update: F) -> Result<RunState>
where
    F: FnMut(&mut RunState),
{
    let mut state = load_run_state(layout, run_id)?;
    update(&mut state);
    store_run_state(layout, &state)?;
    Ok(state)
}

/// Finalizes one digest run as failed with durable headline and error metadata.
fn finalize_run_failed(
    layout: &ProjectLayout,
    run_id: &RunId,
    phase: RunPhase,
    headline: &str,
    error_code: &str,
    error_message: &str,
) -> Result<RunState> {
    update_run_state(layout, run_id, |state| {
        let now = current_utc_timestamp();
        state.status = RunStatus::Failed;
        state.phase = phase;
        state.updated_at = now.clone();
        state.finished_at = Some(now.clone());
        state.heartbeat_at = Some(now.clone());
        state.progress_percent = Some(100);
        state.headline = Some(headline.to_owned());
        state.error_code = Some(error_code.to_owned());
        state.error_message = Some(error_message.to_owned());
    })
}

/// Finalizes one digest run as canceled after cooperative worker shutdown.
fn finalize_run_canceled(
    layout: &ProjectLayout,
    run_id: &RunId,
    phase: RunPhase,
    headline: &str,
) -> Result<RunState> {
    update_run_state(layout, run_id, |state| {
        let now = current_utc_timestamp();
        state.status = RunStatus::Canceled;
        state.phase = phase;
        state.cancel_requested = true;
        state.updated_at = now.clone();
        state.finished_at = Some(now.clone());
        state.heartbeat_at = Some(now.clone());
        state.progress_percent = Some(100);
        state.headline = Some(headline.to_owned());
    })
}

/// Finalizes one digest run as succeeded after proposal validation completes.
fn finalize_run_succeeded(
    layout: &ProjectLayout,
    run_id: &RunId,
    phase: RunPhase,
    headline: &str,
) -> Result<RunState> {
    update_run_state(layout, run_id, |state| {
        let now = current_utc_timestamp();
        state.status = RunStatus::Succeeded;
        state.phase = phase;
        state.updated_at = now.clone();
        state.finished_at = Some(now.clone());
        state.heartbeat_at = Some(now.clone());
        state.progress_percent = Some(100);
        state.headline = Some(headline.to_owned());
        state.error_code = None;
        state.error_message = None;
    })
}

/// Waits until the parent process persists the worker registration fields after spawning the child.
fn wait_for_worker_registration(layout: &ProjectLayout, run_id: &RunId) -> Result<RunState> {
    let deadline = SystemTime::now() + WORKER_REGISTRATION_TIMEOUT;
    loop {
        let state = load_run_state(layout, run_id)?;
        if state.cancel_requested || is_finished_status(state.status) {
            return Ok(state);
        }
        if matches!(state.status, RunStatus::Running)
            && state.started_at.is_some()
            && state.pid.is_some()
        {
            return Ok(state);
        }
        if SystemTime::now() >= deadline {
            bail!("timed out waiting for worker registration fields for run `{run_id}`");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Loads one selected session plus its narrative turn details for the digest context bundle.
fn load_selected_session_context(
    root: &Path,
    project_id: &str,
    session_ref: &str,
    session_summaries: &[SessionSummary],
) -> Result<DigestContextSession> {
    let (provider, session_id) = parse_session_ref(session_ref)?;
    let session = session_summaries
        .iter()
        .find(|session| session.provider == provider && session.session_id == session_id)
        .cloned()
        .with_context(|| format!("selected session `{session_ref}` was not found in the index"))?;
    let turn_summaries = query_turns(Some(root.to_path_buf()), project_id, provider, session_id)
        .with_context(|| format!("failed to load indexed turns for `{session_ref}`"))?;
    let turns = turn_summaries
        .turns
        .into_iter()
        .map(|turn| {
            query_turn(
                Some(root.to_path_buf()),
                project_id,
                provider,
                session_id,
                turn.turn_ordinal,
                TurnDetailOptions {
                    include_raw: false,
                    include_insights: true,
                    narrative: true,
                },
            )
            .with_context(|| {
                format!(
                    "failed to load narrative turn {} for `{session_ref}`",
                    turn.turn_ordinal
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(DigestContextSession { session, turns })
}

/// Builds one runtime request from the persisted run state and prepared prompt.
fn build_runtime_request(
    root: &Path,
    project_id: &str,
    layout: &ProjectLayout,
    run_id: &RunId,
    state: &RunState,
    prompt: &str,
    schema_path: &Path,
) -> Result<RuntimeRequest> {
    let agent = AgentId::parse(state.agent_id.as_deref().unwrap_or_default())
        .context("run state is missing a supported agent id")?;
    let runtime = RuntimeKind::parse(state.runtime.as_deref().unwrap_or_default())
        .context("run state is missing a supported runtime kind")?;
    Ok(RuntimeRequest {
        agent,
        runtime,
        model: state.model.clone().unwrap_or_default(),
        auth_profile: state.auth_profile.clone(),
        prompt: prompt.to_owned(),
        workdir: resolve_runtime_workdir(root, project_id, layout, run_id)?,
        schema_path: schema_path.to_path_buf(),
        proposal_path: layout.run_proposal_path(run_id),
    })
}

/// Resolves the runtime working directory from project config with a safe run-dir fallback.
fn resolve_runtime_workdir(
    root: &Path,
    project_id: &str,
    layout: &ProjectLayout,
    run_id: &RunId,
) -> Result<PathBuf> {
    let project = resolve_registered_project(root, project_id)?;
    let candidate = project.local_path;
    if candidate.exists() {
        Ok(candidate)
    } else {
        Ok(layout.run_dir(run_id))
    }
}

/// Resolves one registered project config from the shared Darc root.
fn resolve_registered_project(root: &Path, project_id: &str) -> Result<ProjectConfig> {
    registered_projects(root)?
        .into_iter()
        .find(|project| project.id == project_id)
        .with_context(|| format!("project id `{project_id}` was not found in the shared config"))
}

/// Executes one prepared runtime command while streaming logs and preserving worker heartbeats.
fn execute_runtime_command(
    layout: &ProjectLayout,
    run_id: &RunId,
    command: darc_agent::RuntimeCommand,
) -> Result<RuntimeExecution> {
    let mut child = Command::new(&command.program)
        .args(&command.args)
        .current_dir(&command.workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {}", command.display_name))?;
    let stdout_handle = capture_process_stream(
        child
            .stdout
            .take()
            .context("runtime child stdout pipe was not captured")?,
        layout.run_stdout_log_path(run_id),
    );
    let stderr_handle = capture_process_stream(
        child
            .stderr
            .take()
            .context("runtime child stderr pipe was not captured")?,
        layout.run_stderr_log_path(run_id),
    );

    let mut last_heartbeat = SystemTime::now();
    let mut cancel_noted = false;
    let exit_status = loop {
        if let Some(status) = child.try_wait().context("failed to poll runtime child")? {
            break status;
        }

        if last_heartbeat.elapsed().unwrap_or_default() >= RUN_HEARTBEAT_INTERVAL {
            update_run_state(layout, run_id, |state| {
                let now = current_utc_timestamp();
                if state.cancel_requested {
                    state.status = RunStatus::CancelRequested;
                    state.headline = Some("Cancel requested; waiting for runtime exit".to_owned());
                }
                state.updated_at = now.clone();
                state.heartbeat_at = Some(now);
            })?;
            last_heartbeat = SystemTime::now();
        }

        let state = load_run_state(layout, run_id)?;
        if cancel_requested(layout, run_id, &state)? && !cancel_noted {
            append_run_event(
                layout,
                run_id,
                RunEvent::info(
                    RunPhase::WaitingForAgent,
                    "Cancel requested; waiting for runtime process to exit".to_owned(),
                ),
            )?;
            cancel_noted = true;
        }

        thread::sleep(RUN_POLL_INTERVAL);
    };

    let stdout = stdout_handle
        .join()
        .map_err(|_| anyhow::anyhow!("runtime stdout capture thread panicked"))??;
    let stderr = stderr_handle
        .join()
        .map_err(|_| anyhow::anyhow!("runtime stderr capture thread panicked"))??;
    let proposal_bytes = match &command.proposal_output {
        ProposalOutputSource::Stdout => Some(stdout.clone()),
        ProposalOutputSource::File(path) if path.exists() => {
            Some(fs::read(path).with_context(|| format!("failed to read {}", path.display()))?)
        }
        ProposalOutputSource::File(_) => None,
    };
    Ok(RuntimeExecution {
        display_name: command.display_name,
        proposal_source: command.proposal_output,
        exit_code: exit_status.code(),
        stdout,
        stderr,
        proposal_bytes,
    })
}

/// Captures one runtime output stream into both a durable log file and an in-memory buffer.
fn capture_process_stream<R>(mut reader: R, path: PathBuf) -> thread::JoinHandle<Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let mut collected = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            let read = reader
                .read(&mut buffer)
                .with_context(|| format!("failed to read {}", path.display()))?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read])
                .with_context(|| format!("failed to append {}", path.display()))?;
            collected.extend_from_slice(&buffer[..read]);
        }
        file.flush()
            .with_context(|| format!("failed to flush {}", path.display()))?;
        Ok(collected)
    })
}

/// Builds the prompt supplied to the external agent runtime for one digest run.
fn build_digest_runtime_prompt(context: &DigestContextArtifact) -> Result<String> {
    let context_json =
        serde_json::to_string_pretty(context).context("failed to serialize digest context JSON")?;
    Ok(format!(
        concat!(
            "You are generating a Context Wiki digest proposal for Darc.\n\n",
            "Return exactly one JSON object that matches the provided output schema.\n",
            "Do not return Markdown, prose, code fences, or commentary.\n\n",
            "Rules:\n",
            "- Set `schema` to `{schema}`.\n",
            "- Set `project_id` to `{project_id}`.\n",
            "- Set `run_id` to `{run_id}`.\n",
            "- The only allowed entry type is `decision_trace`.\n",
            "- The only allowed operation is `create`.\n",
            "- Use only categories from `registry.categories`.\n",
            "- Use only domains from `registry.domains` or `target_domains`.\n",
            "- Evidence references must use `<provider>:<session-id>#<turn-ordinal>` and only reference selected sessions.\n",
            "- It is valid to return zero entries when the context does not contain durable decisions.\n",
            "- Always include `run_summary`, even when `entries` is empty.\n",
            "- Set `run_summary.extracted_decision_count` to the number of entries you return.\n\n",
            "Context bundle:\n{context_json}\n"
        ),
        schema = DIGEST_PROPOSAL_SCHEMA,
        project_id = context.project_id,
        run_id = context.run_id,
        context_json = context_json,
    ))
}

/// Builds the allowed proposal domain list from persisted registry and run-target hints.
fn build_allowed_domains(registry: &ProjectRegistry, state: &RunState) -> Vec<String> {
    let mut domains = registry.domains.clone();
    for domain in &state.target_domains {
        if !domains.contains(domain) {
            domains.push(domain.clone());
        }
    }
    domains
}

/// Writes one digest result artifact into the durable run directory.
fn write_digest_result(
    layout: &ProjectLayout,
    run_id: &RunId,
    artifact: &DigestResultArtifact,
) -> Result<()> {
    write_json_artifact(&layout.run_result_path(run_id), artifact)
}

/// Builds one digest result artifact from runtime and validation outcomes.
fn build_result_artifact(
    state: &RunState,
    completed_at: String,
    runtime: Option<&RuntimeExecution>,
    validation: DigestValidationArtifact,
    error_code: Option<String>,
    error_message: Option<String>,
    note: Option<String>,
) -> DigestResultArtifact {
    DigestResultArtifact {
        schema: RUN_RESULT_SCHEMA.to_owned(),
        project_id: state.project_id.clone(),
        run_id: state.run_id.to_string(),
        status: state.status,
        completed_at,
        error_code,
        error_message,
        runtime: DigestRuntimeArtifact {
            agent_id: state.agent_id.clone(),
            runtime: state.runtime.clone(),
            model: state.model.clone(),
            auth_profile: state.auth_profile.clone(),
            display_name: runtime.map(|runtime| runtime.display_name.clone()),
            exit_code: runtime.and_then(|runtime| runtime.exit_code),
            stdout_bytes: runtime.map_or(0, |runtime| runtime.stdout.len()),
            stderr_bytes: runtime.map_or(0, |runtime| runtime.stderr.len()),
            proposal_source: runtime.map(runtime_proposal_source_name),
            proposal_captured: runtime.is_some_and(|runtime| runtime.proposal_bytes.is_some()),
        },
        validation,
        note,
    }
}

/// Returns the durable proposal-source label for one runtime execution.
fn runtime_proposal_source_name(runtime: &RuntimeExecution) -> String {
    match runtime.proposal_source {
        ProposalOutputSource::Stdout => "stdout".to_owned(),
        ProposalOutputSource::File(_) => "file".to_owned(),
    }
}

/// Returns whether one in-flight run heartbeat is stale enough to treat as interrupted.
fn is_run_state_stale(state: &RunState, now: SystemTime) -> bool {
    if !matches!(
        state.status,
        RunStatus::Running | RunStatus::CancelRequested
    ) {
        return false;
    }

    let timestamp = state
        .heartbeat_at
        .as_deref()
        .or(state.started_at.as_deref())
        .or(Some(state.updated_at.as_str()));
    let Some(timestamp) = timestamp else {
        return false;
    };
    parse_utc_timestamp(timestamp)
        .and_then(|ts| now.duration_since(ts).ok())
        .map(|elapsed| elapsed >= RUN_STALE_TIMEOUT)
        .unwrap_or(false)
}

/// Returns one query-visible run summary with stale in-flight runs normalized to `interrupted`.
pub(crate) fn visible_run_summary(summary: &darc_wiki::RunSummary) -> darc_wiki::RunSummary {
    if is_run_summary_stale(summary, SystemTime::now()) {
        let mut normalized = summary.clone();
        normalized.status = RunStatus::Interrupted;
        normalized
    } else {
        summary.clone()
    }
}

/// Loads one project's run summaries with stale in-flight runs normalized for read-side display.
pub(crate) fn load_visible_run_summaries(
    layout: &ProjectLayout,
) -> Result<Vec<darc_wiki::RunSummary>> {
    Ok(list_runs(layout)?
        .into_iter()
        .map(|summary| visible_run_summary(&summary))
        .collect())
}

/// Returns whether one run summary should be displayed as stale/interrupted.
fn is_run_summary_stale(summary: &darc_wiki::RunSummary, now: SystemTime) -> bool {
    if !matches!(
        summary.status,
        RunStatus::Running | RunStatus::CancelRequested
    ) {
        return false;
    }
    let timestamp = summary
        .heartbeat_at
        .as_deref()
        .or(Some(summary.updated_at.as_str()));
    let Some(timestamp) = timestamp else {
        return false;
    };
    parse_utc_timestamp(timestamp)
        .and_then(|ts| now.duration_since(ts).ok())
        .map(|elapsed| elapsed >= RUN_STALE_TIMEOUT)
        .unwrap_or(false)
}

/// Stores one persisted digest start request artifact.
#[derive(Debug, Serialize)]
struct DigestRequestArtifact {
    schema: String,
    project_id: String,
    run_id: String,
    selected_sessions: Vec<String>,
    target_categories: Vec<String>,
    target_domains: Vec<String>,
    agent_id: String,
    runtime: String,
    model: String,
    auth_profile: Option<String>,
    requested_by: String,
    request_source: String,
    created_at: String,
}

/// Stores one persisted digest context artifact.
#[derive(Debug, Serialize)]
struct DigestContextArtifact {
    schema: String,
    project_id: String,
    run_id: String,
    selected_sessions: Vec<String>,
    target_categories: Vec<String>,
    target_domains: Vec<String>,
    registry: ProjectRegistry,
    sessions: Vec<DigestContextSession>,
    generated_at: String,
}

/// Stores one selected session plus narrative turn details in the digest context artifact.
#[derive(Debug, Clone, Serialize)]
struct DigestContextSession {
    session: SessionSummary,
    turns: Vec<TurnDetail>,
}

/// Stores the runtime execution metadata captured for one digest run.
#[derive(Debug, Clone)]
struct RuntimeExecution {
    display_name: String,
    proposal_source: ProposalOutputSource,
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    proposal_bytes: Option<Vec<u8>>,
}

/// Stores one durable result artifact for runtime and validation reporting.
#[derive(Debug, Clone, Serialize)]
struct DigestResultArtifact {
    schema: String,
    project_id: String,
    run_id: String,
    status: RunStatus,
    completed_at: String,
    error_code: Option<String>,
    error_message: Option<String>,
    runtime: DigestRuntimeArtifact,
    validation: DigestValidationArtifact,
    note: Option<String>,
}

/// Stores the runtime execution details embedded in `result.json`.
#[derive(Debug, Clone, Serialize)]
struct DigestRuntimeArtifact {
    agent_id: Option<String>,
    runtime: Option<String>,
    model: Option<String>,
    auth_profile: Option<String>,
    display_name: Option<String>,
    exit_code: Option<i32>,
    stdout_bytes: usize,
    stderr_bytes: usize,
    proposal_source: Option<String>,
    proposal_captured: bool,
}

/// Stores the proposal validation summary embedded in `result.json`.
#[derive(Debug, Clone, Default, Serialize)]
struct DigestValidationArtifact {
    attempted: bool,
    valid: bool,
    entry_count: Option<usize>,
    run_summary_title: Option<String>,
    extracted_decision_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    errors: Vec<ProposalValidationError>,
}

/// Stores one JSONL progress event for one digest run.
#[derive(Debug, Serialize)]
struct RunEvent {
    ts: String,
    level: String,
    phase: RunPhase,
    message: String,
}

impl RunEvent {
    /// Builds one informational run event.
    fn info(phase: RunPhase, message: String) -> Self {
        Self {
            ts: current_utc_timestamp(),
            level: RUN_EVENT_LEVEL_INFO.to_owned(),
            phase,
            message,
        }
    }

    /// Builds one warning run event.
    fn warn(phase: RunPhase, message: String) -> Self {
        Self {
            ts: current_utc_timestamp(),
            level: RUN_EVENT_LEVEL_WARN.to_owned(),
            phase,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, path::PathBuf};

    use anyhow::Result;
    use darc_test_utils::unique_test_dir;

    use super::*;
    use crate::{
        config::{ProjectConfig, SharedConfig, SourcesConfig},
        constants::CONFIG_FILE_NAME,
    };

    /// Writes one minimal shared config fixture for wiki backend tests.
    fn write_config(root: &Path, config: &SharedConfig) -> Result<()> {
        fs::create_dir_all(root)?;
        fs::write(root.join(CONFIG_FILE_NAME), toml::to_string_pretty(config)?)?;
        Ok(())
    }

    /// Builds one minimal configured project fixture for wiki backend tests.
    fn build_project(root: &Path, project_id: &str, project_root: PathBuf) -> ProjectConfig {
        ProjectConfig {
            id: project_id.to_owned(),
            name: "repo".to_owned(),
            local_path: project_root,
            git_upstream: None,
            sessions_root: root.join(format!("projects/{project_id}/sessions")),
            known_paths: Vec::new(),
        }
    }

    #[test]
    fn backend_creates_empty_project_wiki_and_lists_zero_state() -> Result<()> {
        let root = unique_test_dir("core-wiki-empty");
        let project_root = root.join("repo");
        let project_id = "repo-123";
        fs::create_dir_all(&project_root)?;
        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![build_project(&root, project_id, project_root)],
                SourcesConfig::default(),
            ),
        )?;

        let layout = ensure_project_wiki(Some(root.clone()), project_id)?;
        assert_eq!(
            layout.root,
            root.join("context-wiki").join("projects").join(project_id)
        );
        assert!(root.join("context-wiki/VERSION").exists());
        assert!(!root.join("context-wiki/context-wiki").exists());
        assert!(layout.registry_dir.exists());
        assert!(layout.categories_path.exists());
        assert!(layout.domains_path.exists());
        assert!(layout.entries_dir.exists());
        assert!(layout.digests_dir.exists());
        assert!(layout.runs_dir.exists());

        let wiki = load_project_wiki(Some(root.clone()), project_id)?;
        assert_eq!(wiki.project_id, project_id);
        assert_eq!(wiki.registry.categories, crate::DEFAULT_CATEGORY_IDS);
        assert!(wiki.registry.domains.is_empty());
        assert!(wiki.entries.is_empty());
        assert!(wiki.digests.is_empty());
        assert!(wiki.runs.is_empty());

        fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn backend_round_trips_run_state_through_core_wiring() -> Result<()> {
        let root = unique_test_dir("core-wiki-run");
        let project_root = root.join("repo");
        let project_id = "repo-123";
        fs::create_dir_all(&project_root)?;
        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![build_project(&root, project_id, project_root)],
                SourcesConfig::default(),
            ),
        )?;

        let run_state = crate::RunState {
            schema_version: 1,
            run_id: crate::RunId::new("cwrun_01backend")?,
            project_id: project_id.to_owned(),
            status: crate::RunStatus::Queued,
            phase: crate::RunPhase::PreparingContext,
            created_at: "2026-04-13T10:00:00Z".to_owned(),
            started_at: None,
            updated_at: "2026-04-13T10:00:00Z".to_owned(),
            finished_at: None,
            heartbeat_at: None,
            requested_by: Some("desktop".to_owned()),
            request_source: Some("darc-desktop/0.1.0".to_owned()),
            attempt: 1,
            cancel_requested: false,
            pid: None,
            agent_id: None,
            runtime: None,
            model: None,
            auth_profile: None,
            selected_sessions: Vec::new(),
            target_categories: vec!["architecture".to_owned()],
            target_domains: Vec::new(),
            progress_percent: None,
            headline: Some("Queued".to_owned()),
            proposal_path: None,
            result_path: None,
            events_path: None,
            stdout_log_path: None,
            stderr_log_path: None,
            created_entry_ids: Vec::new(),
            updated_entry_ids: Vec::new(),
            digest_id: None,
            error_code: None,
            error_message: None,
        };

        store_project_wiki_run(Some(root.clone()), project_id, &run_state)?;

        let loaded = load_project_wiki_run(Some(root.clone()), project_id, &run_state.run_id)?;
        assert_eq!(loaded, run_state);

        let wiki = load_project_wiki(Some(root.clone()), project_id)?;
        assert_eq!(wiki.runs.len(), 1);
        assert_eq!(wiki.runs[0].run_id, run_state.run_id);
        assert_eq!(wiki.runs[0].headline.as_deref(), Some("Queued"));

        fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn stale_running_run_is_repaired_to_interrupted() -> Result<()> {
        let root = unique_test_dir("core-wiki-stale");
        let project_root = root.join("repo");
        let project_id = "repo-123";
        fs::create_dir_all(&project_root)?;
        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![build_project(&root, project_id, project_root)],
                SourcesConfig::default(),
            ),
        )?;

        let layout = ensure_project_wiki(Some(root.clone()), project_id)?;
        let run_id = RunId::new("cwrun_01stale")?;
        let state = RunState {
            schema_version: 1,
            run_id: run_id.clone(),
            project_id: project_id.to_owned(),
            status: RunStatus::Running,
            phase: RunPhase::WaitingForAgent,
            created_at: "2026-04-13T10:00:00Z".to_owned(),
            started_at: Some("2026-04-13T10:00:01Z".to_owned()),
            updated_at: "2026-04-13T10:00:02Z".to_owned(),
            finished_at: None,
            heartbeat_at: Some("2026-04-13T10:00:02Z".to_owned()),
            requested_by: Some("desktop".to_owned()),
            request_source: Some("darc-desktop/0.1.0".to_owned()),
            attempt: 1,
            cancel_requested: false,
            pid: Some(999_999),
            agent_id: Some("codex".to_owned()),
            runtime: Some("external_cli".to_owned()),
            model: Some("gpt-5.4".to_owned()),
            auth_profile: Some("openai/default".to_owned()),
            selected_sessions: vec!["codex:session-1".to_owned()],
            target_categories: Vec::new(),
            target_domains: Vec::new(),
            progress_percent: Some(20),
            headline: Some("Waiting".to_owned()),
            proposal_path: Some("proposal.json".to_owned()),
            result_path: Some("result.json".to_owned()),
            events_path: Some("events.jsonl".to_owned()),
            stdout_log_path: Some("agent.stdout.log".to_owned()),
            stderr_log_path: Some("agent.stderr.log".to_owned()),
            created_entry_ids: Vec::new(),
            updated_entry_ids: Vec::new(),
            digest_id: None,
            error_code: None,
            error_message: None,
        };
        store_run_state(&layout, &state)?;

        let repaired = load_project_wiki_run(Some(root.clone()), project_id, &run_id)?;
        assert_eq!(repaired.status, RunStatus::Interrupted);
        assert_eq!(repaired.error_code.as_deref(), Some("worker_interrupted"));

        fs::remove_dir_all(&root)?;
        Ok(())
    }
}
