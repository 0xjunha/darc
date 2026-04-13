use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use darc_wiki::{
    ContextWikiLayout, ProjectLayout, ProjectRegistry, RunId, RunPhase, RunState, RunStatus,
    ensure_registry, list_digests, list_entries, list_runs, load_registry, load_run_state,
    store_run_state,
};
use serde::Serialize;

use crate::{default_root_path, project::registered_projects};

static RUN_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

const RUN_REQUEST_SCHEMA: &str = "darc.wiki.digest.request.v1";
const RUN_CONTEXT_SCHEMA: &str = "darc.wiki.digest.context.v1";
const RUN_EVENT_LEVEL_INFO: &str = "info";
const RUN_EVENT_LEVEL_WARN: &str = "warn";
const DEFAULT_REQUESTED_BY: &str = "cli";
const RUN_POLL_INTERVAL: Duration = Duration::from_millis(200);
const RUN_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const CANCEL_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

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
    pub worker_executable: PathBuf,
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
    repair_stale_runs(&layout).context("failed to repair stale wiki runs")?;
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
        runs: list_runs(&layout).context("failed to list wiki runs")?,
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

/// Starts one new digest run, writes the initial artifacts, and spawns the hidden worker.
pub fn start_project_wiki_digest(
    root: Option<PathBuf>,
    project_id: &str,
    options: &DigestStartOptions,
) -> Result<DigestStartReport> {
    validate_digest_start_options(options)?;
    let layout = ensure_project_wiki(root, project_id)?;
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
        generated_at: now.clone(),
    };
    write_json_artifact(&layout.run_request_path(&run_id), &request)?;
    write_json_artifact(&layout.run_context_path(&run_id), &context)?;
    touch_file(&layout.run_events_path(&run_id))?;
    touch_file(&layout.run_stdout_log_path(&run_id))?;
    touch_file(&layout.run_stderr_log_path(&run_id))?;

    let mut state = RunState {
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

    let stdout = OpenOptions::new()
        .append(true)
        .open(layout.run_stdout_log_path(&run_id))
        .context("failed to open worker stdout log")?;
    let stderr = OpenOptions::new()
        .append(true)
        .open(layout.run_stderr_log_path(&run_id))
        .context("failed to open worker stderr log")?;
    let child = Command::new(&options.worker_executable)
        .args([
            "wiki",
            "digest",
            "worker",
            "--root",
            layout.context().darc_root.to_string_lossy().as_ref(),
            "--project-id",
            project_id,
            "--run-id",
            run_id.as_str(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn();
    let pid = match child {
        Ok(child) => child.id(),
        Err(error) => {
            let failed_at = current_utc_timestamp();
            state.status = RunStatus::Failed;
            state.updated_at = failed_at.clone();
            state.finished_at = Some(failed_at.clone());
            state.heartbeat_at = Some(failed_at);
            state.headline = Some("Failed to spawn digest worker".to_owned());
            state.error_code = Some("worker_spawn_failed".to_owned());
            state.error_message = Some(error.to_string());
            store_run_state(&layout, &state)?;
            append_run_event(
                &layout,
                &run_id,
                RunEvent::warn(
                    RunPhase::PreparingContext,
                    "Failed to spawn digest worker".to_owned(),
                ),
            )?;
            return Err(error).context("failed to spawn wiki digest worker");
        }
    };

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
        &run_id,
        RunEvent::info(
            RunPhase::PreparingContext,
            format!("Started digest worker process with pid {pid}"),
        ),
    )?;

    Ok(DigestStartReport {
        project_id: project_id.to_owned(),
        run_id,
        status: state.status,
        phase: state.phase,
        pid,
    })
}

/// Cancels one existing digest run and finalizes durable state when the worker exits.
pub fn cancel_project_wiki_digest(
    root: Option<PathBuf>,
    project_id: &str,
    run_id: &RunId,
) -> Result<DigestCancelReport> {
    let layout = ensure_project_wiki(root, project_id)?;
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

    if let Some(pid) = state.pid {
        if process_exists(pid) {
            wait_for_worker_exit(pid, CANCEL_WAIT_TIMEOUT);
        }
        if process_exists(pid) {
            terminate_process(pid)
                .with_context(|| format!("failed to terminate worker pid {pid}"))?;
            wait_for_worker_exit(pid, CANCEL_WAIT_TIMEOUT);
        }
    }

    state = load_run_state(&layout, run_id)?;
    if matches!(
        state.status,
        RunStatus::Running | RunStatus::Queued | RunStatus::CancelRequested
    ) && state.pid.map(process_exists).unwrap_or(false)
    {
        bail!(
            "worker process {} did not exit after cancellation",
            state.pid.unwrap_or_default()
        );
    }
    if matches!(
        state.status,
        RunStatus::Running | RunStatus::Queued | RunStatus::CancelRequested
    ) {
        let now = current_utc_timestamp();
        state.status = RunStatus::Canceled;
        state.cancel_requested = true;
        state.updated_at = now.clone();
        state.finished_at = Some(now.clone());
        state.heartbeat_at = Some(now);
        state.headline = Some("Digest run canceled".to_owned());
        store_run_state(&layout, &state)?;
        append_run_event(
            &layout,
            run_id,
            RunEvent::info(RunPhase::WaitingForAgent, "Digest run canceled".to_owned()),
        )?;
    }

    Ok(report_from_run_state(&state))
}

/// Runs the hidden digest worker loop for one existing run.
pub fn run_project_wiki_digest_worker(
    root: Option<PathBuf>,
    project_id: &str,
    run_id: &RunId,
) -> Result<()> {
    let layout = ensure_project_wiki(root, project_id)?;
    let mut state = load_run_state(&layout, run_id)?;
    if is_finished_status(state.status) {
        return Ok(());
    }

    transition_worker_state(
        &layout,
        &mut state,
        RunPhase::ReadingTurns,
        Some(10),
        "Preparing context bundle",
    )?;
    append_run_event(
        &layout,
        run_id,
        RunEvent::info(
            RunPhase::ReadingTurns,
            format!(
                "Loaded {} selected session reference(s)",
                state.selected_sessions.len()
            ),
        ),
    )?;
    transition_worker_state(
        &layout,
        &mut state,
        RunPhase::WaitingForAgent,
        Some(20),
        "Waiting for agent runtime",
    )?;
    append_run_event(
        &layout,
        run_id,
        RunEvent::info(
            RunPhase::WaitingForAgent,
            "Worker is waiting for a future agent runtime implementation".to_owned(),
        ),
    )?;

    let mut last_heartbeat = SystemTime::now();
    loop {
        if cancel_requested(&layout, run_id, &state)? {
            let now = current_utc_timestamp();
            state.status = RunStatus::Canceled;
            state.cancel_requested = true;
            state.updated_at = now.clone();
            state.finished_at = Some(now.clone());
            state.heartbeat_at = Some(now.clone());
            state.headline = Some("Digest run canceled".to_owned());
            store_run_state(&layout, &state)?;
            append_run_event(
                &layout,
                run_id,
                RunEvent::info(RunPhase::WaitingForAgent, "Digest run canceled".to_owned()),
            )?;
            return Ok(());
        }

        if last_heartbeat.elapsed().unwrap_or_default() >= RUN_HEARTBEAT_INTERVAL {
            let now = current_utc_timestamp();
            state.updated_at = now.clone();
            state.heartbeat_at = Some(now);
            store_run_state(&layout, &state)?;
            last_heartbeat = SystemTime::now();
        }

        thread::sleep(RUN_POLL_INTERVAL);
    }
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
    if options.agent_id.trim().is_empty() {
        bail!("agent id must not be empty");
    }
    if options.runtime.trim().is_empty() {
        bail!("runtime must not be empty");
    }
    if options.model.trim().is_empty() {
        bail!("model must not be empty");
    }
    Ok(())
}

/// Validates one `provider:session-id` reference used to select wiki digest sessions.
fn validate_session_ref(session_ref: &str) -> Result<()> {
    let Some((provider, session_id)) = session_ref.split_once(':') else {
        bail!("session ref `{session_ref}` must use the `<provider>:<session-id>` format");
    };
    if !matches!(provider, "claude" | "codex") {
        bail!("session ref `{session_ref}` must start with `claude:` or `codex:`");
    }
    if session_id.trim().is_empty() {
        bail!("session ref `{session_ref}` must include a non-empty session id");
    }
    Ok(())
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
    state: &mut RunState,
    phase: RunPhase,
    progress_percent: Option<u8>,
    headline: &str,
) -> Result<()> {
    let now = current_utc_timestamp();
    state.status = RunStatus::Running;
    state.phase = phase;
    state.updated_at = now.clone();
    state.heartbeat_at = Some(now);
    state.progress_percent = progress_percent;
    state.headline = Some(headline.to_owned());
    store_run_state(layout, state)?;
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

/// Repairs stale in-flight runs whose worker pid is no longer alive.
fn repair_stale_runs(layout: &ProjectLayout) -> Result<()> {
    if !layout.runs_dir.exists() {
        return Ok(());
    }

    let mut run_ids = fs::read_dir(&layout.runs_dir)
        .with_context(|| format!("failed to read {}", layout.runs_dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to read {}", layout.runs_dir.display()))?;
    run_ids.sort_by_key(|entry| entry.path());

    for entry in run_ids {
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }

        let run_id = RunId::new(entry.file_name().to_string_lossy().into_owned())?;
        let mut state = match load_run_state(layout, &run_id) {
            Ok(state) => state,
            Err(_) => continue,
        };
        if !matches!(
            state.status,
            RunStatus::Running | RunStatus::CancelRequested
        ) {
            continue;
        }
        let alive = state.pid.map(process_exists).unwrap_or(false);
        if alive {
            continue;
        }

        let now = current_utc_timestamp();
        state.status = RunStatus::Interrupted;
        state.updated_at = now.clone();
        state.finished_at = Some(now.clone());
        state.heartbeat_at = Some(now.clone());
        state.headline = Some("Digest worker interrupted".to_owned());
        state.error_code = Some("worker_interrupted".to_owned());
        state.error_message = Some("digest worker process was not running".to_owned());
        store_run_state(layout, &state)?;
        append_run_event(
            layout,
            &run_id,
            RunEvent::warn(
                state.phase,
                "Recovered stale run as interrupted because the worker pid was not running"
                    .to_owned(),
            ),
        )?;
    }

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

/// Waits for one worker process to exit cooperatively up to the provided timeout.
fn wait_for_worker_exit(pid: u32, timeout: Duration) {
    let deadline = SystemTime::now() + timeout;
    while SystemTime::now() < deadline {
        if !process_exists(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Returns whether the given pid currently exists.
fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Terminates one existing worker pid using best-effort host process signaling.
fn terminate_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        send_signal(pid, "-TERM")?;
        thread::sleep(Duration::from_millis(200));
        if process_exists(pid) {
            send_signal(pid, "-KILL")?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        bail!("process termination is not implemented on this platform")
    }
}

/// Sends one host signal to one pid through the platform `kill` command.
#[cfg(unix)]
fn send_signal(pid: u32, signal: &str) -> Result<()> {
    let status = Command::new("kill")
        .args([signal, &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to run kill {signal} {pid}"))?;
    if status.success() || !process_exists(pid) {
        Ok(())
    } else {
        bail!("kill {signal} {pid} failed")
    }
}

/// Returns the current UTC timestamp in ISO 8601 format.
fn current_utc_timestamp() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_seconds = duration.as_secs();
    let days = i64::try_from(total_seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_of_day = total_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Converts one Unix-day count into a UTC civil date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
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
    generated_at: String,
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

        let repaired_layout = ensure_project_wiki(Some(root.clone()), project_id)?;
        let repaired = load_run_state(&repaired_layout, &run_id)?;
        assert_eq!(repaired.status, RunStatus::Interrupted);
        assert_eq!(repaired.error_code.as_deref(), Some("worker_interrupted"));

        fs::remove_dir_all(&root)?;
        Ok(())
    }
}
