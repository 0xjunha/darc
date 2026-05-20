use std::{
    io::{self, Write},
    path::PathBuf,
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
use crate::progress::ProgressOutput;
#[cfg(test)]
use crate::progress::render_progress_step_line;
use crate::query_commands::provider_arg_to_source_kind;

/// Renders Darc share push progress for interactive terminals.
pub(crate) struct SharePushProgressPrinter<W> {
    output: ProgressOutput<W>,
    rendering_session_progress: bool,
}

impl SharePushProgressPrinter<io::Stderr> {
    /// Builds one share push progress printer for the current stderr stream.
    pub(crate) fn stderr() -> Self {
        let enabled = stderr_progress_enabled();
        Self {
            output: ProgressOutput::new_with_live_spinner(
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
            output: ProgressOutput::new(writer, style, enabled),
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
        if self.output.enabled() && self.write_event(event).unwrap_or(false) {
            let _ = self.output.flush();
        }
    }

    /// Writes one share push progress event to the configured stream.
    pub(crate) fn write_event(&mut self, event: SharePushProgress) -> io::Result<bool> {
        match event {
            SharePushProgress::Started {
                git_branch,
                remote_name,
                remote_url,
            } => {
                let style = self.output.style();
                self.rendering_session_progress = false;
                let message = format!(
                    "Pushing {} to {} ({})",
                    style.bold(git_branch),
                    style.bold(remote_name),
                    remote_url
                );
                self.output.heading(&message)?;
                Ok(true)
            }
            SharePushProgress::PreparingCache => {
                self.output.step("Preparing share cache...")?;
                Ok(true)
            }
            SharePushProgress::FetchingRemote => {
                self.output.step("Fetching remote branch...")?;
                Ok(true)
            }
            SharePushProgress::HydratingLfs => {
                self.output.step("Hydrating Git LFS objects...")?;
                Ok(true)
            }
            SharePushProgress::ReadingCache => {
                self.output.step("Reading cached share artifacts...")?;
                Ok(true)
            }
            SharePushProgress::ReusingPreviousExport {
                exported_turn_count,
                exported_session_count,
            } => {
                let style = self.output.style();
                let message = format!(
                    "Reusing previous signed export ({} turns, {} sessions).",
                    style.count(exported_turn_count),
                    style.count(exported_session_count)
                );
                self.output.step(&message)?;
                Ok(true)
            }
            SharePushProgress::BuildingExport { total_turns } => {
                let style = self.output.style();
                let message = format!(
                    "Building encrypted export ({} turns)...",
                    style.count(total_turns)
                );
                self.output.step(&message)?;
                Ok(true)
            }
            SharePushProgress::ExportingTurns {
                exported_turns,
                total_turns,
            } => {
                if self.rendering_session_progress {
                    Ok(false)
                } else {
                    self.output
                        .write_throttled_bar("Exporting turns", exported_turns, total_turns)
                }
            }
            SharePushProgress::ExportingSessions {
                exported_sessions,
                total_sessions,
            } => {
                self.rendering_session_progress = true;
                self.output.write_throttled_bar(
                    "Exporting sessions",
                    exported_sessions,
                    total_sessions,
                )
            }
            SharePushProgress::WritingMetadata { object_count } => {
                let style = self.output.style();
                let message = format!(
                    "Writing share metadata ({} objects)...",
                    style.count(object_count)
                );
                self.output.step(&message)?;
                Ok(true)
            }
            SharePushProgress::Committing => {
                self.output.step("Committing share artifacts...")?;
                Ok(true)
            }
            SharePushProgress::Uploading { kind } => self.upload_step(kind),
            SharePushProgress::GitProgress { kind: _, message } => {
                self.write_git_progress(&message)
            }
            SharePushProgress::Finished { commit_id } => {
                self.output.finish_active_line()?;
                let style = self.output.style();
                let done = style.ok("done");
                let commit_id = style.muted(commit_id);
                writeln!(self.output.writer_mut(), "  {} {}", done, commit_id)?;
                writeln!(self.output.writer_mut()).map(|()| true)
            }
            _ => Ok(false),
        }
    }

    /// Writes one upload phase step.
    fn upload_step(&mut self, kind: ShareUploadKind) -> io::Result<bool> {
        let message = match kind {
            ShareUploadKind::Lfs => "Uploading encrypted LFS objects...",
            ShareUploadKind::Git => "Uploading share branch...",
            _ => "Uploading share data...",
        };
        self.output.step(message)?;
        Ok(true)
    }

    /// Writes one streamed Git progress fragment.
    fn write_git_progress(&mut self, message: &str) -> io::Result<bool> {
        if let Some(percent) = git_progress_percent(message) {
            return self
                .output
                .write_throttled_percent_bar("Uploading", percent);
        }
        Ok(false)
    }
}

/// Renders Darc share pull progress for interactive terminals.
pub(crate) struct SharePullProgressPrinter<W> {
    output: ProgressOutput<W>,
}

impl SharePullProgressPrinter<io::Stderr> {
    /// Builds one share pull progress printer for the current stderr stream.
    pub(crate) fn stderr() -> Self {
        let enabled = stderr_progress_enabled();
        Self {
            output: ProgressOutput::new_with_live_spinner(
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
            output: ProgressOutput::new(writer, style, enabled),
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
        if self.output.enabled() && self.write_event(event).unwrap_or(false) {
            let _ = self.output.flush();
        }
    }

    /// Writes one share pull progress event to the configured stream.
    pub(crate) fn write_event(&mut self, event: SharePullProgress) -> io::Result<bool> {
        match event {
            SharePullProgress::Started {
                git_branch,
                remote_name,
                remote_url,
            } => {
                let style = self.output.style();
                let message = format!(
                    "Pulling {} from {} ({})",
                    style.bold(git_branch),
                    style.bold(remote_name),
                    remote_url
                );
                self.output.heading(&message)?;
                Ok(true)
            }
            SharePullProgress::PreparingCache => {
                self.output.step("Preparing share cache...")?;
                Ok(true)
            }
            SharePullProgress::FetchingRemote => {
                self.output.step("Fetching remote branch...")?;
                Ok(true)
            }
            SharePullProgress::HydratingLfs => {
                self.output.step("Hydrating Git LFS objects...")?;
                Ok(true)
            }
            SharePullProgress::ReadingCache => {
                self.output.step("Reading cached share artifacts...")?;
                Ok(true)
            }
            SharePullProgress::ImportingSessions {
                processed_sessions,
                total_sessions,
            } => self.output.write_throttled_bar(
                "Importing sessions",
                processed_sessions,
                total_sessions,
            ),
            SharePullProgress::Finished {
                imported_turn_count,
                skipped_turn_count,
                warning_count,
            } => {
                self.output.finish_active_line()?;
                let style = self.output.style();
                let done = style.ok("done");
                let imported_turn_count = style.count(imported_turn_count);
                let skipped_turn_count = style.count(skipped_turn_count);
                let warning_count = style.count(warning_count);
                writeln!(
                    self.output.writer_mut(),
                    "  {} imported {} turns, skipped {}, warnings {}",
                    done,
                    imported_turn_count,
                    skipped_turn_count,
                    warning_count
                )?;
                writeln!(self.output.writer_mut()).map(|()| true)
            }
            _ => Ok(false),
        }
    }
}

/// Renders one numbered share step with an optional spinner frame.
#[cfg(test)]
pub(crate) fn render_share_step_line(
    style: HumanStyle,
    step_index: usize,
    spinner: Option<&str>,
    message: &str,
) -> String {
    render_progress_step_line(style, step_index, spinner, message)
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
