use std::{
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, bail};
use darc_paths::{current_utc_timestamp, parse_utc_timestamp};
use darc_wiki::{
    ProjectLayout, RunId, RunPhase, RunState, RunStatus, list_runs, load_run_state, store_run_state,
};

use super::{
    DigestCancelReport, RUN_STALE_TIMEOUT, WORKER_REGISTRATION_POLL_INTERVAL,
    WORKER_REGISTRATION_TIMEOUT, artifacts::append_run_event, models::RunEvent,
};

static RUN_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

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

/// Repairs one in-flight run to `interrupted` when its heartbeat is stale.
pub(super) fn repair_run_if_stale(layout: &ProjectLayout, run_id: &RunId) -> Result<()> {
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
    let mut state = load_run_state(layout, run_id)?;
    update(&mut state);
    store_run_state(layout, &state)?;
    Ok(state)
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

/// Finalizes one digest run as succeeded after proposal validation completes.
pub(super) fn finalize_run_succeeded(
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
pub(super) fn load_visible_run_summaries(
    layout: &ProjectLayout,
) -> Result<Vec<darc_wiki::RunSummary>> {
    Ok(list_runs(layout)?
        .into_iter()
        .map(|summary| visible_run_summary(&summary))
        .collect())
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
