use std::{
    fs::OpenOptions,
    io,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use darc_paths::{current_utc_timestamp, parse_utc_timestamp};
use darc_wiki::{
    DigestId, EntryId, ProjectLayout, RunId, RunPhase, RunState, RunStatus, list_runs,
    load_run_state, store_run_state,
};

use super::{
    DigestCancelReport, RUN_STALE_TIMEOUT, WORKER_REGISTRATION_POLL_INTERVAL,
    WORKER_REGISTRATION_TIMEOUT, artifacts::append_run_event, models::RunEvent,
};

static RUN_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
const PROCESS_NOT_FOUND_ERRNO: i32 = 3;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// Generates the next unused run id for one project layout.
pub(super) fn next_run_id(layout: &ProjectLayout) -> Result<RunId> {
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

/// Applies the next worker-visible phase/progress transition to one run state.
pub(super) fn transition_worker_state(
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

/// Refreshes one active worker heartbeat without changing the visible phase or progress fields.
pub(super) fn refresh_worker_heartbeat(layout: &ProjectLayout, run_id: &RunId) -> Result<()> {
    update_run_state(layout, run_id, |state| {
        if is_finished_status(state.status) {
            return;
        }
        let now = current_utc_timestamp();
        if state.cancel_requested {
            state.status = RunStatus::CancelRequested;
        }
        state.updated_at = now.clone();
        state.heartbeat_at = Some(now);
    })?;
    Ok(())
}

/// Returns whether one cancel signal is visible for the provided run.
pub(super) fn cancel_requested(
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

/// Repairs one in-flight run to `interrupted` when its heartbeat is stale and its worker is gone.
pub(super) fn repair_run_if_stale(layout: &ProjectLayout, run_id: &RunId) -> Result<()> {
    if !layout.run_state_path(run_id).exists() {
        return Ok(());
    }

    let now = SystemTime::now();
    let mut repaired_phase = None;
    update_run_state(layout, run_id, |state| {
        if !should_repair_stale_run(state, now) {
            return;
        }
        repaired_phase = Some(state.phase);
        let now = current_utc_timestamp();
        state.status = RunStatus::Interrupted;
        state.updated_at = now.clone();
        state.finished_at = Some(now.clone());
        state.heartbeat_at = Some(now.clone());
        state.headline = Some("Digest worker interrupted".to_owned());
        state.error_code = Some("worker_interrupted".to_owned());
        state.error_message =
            Some("digest worker heartbeat is stale and worker pid is no longer live".to_owned());
    })?;
    if let Some(phase) = repaired_phase {
        append_run_event(
            layout,
            run_id,
            RunEvent::warn(
                phase,
                "Recovered stale run as interrupted because the worker heartbeat is stale and the worker pid is no longer live".to_owned(),
            ),
        )?;
    }
    Ok(())
}

/// Returns whether one run status is already terminal.
pub(super) fn is_finished_status(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Succeeded | RunStatus::Failed | RunStatus::Canceled | RunStatus::Interrupted
    )
}

/// Converts one run state into the external cancel response shape.
pub(super) fn report_from_run_state(state: &RunState) -> DigestCancelReport {
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
pub(super) fn update_run_state<F>(
    layout: &ProjectLayout,
    run_id: &RunId,
    mut update: F,
) -> Result<RunState>
where
    F: FnMut(&mut RunState),
{
    let _lock = lock_run_state(layout, run_id)?;
    let mut state = load_run_state(layout, run_id)?;
    update(&mut state);
    store_run_state(layout, &state)?;
    Ok(state)
}

/// Runs one locked read-modify-write cycle that may perform side effects before storing state.
pub(super) fn with_locked_run_state<F, T>(
    layout: &ProjectLayout,
    run_id: &RunId,
    mut operation: F,
) -> Result<T>
where
    F: FnMut(&mut RunState) -> Result<T>,
{
    let _lock = lock_run_state(layout, run_id)?;
    let mut state = load_run_state(layout, run_id)?;
    let output = operation(&mut state)?;
    store_run_state(layout, &state)?;
    Ok(output)
}

/// Locks one run-state mutation path so concurrent writers cannot interleave updates.
fn lock_run_state(layout: &ProjectLayout, run_id: &RunId) -> Result<std::fs::File> {
    let path = layout.run_state_lock_path(run_id);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("failed to lock {}", path.display()))?;
    Ok(file)
}

/// Finalizes one digest run as failed with durable headline and error metadata.
pub(super) fn finalize_run_failed(
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
pub(super) fn finalize_run_canceled(
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

/// Builds one succeeded digest run state after canonical artifact writing completes.
pub(super) fn build_succeeded_run_state(
    mut state: RunState,
    phase: RunPhase,
    headline: &str,
    created_entry_ids: &[EntryId],
    updated_entry_ids: &[EntryId],
    digest_id: &DigestId,
) -> RunState {
    let now = current_utc_timestamp();
    state.status = RunStatus::Succeeded;
    state.phase = phase;
    state.updated_at = now.clone();
    state.finished_at = Some(now.clone());
    state.heartbeat_at = Some(now);
    state.progress_percent = Some(100);
    state.headline = Some(headline.to_owned());
    state.created_entry_ids = created_entry_ids.iter().map(ToString::to_string).collect();
    state.updated_entry_ids = updated_entry_ids.iter().map(ToString::to_string).collect();
    state.digest_id = Some(digest_id.to_string());
    state.error_code = None;
    state.error_message = None;
    state
}

/// Waits until the parent process persists the worker registration fields after spawning the child.
pub(super) fn wait_for_worker_registration(
    layout: &ProjectLayout,
    run_id: &RunId,
) -> Result<RunState> {
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
        thread::sleep(WORKER_REGISTRATION_POLL_INTERVAL);
    }
}

/// Loads one project's run summaries after durably repairing stale dead runs.
pub(crate) fn load_visible_run_summaries(
    layout: &ProjectLayout,
) -> Result<Vec<darc_wiki::RunSummary>> {
    list_runs(layout)?
        .into_iter()
        .map(|summary| {
            repair_run_if_stale(layout, &summary.run_id)?;
            let state = load_run_state(layout, &summary.run_id)?;
            Ok(darc_wiki::RunSummary {
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
                run_dir: summary.run_dir,
                run_state_path: summary.run_state_path,
            })
        })
        .collect()
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

/// Returns whether one stale run should be durably repaired based on worker liveness.
fn should_repair_stale_run(state: &RunState, now: SystemTime) -> bool {
    is_run_state_stale(state, now) && !is_process_alive(state.pid)
}

/// Returns whether one worker pid is still live enough to avoid stale-run repair.
fn is_process_alive(pid: Option<u32>) -> bool {
    let Some(pid) = pid else {
        return false;
    };
    process_exists(pid)
}

/// Returns whether one concrete process identifier currently exists.
#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: `kill(pid, 0)` performs an existence check without sending a signal.
    let result = unsafe { kill(pid as i32, 0) };
    if result == 0 {
        return true;
    }
    !matches!(
        io::Error::last_os_error().raw_os_error(),
        Some(PROCESS_NOT_FOUND_ERRNO)
    )
}

/// Returns whether one concrete process identifier currently exists.
#[cfg(not(unix))]
fn process_exists(pid: u32) -> bool {
    let _ = pid;
    false
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::mpsc, thread, time::Duration};

    use darc_test_utils::unique_test_dir;
    use darc_wiki::ContextWikiLayout;

    use super::*;

    /// Verifies that the locked success path cannot be overwritten by a later no-op updater.
    #[test]
    fn locked_run_state_keeps_succeeded_state_visible() -> Result<()> {
        let root = unique_test_dir("wiki-state-lock");
        let layout = ContextWikiLayout::new(&root).project_layout("repo-123")?;
        layout.ensure()?;

        let run_id = RunId::new("cwrun_01locktest")?;
        store_run_state(
            &layout,
            &RunState {
                schema_version: 1,
                run_id: run_id.clone(),
                project_id: "repo-123".to_owned(),
                status: RunStatus::Running,
                phase: RunPhase::WritingArtifacts,
                created_at: "2026-04-13T10:00:00Z".to_owned(),
                started_at: Some("2026-04-13T10:00:01Z".to_owned()),
                updated_at: "2026-04-13T10:00:02Z".to_owned(),
                finished_at: None,
                heartbeat_at: Some("2026-04-13T10:00:02Z".to_owned()),
                requested_by: Some("desktop".to_owned()),
                request_source: Some("darc-desktop/0.1.0".to_owned()),
                attempt: 1,
                cancel_requested: false,
                pid: Some(42),
                agent_id: Some("codex".to_owned()),
                runtime: Some("external_cli".to_owned()),
                model: Some("gpt-5.4".to_owned()),
                auth_profile: None,
                selected_sessions: vec!["codex:session-1".to_owned()],
                target_categories: vec!["product".to_owned()],
                target_domains: vec!["query-protocol".to_owned()],
                progress_percent: Some(100),
                headline: Some("Writing final result artifacts".to_owned()),
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
            },
        )?;

        let (started_tx, started_rx) = mpsc::channel();
        let layout_clone = layout.clone();
        let run_id_clone = run_id.clone();
        let worker = thread::spawn(move || -> Result<()> {
            with_locked_run_state(&layout_clone, &run_id_clone, |state| {
                *state = build_succeeded_run_state(
                    state.clone(),
                    RunPhase::WritingArtifacts,
                    "Wrote canonical wiki artifacts",
                    &[],
                    &[],
                    &DigestId::new("dg_01locktest")?,
                );
                started_tx
                    .send(())
                    .expect("test should observe the locked success path");
                thread::sleep(Duration::from_millis(150));
                Ok(())
            })
        });

        started_rx
            .recv()
            .expect("test should wait until the success lock is held");
        let observed = update_run_state(&layout, &run_id, |_| {})?;
        worker
            .join()
            .expect("worker thread should not panic during test")?;

        assert_eq!(observed.status, RunStatus::Succeeded);
        assert_eq!(observed.digest_id.as_deref(), Some("dg_01locktest"));

        let final_state = load_run_state(&layout, &run_id)?;
        assert_eq!(final_state.status, RunStatus::Succeeded);
        assert_eq!(final_state.digest_id.as_deref(), Some("dg_01locktest"));

        fs::remove_dir_all(&root)?;
        Ok(())
    }
}
