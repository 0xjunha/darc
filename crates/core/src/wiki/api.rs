use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use darc_paths::current_utc_timestamp;
use darc_wiki::{
    ContextWikiLayout, EntryId, EntryStatusChange, ProjectLayout, RunId, RunPhase, RunState,
    RunStatus, discard_entry, ensure_registry, list_digests, list_entries, load_registry,
    load_run_state, restore_entry, store_run_state,
};

use super::{
    DEFAULT_REQUESTED_BY, DigestCancelReport, DigestStartOptions, DigestStartReport,
    EntryMutationReport, PreparedDigestRun, ProjectWikiData, RUN_CONTEXT_SCHEMA,
    RUN_REQUEST_SCHEMA,
    artifacts::{
        append_run_event, relative_artifact_name, touch_file, write_json_artifact,
        write_terminal_result,
    },
    context::{validate_digest_start_options, validate_digest_targets},
    models::{DigestContextArtifact, DigestRequestArtifact, DigestValidationArtifact, RunEvent},
    state::{
        finalize_run_failed, is_finished_status, load_visible_run_summaries, next_run_id,
        repair_run_if_stale, report_from_run_state, update_run_state,
    },
    worker,
};
use crate::{default_root_path, project::registered_projects};

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
    load_project_wiki_run_from_layout(&layout, run_id)
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
        proposal_schema_path: Some(
            layout
                .digest_proposal_schema_relative_path()
                .to_string_lossy()
                .into_owned(),
        ),
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
    let started_at = current_utc_timestamp();
    let state = update_run_state(&layout, run_id, |state| {
        state.status = RunStatus::Running;
        state.started_at = Some(started_at.clone());
        state.updated_at = started_at.clone();
        state.heartbeat_at = Some(started_at.clone());
        state.pid = Some(pid);
        state.headline = Some("Preparing digest context".to_owned());
    })?;
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
    let final_state = finalize_run_failed(
        &layout,
        run_id,
        RunPhase::PreparingContext,
        "Failed to spawn digest worker",
        "worker_spawn_failed",
        error_message,
    )?;
    write_terminal_result(
        &layout,
        run_id,
        &final_state,
        None,
        DigestValidationArtifact::default(),
        None,
    )?;
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
    let now = current_utc_timestamp();
    let state = update_run_state(&layout, run_id, |state| {
        if is_finished_status(state.status) {
            return;
        }
        state.cancel_requested = true;
        if matches!(state.status, RunStatus::Queued | RunStatus::Running) {
            state.status = RunStatus::CancelRequested;
        }
        state.updated_at = now.clone();
        state.heartbeat_at = Some(now.clone());
        state.headline = Some("Cancel requested".to_owned());
    })?;
    if is_finished_status(state.status) {
        return Ok(report_from_run_state(&state));
    }

    touch_file(&layout.run_cancel_flag_path(run_id))?;
    append_run_event(
        &layout,
        run_id,
        RunEvent::info(RunPhase::WaitingForAgent, "Cancel requested".to_owned()),
    )?;

    Ok(report_from_run_state(&state))
}

/// Discards one canonical wiki entry for the requested configured project.
pub fn discard_project_wiki_entry(
    root: Option<PathBuf>,
    project_id: &str,
    entry_id: &EntryId,
) -> Result<EntryMutationReport> {
    let layout = ensure_project_wiki(root, project_id)?;
    let change = discard_entry(&layout, entry_id).context("failed to discard wiki entry")?;
    Ok(entry_mutation_report(project_id, change))
}

/// Restores one discarded canonical wiki entry for the requested configured project.
pub fn restore_project_wiki_entry(
    root: Option<PathBuf>,
    project_id: &str,
    entry_id: &EntryId,
) -> Result<EntryMutationReport> {
    let layout = ensure_project_wiki(root, project_id)?;
    let change = restore_entry(&layout, entry_id).context("failed to restore wiki entry")?;
    Ok(entry_mutation_report(project_id, change))
}

/// Runs the hidden digest worker loop for one existing run.
pub fn run_project_wiki_digest_worker(
    root: Option<PathBuf>,
    project_id: &str,
    run_id: &RunId,
) -> Result<()> {
    worker::run_project_wiki_digest_worker(root, project_id, run_id)
}

/// Loads one durable wiki run state from one already-resolved project wiki layout.
pub(crate) fn load_project_wiki_run_from_layout(
    layout: &ProjectLayout,
    run_id: &RunId,
) -> Result<RunState> {
    repair_run_if_stale(layout, run_id).context("failed to repair stale wiki run")?;
    load_run_state(layout, run_id).context("failed to load wiki run state")
}

/// Resolves one validated project wiki layout from the configured Darc root.
pub(super) fn resolve_project_layout(
    root: Option<PathBuf>,
    project_id: &str,
) -> Result<ProjectLayout> {
    let (layout, _) = resolve_project_layout_and_root(root, project_id)?;
    Ok(layout)
}

/// Resolves one validated project wiki layout plus configured project root path.
pub(super) fn resolve_project_layout_and_root(
    root: Option<PathBuf>,
    project_id: &str,
) -> Result<(ProjectLayout, PathBuf)> {
    let root = root.unwrap_or_else(default_root_path);
    let project = registered_projects(&root)?
        .into_iter()
        .find(|project| project.id == project_id)
        .with_context(|| format!("project id `{project_id}` was not found in the shared config"))?;
    let layout = ContextWikiLayout::new(root)
        .project_layout(project.id)
        .context("failed to resolve project wiki layout")?;
    Ok((layout, project.local_path))
}

/// Converts one leaf wiki entry status change into the external CLI response shape.
fn entry_mutation_report(project_id: &str, change: EntryStatusChange) -> EntryMutationReport {
    EntryMutationReport {
        project_id: project_id.to_owned(),
        entry_id: change.entry_id,
        previous_status: change.previous_status,
        status: change.status,
        updated_at: change.updated_at,
        changed: change.changed,
    }
}
