use std::{
    fs,
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use darc_core::{
    RefreshAllBestEffortReport, RefreshOptions, RefreshProgress, RefreshProjectAttempt,
    RefreshProjectFailure, RefreshReport, SourceKind, config::load_config,
    refresh_all_projects_best_effort_with_progress, refresh_project_with_progress,
};
use darc_paths::current_utc_timestamp;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::args::{ProviderArg, RefreshArgs};
use crate::output::{
    HumanStyle, print_field, print_line, print_project_warning, print_section,
    stderr_progress_enabled,
};
use crate::progress::ProgressOutput;
use crate::service::run_refresh_auto;
use crate::sync_index::{
    add_init_hint_for_unconfigured_project, format_skipped_rollout, format_sources,
    print_index_summary, print_project_run_header, print_sync_result,
};

/// Renders refresh progress events for interactive terminals.
pub(crate) struct RefreshProgressPrinter<W> {
    output: ProgressOutput<W>,
    total_projects: usize,
    current_project_index: usize,
}

impl RefreshProgressPrinter<io::Stderr> {
    /// Builds one refresh progress printer for the current stderr stream.
    pub(crate) fn stderr() -> Self {
        Self::new_with_live_spinner(
            io::stderr(),
            HumanStyle::stderr(),
            stderr_progress_enabled(),
            true,
        )
    }
}

impl<W: Write> RefreshProgressPrinter<W> {
    /// Builds one refresh progress printer from resolved terminal facts.
    #[cfg(test)]
    pub(crate) fn new(writer: W, style: HumanStyle, enabled: bool) -> Self {
        Self::new_with_live_spinner(writer, style, enabled, false)
    }

    /// Builds one refresh progress printer with optional live step animation.
    fn new_with_live_spinner(
        writer: W,
        style: HumanStyle,
        enabled: bool,
        live_spinner: bool,
    ) -> Self {
        Self {
            output: ProgressOutput::new_with_live_spinner(writer, style, enabled, live_spinner),
            total_projects: 1,
            current_project_index: 0,
        }
    }

    /// Finishes any active progress row before the caller prints another message.
    pub(crate) fn finish(&mut self) {
        let _ = self.output.finish();
    }

    /// Records one refresh progress event, ignoring presentation write failures.
    pub(crate) fn record(&mut self, event: RefreshProgress) {
        if self.output.enabled() && self.write_event(event).unwrap_or(false) {
            let _ = self.output.flush();
        }
    }

    /// Writes one refresh progress event to the configured stream.
    pub(crate) fn write_event(&mut self, event: RefreshProgress) -> io::Result<bool> {
        match event {
            RefreshProgress::WorkspaceStarted { total_projects } => {
                self.total_projects = total_projects;
                let style = self.output.style();
                let message = format!(
                    "Refreshing workspace ({} project{})",
                    style.count(total_projects),
                    if total_projects == 1 { "" } else { "s" }
                );
                self.output.heading(&message)?;
                Ok(true)
            }
            RefreshProgress::ProjectStarted {
                project_name,
                project_root: _project_root,
                project_index,
                total_projects,
            } => {
                self.total_projects = total_projects;
                self.current_project_index = project_index;
                let style = self.output.style();
                if total_projects > 1 {
                    let message = format!(
                        "  [{}/{}] {}",
                        style.count(project_index),
                        style.count(total_projects),
                        style.bold(project_name)
                    );
                    self.output.heading(&message)?;
                } else {
                    let message = format!("Refreshing {}", style.bold(project_name));
                    self.output.heading(&message)?;
                }
                Ok(true)
            }
            RefreshProgress::SyncStarted { project_name: _ } => {
                let indent = self.indent();
                self.output.step_with_indent(indent, "Syncing archive...")?;
                Ok(true)
            }
            RefreshProgress::SyncingSessions {
                project_name: _,
                synced_sessions,
                total_sessions,
            } => self.write_session_progress("Syncing sessions", synced_sessions, total_sessions),
            RefreshProgress::SyncFinished { project_name: _ } => Ok(false),
            RefreshProgress::IndexStarted { project_name: _ } => {
                let indent = self.indent();
                self.output
                    .step_with_indent(indent, "Indexing sessions...")?;
                Ok(true)
            }
            RefreshProgress::IndexingSessions {
                project_name: _,
                indexed_sessions,
                total_sessions,
            } => self.write_session_progress("Indexing sessions", indexed_sessions, total_sessions),
            RefreshProgress::IndexFinished { project_name: _ } => Ok(false),
            RefreshProgress::ProjectFinished { project_name: _ } => {
                self.output.finish_active_line()?;
                let style = self.output.style();
                let indent = self.indent();
                writeln!(self.output.writer_mut(), "{}{}", indent, style.ok("done"))?;
                self.write_workspace_project_bar()?;
                writeln!(self.output.writer_mut())?;
                Ok(true)
            }
            RefreshProgress::ProjectFailed { project_name: _ } => {
                self.output.finish_active_line()?;
                let style = self.output.style();
                let indent = self.indent();
                writeln!(
                    self.output.writer_mut(),
                    "{}{}",
                    indent,
                    style.error("failed")
                )?;
                writeln!(self.output.writer_mut())?;
                Ok(true)
            }
        }
    }

    /// Returns the current phase indentation for project or workspace progress.
    pub(crate) fn indent(&self) -> &'static str {
        if self.total_projects > 1 {
            "    "
        } else {
            "  "
        }
    }

    /// Writes the workspace-level project bar after one project finishes.
    fn write_workspace_project_bar(&mut self) -> io::Result<()> {
        if self.total_projects <= 1 {
            return Ok(());
        }
        self.output.write_bar(
            "Projects",
            self.current_project_index as u64,
            self.total_projects as u64,
        )?;
        self.output.finish_active_line()
    }

    /// Writes a throttled session-count progress bar.
    fn write_session_progress(
        &mut self,
        label: &'static str,
        current: usize,
        total: usize,
    ) -> io::Result<bool> {
        let current = current.min(total);
        let indent = self.indent();
        self.output
            .write_throttled_bar_with_indent(indent, label, current as u64, total as u64)
    }
}

pub(crate) const DEFAULT_WATCH_DEBOUNCE: Duration = Duration::from_secs(30);
pub(crate) const DEFAULT_WATCH_MIN_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const DEFAULT_WATCH_RECONCILE_INTERVAL: Duration = Duration::from_secs(600);
pub(crate) const REFRESH_LOCK_SCHEMA: &str = "darc.refresh.lock.v1";

/// Stores one parsed refresh invocation for one-shot and watch modes.
#[derive(Debug, Clone)]
pub(crate) struct RefreshRunRequest {
    pub(crate) root: PathBuf,
    pub(crate) all: bool,
    pub(crate) provider_filter: Vec<SourceKind>,
}

/// Stores command-line watch overrides before config defaults are applied.
#[derive(Debug, Clone, Default)]
pub(crate) struct WatchOverrides {
    pub(crate) debounce: Option<String>,
    pub(crate) min_interval: Option<String>,
    pub(crate) reconcile_interval: Option<String>,
    pub(crate) poll: bool,
}

/// Stores resolved continuous refresh settings.
#[derive(Debug, Clone)]
pub(crate) struct WatchSettings {
    pub(crate) debounce: Duration,
    pub(crate) min_interval: Duration,
    pub(crate) reconcile_interval: Duration,
    pub(crate) provider_filter: Vec<SourceKind>,
    pub(crate) poll: bool,
    pub(crate) watch_paths: Vec<PathBuf>,
}

/// Stores the latest foreground or service refresh state.
#[derive(Debug, Default, Clone)]
pub(crate) struct WatchState {
    pub(crate) watch_identity: Option<WatchIdentity>,
    pub(crate) last_event_at: Option<String>,
    pub(crate) last_refresh_reason: Option<String>,
    pub(crate) last_refresh_started_at: Option<String>,
    pub(crate) last_refresh_completed_at: Option<String>,
    pub(crate) last_refresh_succeeded: Option<bool>,
    pub(crate) last_error: Option<String>,
}

/// Stores the status JSON written by continuous refresh mode.
#[derive(Debug, Serialize)]
pub(crate) struct WatchStatus<'a> {
    pub(crate) schema: &'a str,
    pub(crate) generated_at: String,
    pub(crate) root: String,
    pub(crate) mode: &'a str,
    pub(crate) running: bool,
    pub(crate) watch_pid: Option<u32>,
    pub(crate) watch_token: Option<&'a str>,
    pub(crate) debounce: Option<String>,
    pub(crate) min_interval: Option<String>,
    pub(crate) reconcile_interval: Option<String>,
    pub(crate) poll: Option<bool>,
    pub(crate) last_event_at: Option<&'a str>,
    pub(crate) last_refresh_reason: Option<&'a str>,
    pub(crate) last_refresh_started_at: Option<&'a str>,
    pub(crate) last_refresh_completed_at: Option<&'a str>,
    pub(crate) last_refresh_succeeded: Option<bool>,
    pub(crate) last_error: Option<&'a str>,
}

/// Identifies one running watch loop instance in the status file.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct WatchIdentity {
    pub(crate) pid: u32,
    pub(crate) token: String,
}

impl WatchIdentity {
    /// Builds identity metadata for the current watch process.
    pub(crate) fn current() -> Self {
        let pid = std::process::id();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        Self {
            pid,
            token: format!("{pid}:{nonce}"),
        }
    }

    /// Returns whether a status file still belongs to this watch instance.
    pub(crate) fn matches_status(&self, status: &JsonValue) -> bool {
        status
            .get("watch_pid")
            .and_then(JsonValue::as_u64)
            .is_some_and(|pid| pid == u64::from(self.pid))
            && status
                .get("watch_token")
                .and_then(JsonValue::as_str)
                .is_some_and(|token| token == self.token.as_str())
    }
}

/// Stores the active refresh lock holder for diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RefreshLockInfo {
    pub(crate) schema: String,
    pub(crate) pid: u32,
    pub(crate) started_at: String,
}

impl RefreshLockInfo {
    /// Builds lock metadata for the current process.
    pub(crate) fn current() -> Self {
        Self {
            schema: REFRESH_LOCK_SCHEMA.to_owned(),
            pid: std::process::id(),
            started_at: current_utc_timestamp(),
        }
    }
}

/// Describes whether the refresh lock file is currently held.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefreshLockSnapshot {
    Missing,
    Available { stale_info: Option<RefreshLockInfo> },
    Held { holder: Option<RefreshLockInfo> },
}

/// Holds an advisory refresh lock until dropped.
pub(crate) struct RefreshLock {
    pub(crate) file: File,
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = clear_refresh_lock_info(&mut self.file);
        let _ = self.file.unlock();
    }
}

/// Marks watch status as stopped when the foreground loop exits.
pub(crate) struct WatchStatusGuard {
    pub(crate) root: PathBuf,
    pub(crate) identity: WatchIdentity,
}

impl WatchStatusGuard {
    /// Builds one status guard for a running watch loop.
    pub(crate) fn new(root: PathBuf, identity: WatchIdentity) -> Self {
        Self { root, identity }
    }
}

impl Drop for WatchStatusGuard {
    fn drop(&mut self) {
        let _ = mark_watch_status_stopped_if_current(&self.root, &self.identity);
    }
}

/// Represents filesystem watcher notifications consumed by the refresh loop.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) enum WatchSignal {
    Changed,
    Warning(String),
}

/// Describes why the watch loop should run a refresh cycle.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum WatchRefreshReason {
    Change,
    Reconcile,
}

impl WatchRefreshReason {
    /// Returns the stable status/log label for this refresh reason.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Change => "change",
            Self::Reconcile => "reconcile",
        }
    }
}

/// Runs the daily refresh workflow for one or all projects.
pub(crate) fn run_refresh(args: RefreshArgs) -> Result<()> {
    if args.auto {
        return run_refresh_auto(&args.root);
    }

    let provider_filter = args.provider.into_iter().map(ProviderArg::into).collect();
    let request = RefreshRunRequest {
        root: args.root,
        all: args.all,
        provider_filter,
    };

    if args.watch {
        return run_refresh_watch(
            request,
            WatchOverrides {
                debounce: args.debounce,
                min_interval: args.min_interval,
                reconcile_interval: args.reconcile_interval,
                poll: args.poll,
            },
        );
    }

    run_refresh_once(&request)
}

/// Runs one refresh cycle under the shared workspace lock.
pub(crate) fn run_refresh_once(request: &RefreshRunRequest) -> Result<()> {
    let _lock = if request.root.join("config.toml").exists() {
        Some(acquire_refresh_lock(&request.root)?)
    } else {
        None
    };
    let options = RefreshOptions {
        provider_filter: request.provider_filter.clone(),
    };
    let mut progress = RefreshProgressPrinter::stderr();

    if request.all {
        let report = refresh_all_projects_best_effort_with_progress(
            Some(request.root.clone()),
            options,
            |event| progress.record(event),
        )?;
        progress.finish();
        print_refresh_all_report(&report);
        let result = refresh_all_exit_status(&report);
        return result;
    }

    let report = refresh_project_with_progress(Some(request.root.clone()), options, |event| {
        progress.record(event);
    })
    .map_err(add_init_hint_for_unconfigured_project)?;
    progress.finish();
    print_refresh_report(&report);
    Ok(())
}

/// Runs continuous foreground refresh until interrupted.
pub(crate) fn run_refresh_watch(
    mut request: RefreshRunRequest,
    overrides: WatchOverrides,
) -> Result<()> {
    let settings = load_watch_settings(&request.root, &request.provider_filter, &overrides)?;
    request.provider_filter = settings.provider_filter.clone();
    let style = HumanStyle::stdout();

    print_section(style, "Watch");
    print_field(
        style,
        2,
        "Scope",
        if request.all {
            "the shared workspace"
        } else {
            "the active project"
        },
    );
    print_field(style, 2, "Root", style.path(request.root.display()));
    print_field(style, 2, "Debounce", format_duration(settings.debounce));
    print_field(
        style,
        2,
        "Minimum interval",
        format_duration(settings.min_interval),
    );
    print_field(
        style,
        2,
        "Reconcile interval",
        format_duration(settings.reconcile_interval),
    );
    print_field(
        style,
        2,
        "Watcher",
        if settings.poll {
            style.warn("polling reconcile")
        } else {
            style.ok("macOS filesystem events")
        },
    );
    println!();
    print_section(style, "Watch Paths");
    for path in &settings.watch_paths {
        print_line(2, style.path(path.display()));
    }
    println!();

    let (event_tx, rx) = mpsc::channel();
    #[cfg(target_os = "macos")]
    let _watcher = if settings.poll {
        None
    } else {
        Some(install_native_watchers(
            &settings.watch_paths,
            event_tx.clone(),
        )?)
    };
    #[cfg(not(target_os = "macos"))]
    let _event_tx = event_tx;
    #[cfg(not(target_os = "macos"))]
    if !settings.poll {
        bail!(
            "native watch mode is currently supported only on macOS; pass `--poll` to use periodic reconcile mode"
        );
    }

    let watch_identity = WatchIdentity::current();
    let mut state = WatchState {
        watch_identity: Some(watch_identity.clone()),
        ..WatchState::default()
    };
    write_watch_status(
        &request.root,
        &state,
        true,
        "refresh-watch",
        Some(&settings),
    )?;
    let _status_guard = WatchStatusGuard::new(request.root.clone(), watch_identity);
    run_refresh_cycle(&request, &mut state, &settings, "initial")?;

    let mut dirty_since: Option<Instant> = None;
    let mut last_refresh_at = Some(Instant::now());
    loop {
        let timeout = watch_loop_timeout(dirty_since, last_refresh_at, &settings);
        match rx.recv_timeout(timeout) {
            Ok(WatchSignal::Changed) => {
                record_watch_change(&mut state, &mut dirty_since, Instant::now());
                write_watch_status(
                    &request.root,
                    &state,
                    true,
                    "refresh-watch",
                    Some(&settings),
                )?;
            }
            Ok(WatchSignal::Warning(warning)) => {
                let style = HumanStyle::stderr();
                eprintln!("{}", style.warn(format!("warning [watch]: {warning}")));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("watch event channel disconnected");
            }
        }

        let now = Instant::now();
        if let Some(reason) = next_watch_refresh(dirty_since, last_refresh_at, now, &settings) {
            run_refresh_cycle(&request, &mut state, &settings, reason.as_str())?;
            last_refresh_at = Some(Instant::now());
            dirty_since = None;
        }
    }
}

/// Records one filesystem change and restarts the debounce quiet period.
pub(crate) fn record_watch_change(
    state: &mut WatchState,
    dirty_since: &mut Option<Instant>,
    now: Instant,
) {
    state.last_event_at = Some(current_utc_timestamp());
    *dirty_since = Some(now);
}

/// Runs one watched refresh cycle and records status without terminating on refresh failure.
pub(crate) fn run_refresh_cycle(
    request: &RefreshRunRequest,
    state: &mut WatchState,
    settings: &WatchSettings,
    reason: &str,
) -> Result<()> {
    let style = HumanStyle::stdout();
    println!(
        "[{}] {} ({reason}).",
        style.muted(current_utc_timestamp()),
        style.bold("Running Darc refresh")
    );
    state.last_refresh_reason = Some(reason.to_owned());
    state.last_refresh_started_at = Some(current_utc_timestamp());
    write_watch_status(&request.root, state, true, "refresh-watch", Some(settings))?;

    match run_refresh_once(request) {
        Ok(()) => {
            state.last_refresh_completed_at = Some(current_utc_timestamp());
            state.last_refresh_succeeded = Some(true);
            state.last_error = None;
            write_watch_status(&request.root, state, true, "refresh-watch", Some(settings))?;
            println!(
                "[{}] {}.",
                style.muted(current_utc_timestamp()),
                style.ok("Refresh completed")
            );
        }
        Err(error) => {
            let style = HumanStyle::stderr();
            let message = format!("{error:#}");
            state.last_refresh_completed_at = Some(current_utc_timestamp());
            state.last_refresh_succeeded = Some(false);
            state.last_error = Some(message.clone());
            write_watch_status(&request.root, state, true, "refresh-watch", Some(settings))?;
            eprintln!("{}", style.error(format!("error [watch]: {message}")));
        }
    }
    Ok(())
}

/// Returns the current timeout for the watch loop.
pub(crate) fn watch_loop_timeout(
    dirty_since: Option<Instant>,
    last_refresh_at: Option<Instant>,
    settings: &WatchSettings,
) -> Duration {
    watch_loop_timeout_at(Instant::now(), dirty_since, last_refresh_at, settings)
}

/// Returns the timeout for a watch loop iteration at one instant.
pub(crate) fn watch_loop_timeout_at(
    now: Instant,
    dirty_since: Option<Instant>,
    last_refresh_at: Option<Instant>,
    settings: &WatchSettings,
) -> Duration {
    let mut deadline = last_refresh_at
        .map(|last_refresh_at| last_refresh_at + settings.reconcile_interval)
        .unwrap_or(now);
    if let Some(dirty_since) = dirty_since {
        let mut dirty_deadline = dirty_since + settings.debounce;
        if let Some(last_refresh_at) = last_refresh_at {
            dirty_deadline = dirty_deadline.max(last_refresh_at + settings.min_interval);
        }
        if dirty_deadline < deadline {
            deadline = dirty_deadline;
        }
    }
    deadline.saturating_duration_since(now)
}

/// Returns the refresh reason that is due at the given instant.
pub(crate) fn next_watch_refresh(
    dirty_since: Option<Instant>,
    last_refresh_at: Option<Instant>,
    now: Instant,
    settings: &WatchSettings,
) -> Option<WatchRefreshReason> {
    if should_run_reconcile_refresh(last_refresh_at, now, settings) {
        Some(WatchRefreshReason::Reconcile)
    } else if should_run_watched_refresh(dirty_since, last_refresh_at, now, settings) {
        Some(WatchRefreshReason::Change)
    } else {
        None
    }
}

/// Returns whether the periodic safety refresh is due.
pub(crate) fn should_run_reconcile_refresh(
    last_refresh_at: Option<Instant>,
    now: Instant,
    settings: &WatchSettings,
) -> bool {
    last_refresh_at
        .map(|last_refresh_at| now.duration_since(last_refresh_at) >= settings.reconcile_interval)
        .unwrap_or(true)
}

/// Returns whether the watch loop should run a refresh now.
pub(crate) fn should_run_watched_refresh(
    dirty_since: Option<Instant>,
    last_refresh_at: Option<Instant>,
    now: Instant,
    settings: &WatchSettings,
) -> bool {
    dirty_since.is_some_and(|started| {
        let refresh_ready = last_refresh_at
            .map(|last_refresh_at| now.duration_since(last_refresh_at) >= settings.min_interval)
            .unwrap_or(true);
        now.duration_since(started) >= settings.debounce && refresh_ready
    })
}

/// Resolves watch settings from CLI overrides and the shared config.
pub(crate) fn load_watch_settings(
    root: &Path,
    cli_providers: &[SourceKind],
    overrides: &WatchOverrides,
) -> Result<WatchSettings> {
    let config_path = root.join("config.toml");
    let config = load_config(&config_path)
        .with_context(|| format!("failed to load watch config from {}", config_path.display()))?;
    let watch = config.watch.clone();
    let debounce = parse_watch_duration(
        "debounce",
        overrides.debounce.as_ref().or(watch.debounce.as_ref()),
        DEFAULT_WATCH_DEBOUNCE,
    )?;
    let min_interval = parse_watch_duration(
        "min_interval",
        overrides
            .min_interval
            .as_ref()
            .or(watch.min_interval.as_ref()),
        DEFAULT_WATCH_MIN_INTERVAL,
    )?;
    let reconcile_interval = parse_watch_duration(
        "reconcile_interval",
        overrides
            .reconcile_interval
            .as_ref()
            .or(watch.reconcile_interval.as_ref()),
        DEFAULT_WATCH_RECONCILE_INTERVAL,
    )?;
    let provider_filter = if cli_providers.is_empty() {
        watch.providers.clone()
    } else {
        cli_providers.to_vec()
    };
    let poll = overrides.poll || watch.poll;
    let watch_paths = watch_paths(root, &config, &provider_filter)?;

    Ok(WatchSettings {
        debounce,
        min_interval,
        reconcile_interval,
        provider_filter,
        poll,
        watch_paths,
    })
}

/// Builds the source and config paths that can trigger a watched refresh.
pub(crate) fn watch_paths(
    root: &Path,
    config: &darc_core::config::SharedConfig,
    provider_filter: &[SourceKind],
) -> Result<Vec<PathBuf>> {
    let mut paths = vec![root.join("config.toml")];
    if (provider_filter.is_empty() || provider_filter.contains(&SourceKind::Claude))
        && let Some(source) = &config.sources.claude
        && source.enabled
    {
        paths.push(source.projects_root.clone());
    }
    if (provider_filter.is_empty() || provider_filter.contains(&SourceKind::Codex))
        && let Some(source) = &config.sources.codex
        && source.enabled
    {
        paths.push(source.sessions_root.clone());
        paths.push(source.home.join("archived_sessions"));
    }

    let existing = paths
        .into_iter()
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    if existing.is_empty() {
        bail!("no existing Darc config or source paths are available to watch");
    }
    Ok(existing)
}

/// Parses one watch duration setting.
pub(crate) fn parse_watch_duration(
    name: &str,
    value: Option<&String>,
    default: Duration,
) -> Result<Duration> {
    match value {
        Some(value) => parse_duration(value)
            .with_context(|| format!("invalid watch `{name}` duration `{value}`")),
        None => Ok(default),
    }
}

/// Parses a compact duration such as `500ms`, `30s`, `5m`, or `1h`.
pub(crate) fn parse_duration(value: &str) -> Result<Duration> {
    let value = value.trim();
    if value.is_empty() {
        bail!("duration must not be empty");
    }
    let digit_len = value.bytes().take_while(u8::is_ascii_digit).count();
    if digit_len == 0 || digit_len == value.len() {
        bail!("duration must use a unit: ms, s, m, or h");
    }
    let amount = value[..digit_len]
        .parse::<u64>()
        .context("duration amount must be an unsigned integer")?;
    let duration = match &value[digit_len..] {
        "ms" => Duration::from_millis(amount),
        "s" => Duration::from_secs(amount),
        "m" => Duration::from_secs(amount.saturating_mul(60)),
        "h" => Duration::from_secs(amount.saturating_mul(3_600)),
        unit => bail!("unsupported duration unit `{unit}`; use ms, s, m, or h"),
    };
    if duration.is_zero() {
        bail!("duration must be greater than zero");
    }
    Ok(duration)
}

/// Formats one duration in a compact CLI-friendly form.
pub(crate) fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis.is_multiple_of(3_600_000) {
        format!("{}h", millis / 3_600_000)
    } else if millis.is_multiple_of(60_000) {
        format!("{}m", millis / 60_000)
    } else if millis.is_multiple_of(1_000) {
        format!("{}s", millis / 1_000)
    } else {
        format!("{millis}ms")
    }
}

/// Acquires the shared refresh lock for this Darc root.
pub(crate) fn acquire_refresh_lock(root: &Path) -> Result<RefreshLock> {
    let run_dir = root.join("run");
    fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create {}", run_dir.display()))?;
    let lock_path = run_dir.join("refresh.lock");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    file.try_lock_exclusive().with_context(|| {
        let holder = read_refresh_lock_info(&lock_path)
            .ok()
            .flatten()
            .map(|info| format!(" by process {} since {}", info.pid, info.started_at))
            .unwrap_or_default();
        format!(
            "another Darc refresh is already running{holder} ({})",
            lock_path.display()
        )
    })?;
    write_refresh_lock_info(&mut file, &RefreshLockInfo::current())?;
    Ok(RefreshLock { file })
}

/// Inspects the shared refresh lock without taking ownership of it.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn inspect_refresh_lock(root: &Path) -> Result<RefreshLockSnapshot> {
    let lock_path = root.join("run/refresh.lock");
    if !lock_path.exists() {
        return Ok(RefreshLockSnapshot::Missing);
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    let info = read_refresh_lock_info(&lock_path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            file.unlock()
                .with_context(|| format!("failed to unlock {}", lock_path.display()))?;
            Ok(RefreshLockSnapshot::Available { stale_info: info })
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            Ok(RefreshLockSnapshot::Held { holder: info })
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect refresh lock {}", lock_path.display())),
    }
}

/// Writes refresh lock holder metadata into an acquired lock file.
pub(crate) fn write_refresh_lock_info(file: &mut File, info: &RefreshLockInfo) -> Result<()> {
    file.set_len(0).context("failed to truncate refresh lock")?;
    file.seek(SeekFrom::Start(0))
        .context("failed to rewind refresh lock")?;
    serde_json::to_writer_pretty(&mut *file, info).context("failed to serialize refresh lock")?;
    file.write_all(b"\n")
        .context("failed to write refresh lock newline")?;
    file.flush().context("failed to flush refresh lock")
}

/// Reads refresh lock holder metadata when the lock file contains it.
pub(crate) fn read_refresh_lock_info(lock_path: &Path) -> Result<Option<RefreshLockInfo>> {
    let content = fs::read_to_string(lock_path)
        .with_context(|| format!("failed to read {}", lock_path.display()))?;
    if content.trim().is_empty() {
        return Ok(None);
    }
    let Ok(info) = serde_json::from_str::<RefreshLockInfo>(&content) else {
        return Ok(None);
    };
    Ok((info.schema == REFRESH_LOCK_SCHEMA).then_some(info))
}

/// Clears refresh lock holder metadata before releasing the lock.
pub(crate) fn clear_refresh_lock_info(file: &mut File) -> Result<()> {
    file.set_len(0).context("failed to clear refresh lock")?;
    file.seek(SeekFrom::Start(0))
        .context("failed to rewind refresh lock")?;
    file.flush().context("failed to flush refresh lock")
}

/// Writes the current continuous refresh status JSON.
pub(crate) fn write_watch_status(
    root: &Path,
    state: &WatchState,
    running: bool,
    mode: &str,
    settings: Option<&WatchSettings>,
) -> Result<()> {
    let run_dir = root.join("run");
    fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create {}", run_dir.display()))?;
    let watch_identity = state.watch_identity.as_ref();
    let status = WatchStatus {
        schema: "darc.watch.status.v1",
        generated_at: current_utc_timestamp(),
        root: root.display().to_string(),
        mode,
        running,
        watch_pid: watch_identity.map(|identity| identity.pid),
        watch_token: watch_identity.map(|identity| identity.token.as_str()),
        debounce: settings.map(|settings| format_duration(settings.debounce)),
        min_interval: settings.map(|settings| format_duration(settings.min_interval)),
        reconcile_interval: settings.map(|settings| format_duration(settings.reconcile_interval)),
        poll: settings.map(|settings| settings.poll),
        last_event_at: state.last_event_at.as_deref(),
        last_refresh_reason: state.last_refresh_reason.as_deref(),
        last_refresh_started_at: state.last_refresh_started_at.as_deref(),
        last_refresh_completed_at: state.last_refresh_completed_at.as_deref(),
        last_refresh_succeeded: state.last_refresh_succeeded,
        last_error: state.last_error.as_deref(),
    };
    let content = serde_json::to_vec_pretty(&status).context("failed to serialize watch status")?;
    let status_path = run_dir.join("status.json");
    fs::write(&status_path, content)
        .with_context(|| format!("failed to write {}", status_path.display()))
}

/// Marks an existing watch status file as stopped while preserving its last refresh details.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn mark_watch_status_stopped(root: &Path) -> Result<()> {
    mark_watch_status_stopped_matching(root, None)
}

/// Marks watch status stopped only when it still belongs to this watch instance.
pub(crate) fn mark_watch_status_stopped_if_current(
    root: &Path,
    identity: &WatchIdentity,
) -> Result<()> {
    mark_watch_status_stopped_matching(root, Some(identity))
}

/// Marks watch status stopped after optionally checking status ownership.
pub(crate) fn mark_watch_status_stopped_matching(
    root: &Path,
    expected_identity: Option<&WatchIdentity>,
) -> Result<()> {
    let run_dir = root.join("run");
    fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create {}", run_dir.display()))?;
    let status_path = run_dir.join("status.json");
    let mut status = stopped_watch_status_value(root, &status_path)?;
    if expected_identity.is_some_and(|identity| !identity.matches_status(&status)) {
        return Ok(());
    }
    let object = status.as_object_mut().expect("stopped status is an object");
    object.insert(
        "generated_at".to_owned(),
        JsonValue::String(current_utc_timestamp()),
    );
    object.insert("running".to_owned(), JsonValue::Bool(false));
    let content = serde_json::to_vec_pretty(&status).context("failed to serialize watch status")?;
    fs::write(&status_path, content)
        .with_context(|| format!("failed to write {}", status_path.display()))
}

/// Returns the status object to update when marking a watch stopped.
pub(crate) fn stopped_watch_status_value(root: &Path, status_path: &Path) -> Result<JsonValue> {
    let content = match fs::read_to_string(status_path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(minimal_stopped_watch_status(root));
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", status_path.display()));
        }
    };
    let Ok(status) = serde_json::from_str::<JsonValue>(&content) else {
        return Ok(minimal_stopped_watch_status(root));
    };
    if status.is_object() {
        Ok(status)
    } else {
        Ok(minimal_stopped_watch_status(root))
    }
}

/// Builds a minimal stopped watch status for missing or malformed status files.
pub(crate) fn minimal_stopped_watch_status(root: &Path) -> JsonValue {
    serde_json::json!({
        "schema": "darc.watch.status.v1",
        "generated_at": current_utc_timestamp(),
        "root": root.display().to_string(),
        "mode": "refresh-watch",
        "running": false,
        "watch_pid": null,
        "watch_token": null,
        "debounce": null,
        "min_interval": null,
        "reconcile_interval": null,
        "poll": null,
        "last_event_at": null,
        "last_refresh_reason": null,
        "last_refresh_started_at": null,
        "last_refresh_completed_at": null,
        "last_refresh_succeeded": null,
        "last_error": null,
    })
}

/// Installs native macOS watchers for the selected paths.
#[cfg(target_os = "macos")]
pub(crate) fn install_native_watchers(
    paths: &[PathBuf],
    tx: mpsc::Sender<WatchSignal>,
) -> Result<notify::RecommendedWatcher> {
    use notify::{Config, RecursiveMode, Watcher};

    let mut watcher = notify::RecommendedWatcher::new(
        move |event: notify::Result<notify::Event>| match event {
            Ok(_event) => {
                let _ = tx.send(WatchSignal::Changed);
            }
            Err(error) => {
                let _ = tx.send(WatchSignal::Warning(error.to_string()));
            }
        },
        Config::default(),
    )
    .context("failed to create macOS filesystem watcher")?;

    for path in paths {
        watcher
            .watch(path, RecursiveMode::Recursive)
            .with_context(|| format!("failed to watch {}", path.display()))?;
    }
    Ok(watcher)
}

/// Prints the combined sync and index summary for one refreshed project.
pub(crate) fn print_refresh_report(report: &RefreshReport) {
    let style = HumanStyle::stdout();
    print_refresh_report_with_style(style, report);
}

/// Prints the combined sync and index summary using one resolved style context.
pub(crate) fn print_refresh_report_with_style(style: HumanStyle, report: &RefreshReport) {
    for warning in &report.sync.warnings {
        print_project_warning(&report.sync.project_name, warning);
    }
    for skipped in &report.index.skipped_rollouts {
        print_project_warning(&report.sync.project_name, format_skipped_rollout(skipped));
    }

    print_project_run_header(
        style,
        "Refresh",
        &report.sync.project_name,
        &report.sync.project_root,
        Some(report.sync.sessions_root.as_path()),
    );
    println!();
    print_section(style, "Providers");
    match format_refresh_provider_lines(report) {
        RefreshProviderLines::Shared(providers) => print_field(style, 2, "Selected", providers),
        RefreshProviderLines::Split {
            sync_providers,
            index_providers,
        } => {
            print_field(style, 2, "Sync", sync_providers);
            print_field(style, 2, "Index", index_providers);
        }
    }
    println!();
    print_sync_result(style, &report.sync);
    println!();
    print_index_summary(style, &report.index);
    println!();
    print_section(style, "Changes");
    print_field(
        style,
        2,
        "Manifest",
        if report.sync.manifest_written {
            style.ok("updated")
        } else {
            style.muted("unchanged")
        },
    );
    print_field(
        style,
        2,
        "Config",
        if report.sync.config_written {
            style.ok("updated")
        } else {
            style.muted("unchanged")
        },
    );
    println!();
    print_section(style, "Status");
    let status = if report.index.skipped_rollouts.is_empty() {
        style.ok("refreshed")
    } else {
        style.warn("refreshed with skipped rollouts")
    };
    print_field(style, 2, "Overall", status);
}

/// Prints one multi-project refresh report with per-project results and totals.
pub(crate) fn print_refresh_all_report(report: &RefreshAllBestEffortReport) {
    let style = HumanStyle::stdout();
    for (index, project) in report.projects.iter().enumerate() {
        if index > 0 {
            println!();
        }
        print_refresh_all_project_report(style, project);
    }
    println!();
    print_section(style, "Workspace Summary");
    print_field(style, 2, "Succeeded", style.ok(report.refreshed_count()));
    let failed = report.failed_count();
    let failed = if failed == 0 {
        style.ok(failed)
    } else {
        style.error(failed)
    };
    print_field(style, 2, "Failed", failed);
}

/// Prints one project-scoped entry from a multi-project refresh report.
pub(crate) fn print_refresh_all_project_report(style: HumanStyle, project: &RefreshProjectAttempt) {
    match project {
        RefreshProjectAttempt::Refreshed(report) => print_refresh_report_with_style(style, report),
        RefreshProjectAttempt::Failed(failure) => print_refresh_project_failure(style, failure),
    }
}

/// Prints one structured project refresh failure from a best-effort workspace refresh.
pub(crate) fn print_refresh_project_failure(style: HumanStyle, failure: &RefreshProjectFailure) {
    print_project_run_header(
        style,
        "Refresh",
        &failure.project_name,
        &failure.project_root,
        None,
    );
    println!();
    print_section(style, "Status");
    print_field(style, 2, "Overall", style.error("failed"));
    print_field(
        style,
        2,
        "Error",
        style.error(format!("{:#}", failure.error)),
    );
}

/// Stores the provider lines rendered for one refresh report.
pub(crate) enum RefreshProviderLines {
    Shared(String),
    Split {
        sync_providers: String,
        index_providers: String,
    },
}

/// Formats the provider lines for one refresh report.
pub(crate) fn format_refresh_provider_lines(report: &RefreshReport) -> RefreshProviderLines {
    let sync_providers = format_sources(&report.sync.sources);
    let index_providers = format_sources(&report.index.providers);
    if report.sync.sources == report.index.providers {
        RefreshProviderLines::Shared(sync_providers)
    } else {
        RefreshProviderLines::Split {
            sync_providers,
            index_providers,
        }
    }
}

/// Converts one workspace refresh report into the final CLI exit result.
pub(crate) fn refresh_all_exit_status(report: &RefreshAllBestEffortReport) -> Result<()> {
    if report.has_failures() {
        bail!(
            "{} project(s) failed during workspace refresh",
            report.failed_count()
        );
    }
    Ok(())
}
