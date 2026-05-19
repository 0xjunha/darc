use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Result, bail};
use darc_core::{
    SharePolicy, SharePushReport, ShareState, add_share_recipient, add_share_remote,
    exclude_all_sessions, fetch_share_branch, include_all_sessions, merge_share_branch,
    pull_share_branch, pull_share_branch_with_progress, push_share_branch,
    push_share_branch_with_progress, remove_share_recipient, set_session_share_state,
    set_share_policy, share_config, share_identity, share_key, share_remote_display_url,
    share_status,
};
use darc_core::{SharePullProgress, SharePushProgress, ShareUploadKind};

use crate::args::{
    RemoteArgs, RemoteCommands, ShareArgs, ShareBranchArgs, ShareCommands, SharePolicyArg,
    ShareRecipientCommands, ShareSessionSelectionArgs,
};
use crate::output::{HumanStyle, stderr_progress_enabled};
use crate::query_commands::provider_arg_to_source_kind;

const SHARE_PROGRESS_BAR_WIDTH: usize = 24;
const SHARE_PROGRESS_LABEL_WIDTH: usize = 18;
const SHARE_PROGRESS_SPINNER_FRAMES: [&str; 10] =
    ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const CLEAR_ACTIVE_LINE: &str = "\x1b[K";

/// Renders common share progress lines for interactive terminals.
struct ShareProgressOutput<W> {
    writer: W,
    style: HumanStyle,
    enabled: bool,
    live_spinner: bool,
    active_line: bool,
    active_step: Option<ActiveShareStep>,
    step_index: usize,
}

impl<W: Write> ShareProgressOutput<W> {
    /// Builds one common share progress output from resolved terminal facts.
    #[cfg(test)]
    fn new(writer: W, style: HumanStyle, enabled: bool) -> Self {
        Self::new_with_live_spinner(writer, style, enabled, false)
    }

    /// Builds one common share progress output with optional live step animation.
    fn new_with_live_spinner(
        writer: W,
        style: HumanStyle,
        enabled: bool,
        live_spinner: bool,
    ) -> Self {
        Self {
            writer,
            style,
            enabled,
            live_spinner: enabled && live_spinner,
            active_line: false,
            active_step: None,
            step_index: 0,
        }
    }

    /// Returns whether this output will render progress.
    fn enabled(&self) -> bool {
        self.enabled
    }

    /// Flushes the configured progress stream.
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }

    /// Finishes any active progress row before the caller prints another message.
    fn finish(&mut self) -> io::Result<()> {
        if self.enabled {
            self.finish_active_line()?;
            self.flush()?;
        }
        Ok(())
    }

    /// Writes one operation heading and resets numbered steps.
    fn heading(&mut self, message: &str) -> io::Result<()> {
        self.finish_active_line()?;
        self.step_index = 0;
        writeln!(self.writer, "{message}")
    }

    /// Writes one numbered step.
    fn step(&mut self, message: &str) -> io::Result<()> {
        self.finish_active_line()?;
        self.step_index += 1;
        if self.live_spinner {
            let message = message.to_owned();
            write!(
                self.writer,
                "\r{}{}",
                render_share_step_line(
                    self.style,
                    self.step_index,
                    Some(SHARE_PROGRESS_SPINNER_FRAMES[0]),
                    &message
                ),
                CLEAR_ACTIVE_LINE
            )?;
            self.writer.flush()?;
            let spinner = LiveShareStepSpinner::start(self.style, self.step_index, message.clone());
            self.active_step = Some(ActiveShareStep {
                index: self.step_index,
                message,
                spinner: Some(spinner),
            });
            Ok(())
        } else {
            writeln!(
                self.writer,
                "{}",
                render_share_step_line(self.style, self.step_index, None, message)
            )
        }
    }

    /// Writes one in-place progress bar.
    fn write_bar(&mut self, label: &str, current: u64, total: u64) -> io::Result<()> {
        self.finish_active_step()?;
        let bar = render_share_progress_bar(current, total, SHARE_PROGRESS_BAR_WIDTH, self.style);
        let count = render_share_progress_count(current, total, self.style);
        let percent = render_share_progress_percent(current, total, self.style);
        write!(
            self.writer,
            "\r      {label:<SHARE_PROGRESS_LABEL_WIDTH$} {bar} {count} {percent}{CLEAR_ACTIVE_LINE}"
        )?;
        self.active_line = true;
        Ok(())
    }

    /// Writes one in-place percent progress bar.
    fn write_percent_bar(&mut self, label: &str, percent: u8) -> io::Result<()> {
        self.finish_active_step()?;
        let bar = render_share_progress_bar(
            u64::from(percent),
            100,
            SHARE_PROGRESS_BAR_WIDTH,
            self.style,
        );
        let percent = render_share_percent(u64::from(percent), self.style);
        write!(
            self.writer,
            "\r      {label:<SHARE_PROGRESS_LABEL_WIDTH$} {bar} {percent}{CLEAR_ACTIVE_LINE}"
        )?;
        self.active_line = true;
        Ok(())
    }

    /// Finishes any active live step before rendering another progress shape.
    fn finish_active_step(&mut self) -> io::Result<()> {
        if let Some(mut step) = self.active_step.take() {
            if let Some(spinner) = &mut step.spinner {
                spinner.stop();
            }
            writeln!(
                self.writer,
                "\r{}{}",
                render_share_step_line(self.style, step.index, None, &step.message),
                CLEAR_ACTIVE_LINE
            )?;
        }
        Ok(())
    }

    /// Finishes any in-place progress line before writing regular output.
    fn finish_active_line(&mut self) -> io::Result<()> {
        self.finish_active_step()?;
        if self.active_line {
            writeln!(self.writer)?;
            self.active_line = false;
        }
        Ok(())
    }
}

/// Stores one active share step currently animated by a spinner.
struct ActiveShareStep {
    index: usize,
    message: String,
    spinner: Option<LiveShareStepSpinner>,
}

/// Animates one active share step on stderr while blocking work runs.
struct LiveShareStepSpinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl LiveShareStepSpinner {
    /// Starts one live share step spinner on stderr.
    fn start(style: HumanStyle, step_index: usize, message: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut frame_index = 1;
            let mut writer = io::stderr();
            while !worker_stop.load(Ordering::Relaxed) {
                let frame = SHARE_PROGRESS_SPINNER_FRAMES
                    [frame_index % SHARE_PROGRESS_SPINNER_FRAMES.len()];
                let _ = write!(
                    writer,
                    "\r{}{}",
                    render_share_step_line(style, step_index, Some(frame), &message),
                    CLEAR_ACTIVE_LINE
                );
                let _ = writer.flush();
                frame_index += 1;
                thread::sleep(Duration::from_millis(80));
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Stops the spinner thread and waits for it to exit.
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for LiveShareStepSpinner {
    /// Stops the spinner if its owner is dropped before normal completion.
    fn drop(&mut self) {
        self.stop();
    }
}

/// Renders Darc share push progress for interactive terminals.
pub(crate) struct SharePushProgressPrinter<W> {
    output: ShareProgressOutput<W>,
    rendering_session_progress: bool,
}

impl SharePushProgressPrinter<io::Stderr> {
    /// Builds one share push progress printer for the current stderr stream.
    pub(crate) fn stderr() -> Self {
        let enabled = stderr_progress_enabled();
        Self {
            output: ShareProgressOutput::new_with_live_spinner(
                io::stderr(),
                HumanStyle::stderr(),
                enabled,
                true,
            ),
            rendering_session_progress: false,
        }
    }
}

impl<W: Write> SharePushProgressPrinter<W> {
    /// Builds one share push progress printer from resolved terminal facts.
    #[cfg(test)]
    pub(crate) fn new(writer: W, style: HumanStyle, enabled: bool) -> Self {
        Self {
            output: ShareProgressOutput::new(writer, style, enabled),
            rendering_session_progress: false,
        }
    }

    /// Returns whether this printer will render progress.
    pub(crate) fn enabled(&self) -> bool {
        self.output.enabled()
    }

    /// Finishes any active progress row before the caller prints another message.
    pub(crate) fn finish(&mut self) {
        let _ = self.output.finish();
    }

    /// Records one share push progress event, ignoring presentation write failures.
    pub(crate) fn record(&mut self, event: SharePushProgress) {
        if self.output.enabled() {
            let _ = self.write_event(event);
            let _ = self.output.flush();
        }
    }

    /// Writes one share push progress event to the configured stream.
    pub(crate) fn write_event(&mut self, event: SharePushProgress) -> io::Result<()> {
        match event {
            SharePushProgress::Started {
                git_branch,
                remote_name,
                remote_url,
            } => {
                self.rendering_session_progress = false;
                let message = format!(
                    "Pushing {} to {} ({})",
                    self.output.style.bold(git_branch),
                    self.output.style.bold(remote_name),
                    remote_url
                );
                self.output.heading(&message)
            }
            SharePushProgress::PreparingCache => self.output.step("Preparing share cache..."),
            SharePushProgress::FetchingRemote => self.output.step("Fetching remote branch..."),
            SharePushProgress::HydratingLfs => self.output.step("Hydrating Git LFS objects..."),
            SharePushProgress::ReadingCache => {
                self.output.step("Reading cached share artifacts...")
            }
            SharePushProgress::ReusingPreviousExport {
                exported_turn_count,
                exported_session_count,
            } => {
                let message = format!(
                    "Reusing previous signed export ({} turns, {} sessions).",
                    self.output.style.count(exported_turn_count),
                    self.output.style.count(exported_session_count)
                );
                self.output.step(&message)
            }
            SharePushProgress::BuildingExport { total_turns } => {
                let message = format!(
                    "Building encrypted export ({} turns)...",
                    self.output.style.count(total_turns)
                );
                self.output.step(&message)
            }
            SharePushProgress::ExportingTurns {
                exported_turns,
                total_turns,
            } => {
                if self.rendering_session_progress {
                    Ok(())
                } else {
                    self.output
                        .write_bar("Exporting turns", exported_turns, total_turns)
                }
            }
            SharePushProgress::ExportingSessions {
                exported_sessions,
                total_sessions,
            } => {
                self.rendering_session_progress = true;
                self.output
                    .write_bar("Exporting sessions", exported_sessions, total_sessions)
            }
            SharePushProgress::WritingMetadata { object_count } => {
                let message = format!(
                    "Writing share metadata ({} objects)...",
                    self.output.style.count(object_count)
                );
                self.output.step(&message)
            }
            SharePushProgress::Committing => self.output.step("Committing share artifacts..."),
            SharePushProgress::Uploading { kind } => self.upload_step(kind),
            SharePushProgress::GitProgress { kind: _, message } => {
                self.write_git_progress(&message)
            }
            SharePushProgress::Finished { commit_id } => {
                self.output.finish_active_line()?;
                let done = self.output.style.ok("done");
                let commit_id = self.output.style.muted(commit_id);
                writeln!(self.output.writer, "  {} {}", done, commit_id)?;
                writeln!(self.output.writer)
            }
            _ => Ok(()),
        }
    }

    /// Writes one upload phase step.
    fn upload_step(&mut self, kind: ShareUploadKind) -> io::Result<()> {
        let message = match kind {
            ShareUploadKind::Lfs => "Uploading encrypted LFS objects...",
            ShareUploadKind::Git => "Uploading share branch...",
            _ => "Uploading share data...",
        };
        self.output.step(message)
    }

    /// Writes one streamed Git progress fragment.
    fn write_git_progress(&mut self, message: &str) -> io::Result<()> {
        if let Some(percent) = git_progress_percent(message) {
            self.output.write_percent_bar("Uploading", percent)?;
        }
        Ok(())
    }
}

/// Renders Darc share pull progress for interactive terminals.
pub(crate) struct SharePullProgressPrinter<W> {
    output: ShareProgressOutput<W>,
}

impl SharePullProgressPrinter<io::Stderr> {
    /// Builds one share pull progress printer for the current stderr stream.
    pub(crate) fn stderr() -> Self {
        let enabled = stderr_progress_enabled();
        Self {
            output: ShareProgressOutput::new_with_live_spinner(
                io::stderr(),
                HumanStyle::stderr(),
                enabled,
                true,
            ),
        }
    }
}

impl<W: Write> SharePullProgressPrinter<W> {
    /// Builds one share pull progress printer from resolved terminal facts.
    #[cfg(test)]
    pub(crate) fn new(writer: W, style: HumanStyle, enabled: bool) -> Self {
        Self {
            output: ShareProgressOutput::new(writer, style, enabled),
        }
    }

    /// Returns whether this printer will render progress.
    pub(crate) fn enabled(&self) -> bool {
        self.output.enabled()
    }

    /// Finishes any active progress row before the caller prints another message.
    pub(crate) fn finish(&mut self) {
        let _ = self.output.finish();
    }

    /// Records one share pull progress event, ignoring presentation write failures.
    pub(crate) fn record(&mut self, event: SharePullProgress) {
        if self.output.enabled() {
            let _ = self.write_event(event);
            let _ = self.output.flush();
        }
    }

    /// Writes one share pull progress event to the configured stream.
    pub(crate) fn write_event(&mut self, event: SharePullProgress) -> io::Result<()> {
        match event {
            SharePullProgress::Started {
                git_branch,
                remote_name,
                remote_url,
            } => {
                let message = format!(
                    "Pulling {} from {} ({})",
                    self.output.style.bold(git_branch),
                    self.output.style.bold(remote_name),
                    remote_url
                );
                self.output.heading(&message)
            }
            SharePullProgress::PreparingCache => self.output.step("Preparing share cache..."),
            SharePullProgress::FetchingRemote => self.output.step("Fetching remote branch..."),
            SharePullProgress::HydratingLfs => self.output.step("Hydrating Git LFS objects..."),
            SharePullProgress::ReadingCache => {
                self.output.step("Reading cached share artifacts...")
            }
            SharePullProgress::ImportingSessions {
                processed_sessions,
                total_sessions,
            } => self
                .output
                .write_bar("Importing sessions", processed_sessions, total_sessions),
            SharePullProgress::Finished {
                imported_turn_count,
                skipped_turn_count,
                warning_count,
            } => {
                self.output.finish_active_line()?;
                let done = self.output.style.ok("done");
                let imported_turn_count = self.output.style.count(imported_turn_count);
                let skipped_turn_count = self.output.style.count(skipped_turn_count);
                let warning_count = self.output.style.count(warning_count);
                writeln!(
                    self.output.writer,
                    "  {} imported {} turns, skipped {}, warnings {}",
                    done, imported_turn_count, skipped_turn_count, warning_count
                )?;
                writeln!(self.output.writer)
            }
            _ => Ok(()),
        }
    }
}

/// Renders one numbered share step with an optional spinner frame.
pub(crate) fn render_share_step_line(
    style: HumanStyle,
    step_index: usize,
    spinner: Option<&str>,
    message: &str,
) -> String {
    let step = format!("[{}]", style.count(step_index));
    if let Some(spinner) = spinner {
        format!("  {} {step} {message}", style.path(spinner))
    } else {
        format!("  {step} {message}")
    }
}

/// Renders a fixed-width progress bar with a styled terminal variant.
fn render_share_progress_bar(current: u64, total: u64, width: usize, style: HumanStyle) -> String {
    let filled = if total == 0 {
        width
    } else {
        let bounded = current.min(total);
        let width = u64::try_from(width).unwrap_or(u64::MAX);
        let scaled = (u128::from(bounded) * u128::from(width)) / u128::from(total);
        usize::try_from(scaled).unwrap_or(usize::MAX)
    };
    let filled = filled.min(width);
    let empty = width.saturating_sub(filled);
    if style.enabled {
        format!(
            "{}{}",
            style.ok("━".repeat(filled)),
            style.muted("─".repeat(empty))
        )
    } else {
        format!("[{}{}]", "#".repeat(filled), "-".repeat(empty))
    }
}

/// Renders a fixed-width current/total progress count.
fn render_share_progress_count(current: u64, total: u64, style: HumanStyle) -> String {
    let width = current.max(total).max(1).to_string().len();
    style.count(format!("{current:>width$}/{total}"))
}

/// Renders the percentage for one current/total progress pair.
fn render_share_progress_percent(current: u64, total: u64, style: HumanStyle) -> String {
    let percent = current
        .min(total)
        .saturating_mul(100)
        .checked_div(total)
        .unwrap_or(100);
    render_share_percent(percent, style)
}

/// Renders one right-aligned percentage.
fn render_share_percent(percent: u64, style: HumanStyle) -> String {
    style.count(format!("{percent:>3}%"))
}

/// Extracts the last integer percentage from one Git progress fragment.
fn git_progress_percent(message: &str) -> Option<u8> {
    let percent_index = message.rfind('%')?;
    let prefix = &message[..percent_index];
    let digits = prefix
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    let value = digits
        .chars()
        .rev()
        .collect::<String>()
        .parse::<u8>()
        .ok()?;
    (value <= 100).then_some(value)
}

/// Dispatches Darc share remote commands.
pub(crate) fn run_remote(args: RemoteArgs) -> Result<()> {
    match args.command {
        RemoteCommands::Add(add) => {
            add_share_remote(Some(args.root), add.name.clone(), add.url.clone())?;
            println!(
                "Added Darc share remote `{}` -> {}",
                add.name,
                share_remote_display_url(&add.url)
            );
        }
        RemoteCommands::List => {
            let config = share_config(Some(args.root))?;
            if config.remotes.is_empty() {
                println!("No Darc share remotes configured.");
            } else {
                for remote in config.remotes {
                    println!("{}\t{}", remote.name, share_remote_display_url(&remote.url));
                }
            }
        }
    }
    Ok(())
}

/// Dispatches Darc share management commands.
pub(crate) fn run_share(args: ShareArgs) -> Result<()> {
    match args.command {
        ShareCommands::Status => {
            let status = share_status(Some(args.root))?;
            println!(
                "project={} policy={:?} local={} shared={} selected={} included={} excluded={} unset={}",
                status.project_id,
                status.default_policy,
                status.local_session_count,
                status.shared_session_count,
                status.selected_session_count,
                status.included_session_count,
                status.excluded_session_count,
                status.unset_session_count
            );
        }
        ShareCommands::Key => {
            let key = share_key(Some(args.root))?;
            println!("public_key={}", key.public_key);
            println!("key_path={}", key.key_path.display());
        }
        ShareCommands::Identity => {
            let identity = share_identity(Some(args.root))?;
            println!("user_id={}", identity.user_id);
            if let Some(name) = identity.display_name {
                println!("name={name}");
            }
            if let Some(email) = identity.email {
                println!("email={email}");
            }
            println!("public_key={}", identity.public_key);
        }
        ShareCommands::Policy(policy) => {
            set_share_policy(Some(args.root), share_policy_arg_to_policy(policy.policy))?;
            println!("Updated Darc share policy to {:?}.", policy.policy);
        }
        ShareCommands::Include(selection) => {
            run_share_selection(Some(args.root), selection, ShareState::Included)?;
        }
        ShareCommands::Exclude(selection) => {
            run_share_selection(Some(args.root), selection, ShareState::Excluded)?;
        }
        ShareCommands::Recipient(recipient_args) => match recipient_args.command {
            ShareRecipientCommands::Add(value) => {
                add_share_recipient(Some(args.root), value.recipient.clone())?;
                println!("Added Darc share recipient {}", value.recipient);
            }
            ShareRecipientCommands::Remove(value) => {
                let removed = remove_share_recipient(Some(args.root), &value.recipient)?;
                if removed {
                    println!("Removed Darc share recipient {}", value.recipient);
                } else {
                    println!(
                        "Darc share recipient {} was not configured.",
                        value.recipient
                    );
                }
            }
            ShareRecipientCommands::List => {
                let config = share_config(Some(args.root))?;
                if config.recipients.is_empty() {
                    println!("No Darc share recipients configured.");
                } else {
                    for recipient in config.recipients {
                        println!("{}", recipient.recipient);
                    }
                }
            }
        },
    }
    Ok(())
}

/// Runs `darc push` for one Darc share branch.
pub(crate) fn run_push(args: ShareBranchArgs) -> Result<()> {
    let mut progress = SharePushProgressPrinter::stderr();
    run_push_with_progress_printer(
        args,
        &mut progress,
        push_share_branch,
        |root, branch, remote, progress| {
            push_share_branch_with_progress(root, branch, remote, progress)
        },
    )
}

/// Runs `darc push` with injectable push functions for progress wiring tests.
pub(crate) fn run_push_with_progress_printer<W, P, Q>(
    args: ShareBranchArgs,
    progress: &mut SharePushProgressPrinter<W>,
    push_quiet: P,
    push_progress: Q,
) -> Result<()>
where
    W: Write,
    P: FnOnce(Option<PathBuf>, &str, Option<&str>) -> Result<SharePushReport>,
    Q: FnOnce(
        Option<PathBuf>,
        &str,
        Option<&str>,
        &mut dyn FnMut(SharePushProgress),
    ) -> Result<SharePushReport>,
{
    let report = if progress.enabled() {
        let result = {
            let mut record_progress = |event| progress.record(event);
            push_progress(
                Some(args.root.clone()),
                &args.branch,
                args.remote.as_deref(),
                &mut record_progress,
            )
        };
        if result.is_err() {
            progress.finish();
        }
        result?
    } else {
        push_quiet(
            Some(args.root.clone()),
            &args.branch,
            args.remote.as_deref(),
        )?
    };
    println!(
        "Pushed {} to {} ({}) with {} turns across {} sessions in commit {}.",
        report.git_branch,
        report.remote_name,
        report.remote_url,
        report.exported_turn_count,
        report.exported_session_count,
        report.commit_id
    );
    Ok(())
}

/// Runs `darc fetch` for one Darc share branch.
pub(crate) fn run_fetch(args: ShareBranchArgs) -> Result<()> {
    let report = fetch_share_branch(Some(args.root), &args.branch, args.remote.as_deref())?;
    println!(
        "Fetched {} from {} ({}).",
        report.git_branch, report.remote_name, report.remote_url
    );
    Ok(())
}

/// Runs `darc merge` for one Darc share branch.
pub(crate) fn run_merge(args: ShareBranchArgs) -> Result<()> {
    let report = merge_share_branch(Some(args.root), &args.branch, args.remote.as_deref())?;
    print_merge_report(&report);
    Ok(())
}

/// Runs `darc pull` for one Darc share branch.
pub(crate) fn run_pull(args: ShareBranchArgs) -> Result<()> {
    let mut progress = SharePullProgressPrinter::stderr();
    run_pull_with_progress_printer(
        args,
        &mut progress,
        pull_share_branch,
        |root, branch, remote, progress| {
            pull_share_branch_with_progress(root, branch, remote, progress)
        },
    )
}

/// Runs `darc pull` with injectable pull functions for progress wiring tests.
pub(crate) fn run_pull_with_progress_printer<W, P, Q>(
    args: ShareBranchArgs,
    progress: &mut SharePullProgressPrinter<W>,
    pull_quiet: P,
    pull_progress: Q,
) -> Result<()>
where
    W: Write,
    P: FnOnce(Option<PathBuf>, &str, Option<&str>) -> Result<darc_core::SharePullReport>,
    Q: FnOnce(
        Option<PathBuf>,
        &str,
        Option<&str>,
        &mut dyn FnMut(SharePullProgress),
    ) -> Result<darc_core::SharePullReport>,
{
    let report = if progress.enabled() {
        let result = {
            let mut record_progress = |event| progress.record(event);
            pull_progress(
                Some(args.root.clone()),
                &args.branch,
                args.remote.as_deref(),
                &mut record_progress,
            )
        };
        if result.is_err() {
            progress.finish();
        }
        result?
    } else {
        pull_quiet(
            Some(args.root.clone()),
            &args.branch,
            args.remote.as_deref(),
        )?
    };
    println!(
        "Fetched {} from {} ({}).",
        report.fetch.git_branch, report.fetch.remote_name, report.fetch.remote_url
    );
    print_merge_report(&report.merge);
    Ok(())
}

/// Applies one include/exclude command to all sessions or one resolved session.
fn run_share_selection(
    root: Option<std::path::PathBuf>,
    selection: ShareSessionSelectionArgs,
    state: ShareState,
) -> Result<()> {
    if selection.all {
        match state {
            ShareState::Included => include_all_sessions(root)?,
            ShareState::Excluded => exclude_all_sessions(root)?,
            ShareState::Unset => unreachable!("CLI never selects unset share state"),
        }
        println!("Updated Darc share selection for every local session.");
        return Ok(());
    }
    let Some(session_id) = selection.session_id else {
        bail!("session id is required unless --all is passed");
    };
    let updated = set_session_share_state(
        root,
        selection.provider.map(provider_arg_to_source_kind),
        &session_id,
        state,
    )?;
    println!("Updated {updated} session share selection row(s).");
    Ok(())
}

/// Converts one CLI policy argument to the storage policy enum.
fn share_policy_arg_to_policy(policy: SharePolicyArg) -> SharePolicy {
    match policy {
        SharePolicyArg::Manual => SharePolicy::Manual,
        SharePolicyArg::All => SharePolicy::All,
    }
}

/// Prints one merge/import report including skipped warning details.
fn print_merge_report(report: &darc_core::ShareMergeReport) {
    println!(
        "Imported {} turns from {} (skipped {}).",
        report.imported_turn_count, report.git_branch, report.skipped_turn_count
    );
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
}
