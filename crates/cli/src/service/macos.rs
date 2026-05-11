#[cfg(any(target_os = "macos", test))]
use std::io::Write;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::{
    env, fs,
    io::{self, IsTerminal},
    path::PathBuf,
    process::Command,
    time::{Duration, Instant, SystemTime},
};

#[cfg(any(target_os = "macos", test))]
use anyhow::Result;
#[cfg(target_os = "macos")]
use anyhow::{Context, bail};
#[cfg(any(target_os = "macos", test))]
use serde_json::Value as JsonValue;

#[cfg(target_os = "macos")]
use crate::args::ServiceArgs;
#[cfg(target_os = "macos")]
use crate::args::ServiceCommands;
#[cfg(any(target_os = "macos", test))]
use crate::output::HumanStyle;
#[cfg(target_os = "macos")]
use crate::output::{print_field, print_line, print_section};
#[cfg(target_os = "macos")]
use crate::refresh::inspect_refresh_lock;
#[cfg(any(target_os = "macos", test))]
use crate::refresh::{
    DEFAULT_WATCH_RECONCILE_INTERVAL, RefreshLockInfo, RefreshLockSnapshot,
    mark_watch_status_stopped, parse_duration,
};

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

#[cfg(any(target_os = "macos", test))]
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

/// Summarizes the watch process state from launchd and Darc status facts.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MacosWatchProcessState {
    Running,
    Starting,
    StaleLaunchdRunning,
    StaleLaunchdStopped,
    Stopped,
    Unknown,
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
    let (plist_path, needs_kickstart) = if launch_agent_path.exists() {
        (launch_agent_path, false)
    } else {
        (write_macos_runtime_plist(root)?, true)
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
    if needs_kickstart {
        run_launchctl(&macos_service_kickstart_launchctl_args(&target))?;
    }
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
    vec!["kickstart".to_owned(), target.to_owned()]
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
    mark_macos_service_stopped(root)?;
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
    print_field(
        style,
        2,
        "Refresh lock",
        format_refresh_lock_snapshot(style, &inspect_refresh_lock(root)?),
    );
    let status_path = root.join("run/status.json");
    if status_path.exists() {
        let content = fs::read_to_string(&status_path)
            .with_context(|| format!("failed to read {}", status_path.display()))?;
        let status: JsonValue =
            serde_json::from_str(&content).context("failed to parse watch status JSON")?;
        let status_stale = macos_watch_status_age(&status_path)?
            .is_some_and(|age| macos_watch_status_stale(&status, age));
        print_field(
            style,
            2,
            "Watch process",
            format_macos_watch_process_state(
                style,
                macos_watch_process_state(running, Some(&status), status_stale),
            ),
        );
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
            "Watch process",
            format_macos_watch_process_state(
                style,
                macos_watch_process_state(running, None, false),
            ),
        );
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
#[cfg(any(target_os = "macos", test))]
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
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>30</integer>
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
#[cfg(any(target_os = "macos", test))]
pub(crate) fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Marks the latest watch status stopped after an explicit service stop.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn mark_macos_service_stopped(root: &Path) -> Result<()> {
    mark_watch_status_stopped(root)
}

/// Resolves the watch process state from launchd and the latest status file.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn macos_watch_process_state(
    launchd_running: bool,
    status: Option<&JsonValue>,
    status_stale: bool,
) -> MacosWatchProcessState {
    let status_running = status
        .and_then(|status| status.get("running"))
        .and_then(JsonValue::as_bool);
    match (launchd_running, status_running) {
        (true, Some(true)) if status_stale => MacosWatchProcessState::StaleLaunchdRunning,
        (true, Some(true)) => MacosWatchProcessState::Running,
        (true, Some(false)) if status_stale => MacosWatchProcessState::StaleLaunchdRunning,
        (true, Some(false)) => MacosWatchProcessState::Starting,
        (false, Some(true)) => MacosWatchProcessState::StaleLaunchdStopped,
        (false, Some(false)) => MacosWatchProcessState::Stopped,
        (true, None) if status_stale => MacosWatchProcessState::StaleLaunchdRunning,
        (true, None) => MacosWatchProcessState::Unknown,
        (false, None) => MacosWatchProcessState::Stopped,
    }
}

/// Returns whether a watch status file is too old for its reconcile cadence.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn macos_watch_status_stale(status: &JsonValue, age: std::time::Duration) -> bool {
    let interval = status
        .get("reconcile_interval")
        .and_then(JsonValue::as_str)
        .and_then(|value| parse_duration(value).ok())
        .unwrap_or(DEFAULT_WATCH_RECONCILE_INTERVAL);
    let stale_after = interval.checked_mul(2).unwrap_or(interval);
    age > stale_after
}

/// Returns how long ago the watch status file was updated.
#[cfg(target_os = "macos")]
pub(crate) fn macos_watch_status_age(status_path: &Path) -> Result<Option<Duration>> {
    let modified = status_path
        .metadata()
        .with_context(|| format!("failed to stat {}", status_path.display()))?
        .modified()
        .with_context(|| format!("failed to read modified time for {}", status_path.display()))?;
    Ok(SystemTime::now().duration_since(modified).ok())
}

/// Formats the watch process state for service status output.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn format_macos_watch_process_state(
    style: HumanStyle,
    state: MacosWatchProcessState,
) -> String {
    match state {
        MacosWatchProcessState::Running => style.ok("running"),
        MacosWatchProcessState::Starting => style.warn("launchd running; status stopped"),
        MacosWatchProcessState::StaleLaunchdRunning => style.warn("stale; launchd running"),
        MacosWatchProcessState::StaleLaunchdStopped => style.warn("stale; launchd not running"),
        MacosWatchProcessState::Stopped => style.muted("stopped"),
        MacosWatchProcessState::Unknown => style.muted("unknown"),
    }
}

/// Formats the refresh lock state for service status output.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn format_refresh_lock_snapshot(
    style: HumanStyle,
    snapshot: &RefreshLockSnapshot,
) -> String {
    match snapshot {
        RefreshLockSnapshot::Missing => style.muted("none"),
        RefreshLockSnapshot::Available { stale_info: None } => style.ok("available"),
        RefreshLockSnapshot::Available {
            stale_info: Some(info),
        } => style.warn(format!(
            "available; stale holder metadata: {}",
            format_refresh_lock_holder(info)
        )),
        RefreshLockSnapshot::Held { holder: None } => style.warn("held"),
        RefreshLockSnapshot::Held { holder: Some(info) } => {
            style.warn(format!("held by {}", format_refresh_lock_holder(info)))
        }
    }
}

/// Formats one refresh lock holder for diagnostics.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn format_refresh_lock_holder(info: &RefreshLockInfo) -> String {
    format!("pid {} since {}", info.pid, info.started_at)
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
