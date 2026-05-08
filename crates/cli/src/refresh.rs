#[cfg(target_os = "macos")]
use std::process::Command;

use super::*;

/// Renders refresh progress events for interactive terminals.
pub(crate) struct RefreshProgressPrinter<W> {
    pub(crate) writer: W,
    pub(crate) style: HumanStyle,
    pub(crate) enabled: bool,
    pub(crate) total_projects: usize,
}

impl RefreshProgressPrinter<io::Stderr> {
    /// Builds one refresh progress printer for the current stderr stream.
    pub(crate) fn stderr() -> Self {
        let term = env::var("TERM").ok();
        Self::new(
            io::stderr(),
            HumanStyle::stderr(),
            io::stderr().is_terminal() && term.as_deref() != Some("dumb"),
        )
    }
}

impl<W: Write> RefreshProgressPrinter<W> {
    /// Builds one refresh progress printer from resolved terminal facts.
    pub(crate) fn new(writer: W, style: HumanStyle, enabled: bool) -> Self {
        Self {
            writer,
            style,
            enabled,
            total_projects: 1,
        }
    }

    /// Records one refresh progress event, ignoring presentation write failures.
    pub(crate) fn record(&mut self, event: RefreshProgress) {
        if self.enabled {
            let _ = self.write_event(event);
            let _ = self.writer.flush();
        }
    }

    /// Writes one refresh progress event to the configured stream.
    pub(crate) fn write_event(&mut self, event: RefreshProgress) -> io::Result<()> {
        match event {
            RefreshProgress::WorkspaceStarted { total_projects } => {
                self.total_projects = total_projects;
                writeln!(
                    self.writer,
                    "Refreshing workspace ({} project{})",
                    self.style.count(total_projects),
                    if total_projects == 1 { "" } else { "s" }
                )
            }
            RefreshProgress::ProjectStarted {
                project_name,
                project_root: _project_root,
                project_index,
                total_projects,
            } => {
                self.total_projects = total_projects;
                if total_projects > 1 {
                    writeln!(
                        self.writer,
                        "  [{}/{}] {}",
                        self.style.count(project_index),
                        self.style.count(total_projects),
                        self.style.bold(project_name)
                    )
                } else {
                    writeln!(self.writer, "Refreshing {}", self.style.bold(project_name))
                }
            }
            RefreshProgress::SyncStarted { project_name: _ } => {
                writeln!(self.writer, "{}[1/2] Syncing archive...", self.indent())
            }
            RefreshProgress::SyncFinished { project_name: _ } => Ok(()),
            RefreshProgress::IndexStarted { project_name: _ } => {
                writeln!(self.writer, "{}[2/2] Indexing sessions...", self.indent())
            }
            RefreshProgress::IndexFinished { project_name: _ } => Ok(()),
            RefreshProgress::ProjectFinished { project_name: _ } => {
                writeln!(self.writer, "{}{}", self.indent(), self.style.ok("done"))?;
                writeln!(self.writer)
            }
            RefreshProgress::ProjectFailed { project_name: _ } => {
                writeln!(
                    self.writer,
                    "{}{}",
                    self.indent(),
                    self.style.error("failed")
                )?;
                writeln!(self.writer)
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
}

/// Renders automatic background refresh setup progress for interactive terminals.
#[cfg(any(target_os = "macos", test))]
pub(crate) struct ServiceProgressPrinter<W> {
    pub(crate) writer: W,
    pub(crate) style: HumanStyle,
    pub(crate) enabled: bool,
}

#[cfg(target_os = "macos")]
impl ServiceProgressPrinter<io::Stderr> {
    /// Builds one service progress printer for the current stderr stream.
    pub(crate) fn stderr() -> Self {
        let term = env::var("TERM").ok();
        Self::new(
            io::stderr(),
            HumanStyle::stderr(),
            io::stderr().is_terminal() && term.as_deref() != Some("dumb"),
        )
    }
}

#[cfg(any(target_os = "macos", test))]
impl<W: Write> ServiceProgressPrinter<W> {
    /// Builds one service progress printer from resolved terminal facts.
    pub(crate) fn new(writer: W, style: HumanStyle, enabled: bool) -> Self {
        Self {
            writer,
            style,
            enabled,
        }
    }

    /// Writes the setup start message.
    pub(crate) fn started(&mut self) {
        if self.enabled {
            let _ = writeln!(self.writer, "Enabling background auto-refresh.");
            let _ = writeln!(
                self.writer,
                "Initial refresh backfills the SQLite index and may take a few seconds."
            );
            let _ = self.writer.flush();
        }
    }

    /// Writes one numbered setup step.
    pub(crate) fn step(&mut self, index: usize, total: usize, message: &str) {
        if self.enabled {
            let _ = writeln!(
                self.writer,
                "  [{}/{}] {}",
                self.style.count(index),
                self.style.count(total),
                message
            );
            let _ = self.writer.flush();
        }
    }

    /// Writes the setup completion message.
    pub(crate) fn done(&mut self) {
        if self.enabled {
            let _ = writeln!(self.writer, "  {}", self.style.ok("done"));
            let _ = writeln!(self.writer);
            let _ = self.writer.flush();
        }
    }
}

pub(crate) const DEFAULT_WATCH_DEBOUNCE: Duration = Duration::from_secs(30);
pub(crate) const DEFAULT_WATCH_MIN_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const DEFAULT_WATCH_RECONCILE_INTERVAL: Duration = Duration::from_secs(600);

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

/// Holds an advisory refresh lock until dropped.
pub(crate) struct RefreshLock {
    pub(crate) file: File,
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
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

/// Enables automatic background refresh and starts it immediately.
#[cfg(target_os = "macos")]
pub(crate) fn run_refresh_auto(root: &Path) -> Result<()> {
    let mut progress = ServiceProgressPrinter::stderr();
    progress.started();
    progress.step(1, 2, "Writing LaunchAgent...");
    let plist_path = write_macos_launch_agent(root, true)?;
    progress.step(2, 2, "Starting background service...");
    let outcome = start_macos_service_impl(root)?;
    progress.done();

    let style = HumanStyle::stdout();
    print_section(style, "Service");
    print_field(style, 2, "Status", style.ok(outcome.auto_status()));
    print_field(style, 2, "LaunchAgent", style.path(plist_path.display()));
    print_field(style, 2, "Command", watch_all_command(root, style));
    if let Some(hint) = outcome.auto_hint() {
        print_field(style, 2, "Note", style.muted(hint));
    }
    Ok(())
}

/// Reports unsupported automatic background refresh on non-macOS platforms.
#[cfg(not(target_os = "macos"))]
pub(crate) fn run_refresh_auto(_root: &Path) -> Result<()> {
    bail!("`darc refresh --auto` is currently supported only on macOS")
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
        print_refresh_all_report(&report);
        let result = refresh_all_exit_status(&report);
        return result;
    }

    let report = refresh_project_with_progress(Some(request.root.clone()), options, |event| {
        progress.record(event);
    })
    .map_err(add_init_hint_for_unconfigured_project)?;
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

    let mut state = WatchState::default();
    write_watch_status(
        &request.root,
        &state,
        true,
        "refresh-watch",
        Some(&settings),
    )?;
    run_refresh_cycle(&request, &mut state, &settings, "initial")?;

    let mut dirty_since: Option<Instant> = None;
    let mut last_refresh_at = Some(Instant::now());
    loop {
        let timeout = watch_loop_timeout(dirty_since, last_refresh_at, &settings);
        match rx.recv_timeout(timeout) {
            Ok(WatchSignal::Changed) => {
                state.last_event_at = Some(current_utc_timestamp());
                dirty_since.get_or_insert_with(Instant::now);
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
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    file.try_lock_exclusive().with_context(|| {
        format!(
            "another Darc refresh is already running ({})",
            lock_path.display()
        )
    })?;
    Ok(RefreshLock { file })
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
    let status = WatchStatus {
        schema: "darc.watch.status.v1",
        generated_at: current_utc_timestamp(),
        root: root.display().to_string(),
        mode,
        running,
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

#[cfg(target_os = "macos")]
pub(crate) const MACOS_SERVICE_LABEL: &str = "com.0xjunha.darc.refresh";
#[cfg(target_os = "macos")]
pub(crate) const MACOS_SERVICE_UNLOAD_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_os = "macos")]
pub(crate) const MACOS_SERVICE_RETRY_DELAY: Duration = Duration::from_millis(250);
#[cfg(target_os = "macos")]
pub(crate) const MACOS_SERVICE_BOOTSTRAP_ATTEMPTS: usize = 4;

/// Describes whether service start created a new service or replaced an existing one.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MacosServiceStartOutcome {
    Started,
    Restarted,
}

#[cfg(any(target_os = "macos", test))]
impl MacosServiceStartOutcome {
    /// Returns the status text for `darc refresh --auto`.
    pub(crate) fn auto_status(self) -> &'static str {
        match self {
            Self::Started => "enabled and started",
            Self::Restarted => "enabled and restarted",
        }
    }

    /// Returns the status text for `darc service start`.
    pub(crate) fn service_status(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Restarted => "restarted",
        }
    }

    /// Returns the explanatory hint for an automatic refresh restart.
    pub(crate) fn auto_hint(self) -> Option<&'static str> {
        match self {
            Self::Started => None,
            Self::Restarted => Some(
                "auto-refresh was already running; Darc stopped the existing service and started the updated one",
            ),
        }
    }
}

/// Dispatches one service lifecycle command.
pub(crate) fn run_service(args: ServiceArgs) -> Result<()> {
    run_platform_service(args)
}

/// Runs one macOS LaunchAgent service command.
#[cfg(target_os = "macos")]
pub(crate) fn run_platform_service(args: ServiceArgs) -> Result<()> {
    match args.command {
        ServiceCommands::Start => start_macos_service(&args.root),
        ServiceCommands::Stop => stop_macos_service(&args.root),
        ServiceCommands::Restart => {
            stop_macos_service(&args.root)?;
            start_macos_service(&args.root)
        }
        ServiceCommands::Status => print_macos_service_status(&args.root),
        ServiceCommands::Enable => enable_macos_service(&args.root),
        ServiceCommands::Disable => disable_macos_service(&args.root),
    }
}

/// Reports unsupported service management on non-macOS platforms.
#[cfg(not(target_os = "macos"))]
pub(crate) fn run_platform_service(_args: ServiceArgs) -> Result<()> {
    bail!("`darc service` is currently supported only on macOS")
}

/// Enables the macOS LaunchAgent for future logins.
#[cfg(target_os = "macos")]
pub(crate) fn enable_macos_service(root: &Path) -> Result<()> {
    let plist_path = write_macos_launch_agent(root, true)?;
    let style = HumanStyle::stdout();
    print_section(style, "Service");
    print_field(style, 2, "Status", style.ok("enabled"));
    print_field(style, 2, "LaunchAgent", style.path(plist_path.display()));
    print_line(
        2,
        style.muted("Run `darc service start` to start it in this login session."),
    );
    Ok(())
}

/// Disables and unloads the macOS LaunchAgent.
#[cfg(target_os = "macos")]
pub(crate) fn disable_macos_service(root: &Path) -> Result<()> {
    stop_macos_service(root)?;
    let plist_path = macos_launch_agent_path()?;
    if plist_path.exists() {
        fs::remove_file(&plist_path)
            .with_context(|| format!("failed to remove {}", plist_path.display()))?;
        let style = HumanStyle::stdout();
        print_section(style, "Service");
        print_field(style, 2, "Status", style.warn("disabled"));
        print_field(
            style,
            2,
            "Removed LaunchAgent",
            style.path(plist_path.display()),
        );
    } else {
        let style = HumanStyle::stdout();
        print_section(style, "Service");
        print_field(style, 2, "Status", style.muted("already disabled"));
    }
    remove_macos_runtime_plist(root)?;
    Ok(())
}

/// Starts or restarts the macOS LaunchAgent in the current login session.
#[cfg(target_os = "macos")]
pub(crate) fn start_macos_service(root: &Path) -> Result<()> {
    let outcome = start_macos_service_impl(root)?;
    let style = HumanStyle::stdout();
    print_section(style, "Service");
    print_field(style, 2, "Status", style.ok(outcome.service_status()));
    print_field(style, 2, "Command", watch_all_command(root, style));
    Ok(())
}

/// Starts or restarts the macOS LaunchAgent without printing status.
#[cfg(target_os = "macos")]
pub(crate) fn start_macos_service_impl(root: &Path) -> Result<MacosServiceStartOutcome> {
    let launch_agent_path = macos_launch_agent_path()?;
    let plist_path = if launch_agent_path.exists() {
        launch_agent_path
    } else {
        write_macos_runtime_plist(root)?
    };
    let domain = macos_launch_domain()?;
    let target = macos_launch_target_for_domain(&domain);
    let outcome = if macos_service_target_loaded(&target)? {
        run_launchctl(&macos_service_bootout_launchctl_args(&target))?;
        wait_for_macos_service_unloaded(&target)?;
        MacosServiceStartOutcome::Restarted
    } else {
        MacosServiceStartOutcome::Started
    };

    run_launchctl_with_bootstrap_retry(&macos_service_bootstrap_launchctl_args(
        &plist_path,
        &domain,
    ))?;
    run_launchctl(&macos_service_kickstart_launchctl_args(&target))?;
    Ok(outcome)
}

/// Waits until launchd no longer reports the service target as loaded.
#[cfg(target_os = "macos")]
pub(crate) fn wait_for_macos_service_unloaded(target: &str) -> Result<()> {
    let deadline = Instant::now() + MACOS_SERVICE_UNLOAD_TIMEOUT;
    loop {
        if !macos_service_target_loaded(target)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for the macOS LaunchAgent to unload\n  Target: {target}\n  Hint: launchd still reports the old Darc auto-refresh service as loaded"
            );
        }
        std::thread::sleep(MACOS_SERVICE_RETRY_DELAY);
    }
}

/// Builds the launchctl commands needed to start the service plist.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn macos_service_bootstrap_launchctl_args(
    plist_path: &Path,
    domain: &str,
) -> Vec<String> {
    vec![
        "bootstrap".to_owned(),
        domain.to_owned(),
        plist_path.display().to_string(),
    ]
}

/// Builds the launchctl command needed to unload the service target.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn macos_service_bootout_launchctl_args(target: &str) -> Vec<String> {
    vec!["bootout".to_owned(), target.to_owned()]
}

/// Builds the launchctl command needed to kickstart the service target.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn macos_service_kickstart_launchctl_args(target: &str) -> Vec<String> {
    vec!["kickstart".to_owned(), "-k".to_owned(), target.to_owned()]
}

/// Formats the foreground command used by the background refresh service.
#[cfg(target_os = "macos")]
pub(crate) fn watch_all_command(root: &Path, style: HumanStyle) -> String {
    format!(
        "darc refresh --watch --all --root {}",
        style.path(root.display())
    )
}

/// Stops the macOS LaunchAgent in the current login session.
#[cfg(target_os = "macos")]
pub(crate) fn stop_macos_service(root: &Path) -> Result<()> {
    let style = HumanStyle::stdout();
    if macos_service_loaded()? {
        run_launchctl(&["bootout".to_owned(), macos_launch_target()?])?;
        print_section(style, "Service");
        print_field(style, 2, "Status", style.warn("stopped"));
    } else {
        print_section(style, "Service");
        print_field(style, 2, "Status", style.muted("not running"));
    }
    remove_macos_runtime_plist(root)?;
    Ok(())
}

/// Prints macOS LaunchAgent and Darc watch status.
#[cfg(target_os = "macos")]
pub(crate) fn print_macos_service_status(root: &Path) -> Result<()> {
    let plist_path = macos_launch_agent_path()?;
    let runtime_plist_path = macos_runtime_plist_path(root);
    let enabled = plist_path.exists();
    let running = macos_service_loaded()?;
    let style = HumanStyle::stdout();
    print_section(style, "Service");
    print_field(style, 2, "Name", "Darc refresh");
    print_field(style, 2, "Platform", "macOS LaunchAgent");
    print_field(style, 2, "Label", style.muted(MACOS_SERVICE_LABEL));
    print_field(style, 2, "Enabled", yes_no(style, enabled));
    print_field(style, 2, "Running", yes_no(style, running));
    let launch_agent = if enabled {
        style.path(plist_path.display())
    } else if running && runtime_plist_path.exists() {
        format!(
            "{} {}",
            style.path(runtime_plist_path.display()),
            style.muted("(runtime)")
        )
    } else {
        style.path(plist_path.display())
    };
    print_field(style, 2, "LaunchAgent", launch_agent);

    println!();
    print_section(style, "Watch Status");
    let status_path = root.join("run/status.json");
    if status_path.exists() {
        let content = fs::read_to_string(&status_path)
            .with_context(|| format!("failed to read {}", status_path.display()))?;
        let status: JsonValue =
            serde_json::from_str(&content).context("failed to parse watch status JSON")?;
        print_field(style, 2, "Status file", style.path(status_path.display()));
        print_field(
            style,
            2,
            "Debounce",
            json_string_or_dash(style, &status["debounce"]),
        );
        print_field(
            style,
            2,
            "Minimum interval",
            json_string_or_dash(style, &status["min_interval"]),
        );
        print_field(
            style,
            2,
            "Reconcile interval",
            json_string_or_dash(style, &status["reconcile_interval"]),
        );
        print_field(style, 2, "Poll", json_bool_or_dash(style, &status["poll"]));
        print_field(
            style,
            2,
            "Last event",
            json_string_or_dash(style, &status["last_event_at"]),
        );
        print_field(
            style,
            2,
            "Last refresh reason",
            json_string_or_dash(style, &status["last_refresh_reason"]),
        );
        print_field(
            style,
            2,
            "Last refresh started",
            json_string_or_dash(style, &status["last_refresh_started_at"]),
        );
        print_field(
            style,
            2,
            "Last refresh completed",
            json_string_or_dash(style, &status["last_refresh_completed_at"]),
        );
        print_field(
            style,
            2,
            "Last refresh succeeded",
            json_success_or_dash(style, &status["last_refresh_succeeded"]),
        );
        print_field(
            style,
            2,
            "Last error",
            json_error_or_dash(style, &status["last_error"]),
        );
    } else {
        print_field(
            style,
            2,
            "Status file",
            format!(
                "{} ({})",
                style.muted("not found"),
                style.path(status_path.display())
            ),
        );
    }
    Ok(())
}

/// Writes the LaunchAgent plist used to run `darc refresh --watch --all`.
#[cfg(target_os = "macos")]
pub(crate) fn write_macos_launch_agent(root: &Path, run_at_load: bool) -> Result<PathBuf> {
    let plist_path = macos_launch_agent_path()?;
    write_macos_service_plist(&plist_path, root, run_at_load)
}

/// Writes a runtime-only launchd plist for `service start` without auto-start.
#[cfg(target_os = "macos")]
pub(crate) fn write_macos_runtime_plist(root: &Path) -> Result<PathBuf> {
    let plist_path = macos_runtime_plist_path(root);
    write_macos_service_plist(&plist_path, root, false)
}

/// Writes one launchd plist to the requested path.
#[cfg(target_os = "macos")]
pub(crate) fn write_macos_service_plist(
    plist_path: &Path,
    root: &Path,
    run_at_load: bool,
) -> Result<PathBuf> {
    let parent = plist_path
        .parent()
        .context("LaunchAgent path is missing a parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::create_dir_all(root.join("log"))
        .with_context(|| format!("failed to create {}", root.join("log").display()))?;
    fs::create_dir_all(root.join("run"))
        .with_context(|| format!("failed to create {}", root.join("run").display()))?;

    let executable = env::current_exe().context("failed to resolve current executable")?;
    let plist = macos_launch_agent_plist(root, &executable, run_at_load);
    fs::write(plist_path, plist.as_bytes())
        .with_context(|| format!("failed to write {}", plist_path.display()))?;
    Ok(plist_path.to_path_buf())
}

/// Removes the runtime-only launchd plist when present.
#[cfg(target_os = "macos")]
pub(crate) fn remove_macos_runtime_plist(root: &Path) -> Result<()> {
    let plist_path = macos_runtime_plist_path(root);
    if plist_path.exists() {
        fs::remove_file(&plist_path)
            .with_context(|| format!("failed to remove {}", plist_path.display()))?;
    }
    Ok(())
}

/// Returns the runtime-only LaunchAgent plist path.
#[cfg(target_os = "macos")]
pub(crate) fn macos_runtime_plist_path(root: &Path) -> PathBuf {
    root.join("run")
        .join(format!("{MACOS_SERVICE_LABEL}.plist"))
}

/// Builds the LaunchAgent plist XML.
#[cfg(target_os = "macos")]
pub(crate) fn macos_launch_agent_plist(
    root: &Path,
    executable: &Path,
    run_at_load: bool,
) -> String {
    let stdout = root.join("log/refresh-watch.out.log");
    let stderr = root.join("log/refresh-watch.err.log");
    let run_at_load = if run_at_load { "true" } else { "false" };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{executable}</string>
    <string>refresh</string>
    <string>--watch</string>
    <string>--all</string>
    <string>--root</string>
    <string>{root}</string>
  </array>
  <key>RunAtLoad</key>
  <{run_at_load}/>
  <key>StandardOutPath</key>
  <string>{stdout}</string>
  <key>StandardErrorPath</key>
  <string>{stderr}</string>
</dict>
</plist>
"#,
        label = xml_escape(MACOS_SERVICE_LABEL),
        executable = xml_escape(&executable.display().to_string()),
        root = xml_escape(&root.display().to_string()),
        stdout = xml_escape(&stdout.display().to_string()),
        stderr = xml_escape(&stderr.display().to_string()),
    )
}

/// Returns the per-user LaunchAgent plist path.
#[cfg(target_os = "macos")]
pub(crate) fn macos_launch_agent_path() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{MACOS_SERVICE_LABEL}.plist")))
}

/// Returns the current launchd GUI domain.
#[cfg(target_os = "macos")]
pub(crate) fn macos_launch_domain() -> Result<String> {
    Ok(format!("gui/{}", current_uid()?))
}

/// Returns the launchd service target.
#[cfg(target_os = "macos")]
pub(crate) fn macos_launch_target() -> Result<String> {
    Ok(macos_launch_target_for_domain(&macos_launch_domain()?))
}

/// Returns the launchd service target inside one launchd domain.
#[cfg(target_os = "macos")]
pub(crate) fn macos_launch_target_for_domain(domain: &str) -> String {
    format!("{domain}/{MACOS_SERVICE_LABEL}")
}

/// Returns whether the LaunchAgent is loaded.
#[cfg(target_os = "macos")]
pub(crate) fn macos_service_loaded() -> Result<bool> {
    macos_service_target_loaded(&macos_launch_target()?)
}

/// Returns whether one LaunchAgent target is loaded.
#[cfg(target_os = "macos")]
pub(crate) fn macos_service_target_loaded(target: &str) -> Result<bool> {
    let output = Command::new("launchctl")
        .arg("print")
        .arg(target)
        .output()
        .context("failed to run launchctl print")?;
    Ok(output.status.success())
}

/// Runs `launchctl` and fails on a non-zero exit.
#[cfg(target_os = "macos")]
pub(crate) fn run_launchctl(args: &[String]) -> Result<()> {
    if let Some(failure) = run_launchctl_once(args)? {
        bail!(
            "{}",
            launchctl_failure_message(&failure.args, &failure.stderr)
        );
    }
    Ok(())
}

/// Runs bootstrap with retries for transient launchd restart failures.
#[cfg(target_os = "macos")]
pub(crate) fn run_launchctl_with_bootstrap_retry(args: &[String]) -> Result<()> {
    for attempt in 1..=MACOS_SERVICE_BOOTSTRAP_ATTEMPTS {
        match run_launchctl_once(args)? {
            None => return Ok(()),
            Some(failure)
                if attempt < MACOS_SERVICE_BOOTSTRAP_ATTEMPTS
                    && launchctl_failure_is_retryable_bootstrap(&failure.args, &failure.stderr) =>
            {
                std::thread::sleep(MACOS_SERVICE_RETRY_DELAY);
            }
            Some(failure) => bail!(
                "{}",
                launchctl_failure_message(&failure.args, &failure.stderr)
            ),
        }
    }
    Ok(())
}

/// Runs one launchctl command and returns captured failure details.
#[cfg(target_os = "macos")]
pub(crate) fn run_launchctl_once(args: &[String]) -> Result<Option<LaunchctlFailure>> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .context("failed to run launchctl")?;
    if output.status.success() {
        return Ok(None);
    }
    Ok(Some(LaunchctlFailure {
        args: args.to_vec(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }))
}

/// Captures one non-zero launchctl result.
#[cfg(target_os = "macos")]
pub(crate) struct LaunchctlFailure {
    pub(crate) args: Vec<String>,
    pub(crate) stderr: String,
}

/// Formats one launchctl failure as a structured human-readable error.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn launchctl_failure_message(args: &[String], stderr: &str) -> String {
    let mut message = format!(
        "failed to manage the macOS LaunchAgent\n  Command: {}",
        launchctl_command_display(args)
    );
    let detail = stderr.trim();
    if !detail.is_empty() {
        message.push_str("\n  Detail:");
        for line in detail.lines() {
            message.push_str("\n    ");
            message.push_str(line);
        }
    }
    if let Some(hint) = launchctl_failure_hint(args, detail) {
        message.push_str("\n  Hint: ");
        message.push_str(hint);
    }
    message
}

/// Formats the launchctl command line that failed.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn launchctl_command_display(args: &[String]) -> String {
    let mut command = vec!["launchctl".to_owned()];
    command.extend(args.iter().cloned());
    command.join(" ")
}

/// Returns a contextual hint for known launchctl failure modes.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn launchctl_failure_hint(args: &[String], stderr: &str) -> Option<&'static str> {
    if launchctl_failure_is_retryable_bootstrap(args, stderr) {
        return Some(
            "launchd can report this while replacing a service that was just stopped; Darc retried the start before giving up. Run `darc service status` to confirm the current state.",
        );
    }
    None
}

/// Returns whether one launchctl failure is worth retrying during bootstrap.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn launchctl_failure_is_retryable_bootstrap(args: &[String], stderr: &str) -> bool {
    args.first().is_some_and(|command| command == "bootstrap")
        && stderr.contains("Bootstrap failed: 5")
        && stderr.contains("Input/output error")
}

/// Returns the current numeric user id.
#[cfg(target_os = "macos")]
pub(crate) fn current_uid() -> Result<String> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .context("failed to run id -u")?;
    if !output.status.success() {
        bail!("id -u failed");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Escapes one value for XML text content.
#[cfg(target_os = "macos")]
pub(crate) fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Formats a boolean as a styled yes or no.
#[cfg(target_os = "macos")]
pub(crate) fn yes_no(style: HumanStyle, value: bool) -> String {
    if value {
        style.ok("yes")
    } else {
        style.muted("no")
    }
}

/// Formats one JSON string value or a muted dash.
#[cfg(target_os = "macos")]
pub(crate) fn json_string_or_dash(style: HumanStyle, value: &JsonValue) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| style.muted("-"))
}

/// Formats one JSON boolean value or a muted dash.
#[cfg(target_os = "macos")]
pub(crate) fn json_bool_or_dash(style: HumanStyle, value: &JsonValue) -> String {
    value
        .as_bool()
        .map(|value| value.to_string())
        .unwrap_or_else(|| style.muted("-"))
}

/// Formats a JSON success boolean with state coloring or a muted dash.
#[cfg(target_os = "macos")]
pub(crate) fn json_success_or_dash(style: HumanStyle, value: &JsonValue) -> String {
    match value.as_bool() {
        Some(true) => style.ok("true"),
        Some(false) => style.error("false"),
        None => style.muted("-"),
    }
}

/// Formats a JSON error string with error coloring or a muted dash.
#[cfg(target_os = "macos")]
pub(crate) fn json_error_or_dash(style: HumanStyle, value: &JsonValue) -> String {
    value
        .as_str()
        .map(|value| style.error(value))
        .unwrap_or_else(|| style.muted("-"))
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
