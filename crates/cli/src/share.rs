use std::{
    io::{self, Write},
    path::PathBuf,
};

use anyhow::{Result, bail};
use darc_core::{
    SharePolicy, SharePushReport, ShareState, add_share_recipient, add_share_remote,
    exclude_all_sessions, fetch_share_branch, include_all_sessions, merge_share_branch,
    pull_share_branch, push_share_branch, push_share_branch_with_progress, remove_share_recipient,
    set_session_share_state, set_share_policy, share_config, share_identity, share_key,
    share_remote_display_url, share_status,
};
use darc_core::{SharePushProgress, ShareUploadKind};

use crate::args::{
    RemoteArgs, RemoteCommands, ShareArgs, ShareBranchArgs, ShareCommands, SharePolicyArg,
    ShareRecipientCommands, ShareSessionSelectionArgs,
};
use crate::output::{HumanStyle, stderr_progress_enabled};
use crate::query_commands::provider_arg_to_source_kind;

const SHARE_PROGRESS_BAR_WIDTH: usize = 24;
const CLEAR_ACTIVE_LINE: &str = "\x1b[K";

/// Renders Darc share push progress for interactive terminals.
pub(crate) struct SharePushProgressPrinter<W> {
    writer: W,
    style: HumanStyle,
    enabled: bool,
    active_line: bool,
    step_index: usize,
}

impl SharePushProgressPrinter<io::Stderr> {
    /// Builds one share push progress printer for the current stderr stream.
    pub(crate) fn stderr() -> Self {
        Self::new(
            io::stderr(),
            HumanStyle::stderr(),
            stderr_progress_enabled(),
        )
    }
}

impl<W: Write> SharePushProgressPrinter<W> {
    /// Builds one share push progress printer from resolved terminal facts.
    pub(crate) fn new(writer: W, style: HumanStyle, enabled: bool) -> Self {
        Self {
            writer,
            style,
            enabled,
            active_line: false,
            step_index: 0,
        }
    }

    /// Returns whether this printer will render progress.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Finishes any active progress row before the caller prints another message.
    pub(crate) fn finish(&mut self) {
        if self.enabled {
            let _ = self.finish_active_line();
            let _ = self.writer.flush();
        }
    }

    /// Records one share push progress event, ignoring presentation write failures.
    pub(crate) fn record(&mut self, event: SharePushProgress) {
        if self.enabled {
            let _ = self.write_event(event);
            let _ = self.writer.flush();
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
                self.finish_active_line()?;
                self.step_index = 0;
                writeln!(
                    self.writer,
                    "Pushing {} to {} ({})",
                    self.style.bold(git_branch),
                    self.style.bold(remote_name),
                    remote_url
                )
            }
            SharePushProgress::PreparingCache => self.step("Preparing share cache..."),
            SharePushProgress::FetchingRemote => self.step("Fetching remote branch..."),
            SharePushProgress::HydratingLfs => self.step("Hydrating Git LFS objects..."),
            SharePushProgress::ReadingCache => self.step("Reading cached share artifacts..."),
            SharePushProgress::ReusingPreviousExport {
                exported_turn_count,
                exported_session_count,
            } => self.step(&format!(
                "Reusing previous signed export ({} turns, {} sessions).",
                self.style.count(exported_turn_count),
                self.style.count(exported_session_count)
            )),
            SharePushProgress::BuildingExport { total_turns } => self.step(&format!(
                "Building encrypted export ({} turns)...",
                self.style.count(total_turns)
            )),
            SharePushProgress::ExportingTurns {
                exported_turns,
                total_turns,
            } => self.write_bar("      Exporting", exported_turns, total_turns),
            SharePushProgress::WritingMetadata { object_count } => self.step(&format!(
                "Writing share metadata ({} objects)...",
                self.style.count(object_count)
            )),
            SharePushProgress::Committing => self.step("Committing share artifacts..."),
            SharePushProgress::Uploading { kind } => self.upload_step(kind),
            SharePushProgress::GitProgress { kind: _, message } => {
                self.write_git_progress(&message)
            }
            SharePushProgress::Finished { commit_id } => {
                self.finish_active_line()?;
                writeln!(
                    self.writer,
                    "  {} {}",
                    self.style.ok("done"),
                    self.style.muted(commit_id)
                )?;
                writeln!(self.writer)
            }
            _ => Ok(()),
        }
    }

    /// Writes one numbered push step.
    fn step(&mut self, message: &str) -> io::Result<()> {
        self.finish_active_line()?;
        self.step_index += 1;
        writeln!(
            self.writer,
            "  [{}] {}",
            self.style.count(self.step_index),
            message
        )
    }

    /// Writes one upload phase step.
    fn upload_step(&mut self, kind: ShareUploadKind) -> io::Result<()> {
        let message = match kind {
            ShareUploadKind::Lfs => "Uploading encrypted LFS objects...",
            ShareUploadKind::Git => "Uploading share branch...",
            _ => "Uploading share data...",
        };
        self.step(message)
    }

    /// Writes one in-place progress bar.
    fn write_bar(&mut self, label: &str, current: u64, total: u64) -> io::Result<()> {
        let bar = render_share_progress_bar(current, total, SHARE_PROGRESS_BAR_WIDTH);
        write!(
            self.writer,
            "\r{label} {bar} {}/{}{CLEAR_ACTIVE_LINE}",
            self.style.count(current),
            self.style.count(total)
        )?;
        self.active_line = true;
        Ok(())
    }

    /// Writes one streamed Git progress fragment.
    fn write_git_progress(&mut self, message: &str) -> io::Result<()> {
        if let Some(percent) = git_progress_percent(message) {
            let bar = render_share_progress_bar(u64::from(percent), 100, SHARE_PROGRESS_BAR_WIDTH);
            write!(
                self.writer,
                "\r      Uploading {bar} {}%{CLEAR_ACTIVE_LINE}",
                self.style.count(percent)
            )?;
            self.active_line = true;
        }
        Ok(())
    }

    /// Finishes any in-place progress line before writing regular output.
    fn finish_active_line(&mut self) -> io::Result<()> {
        if self.active_line {
            writeln!(self.writer)?;
            self.active_line = false;
        }
        Ok(())
    }
}

/// Renders a fixed-width ASCII progress bar.
fn render_share_progress_bar(current: u64, total: u64, width: usize) -> String {
    let filled = if total == 0 {
        width
    } else {
        let bounded = current.min(total);
        let width = u64::try_from(width).unwrap_or(u64::MAX);
        let scaled = (u128::from(bounded) * u128::from(width)) / u128::from(total);
        usize::try_from(scaled).unwrap_or(usize::MAX)
    };
    let filled = filled.min(width);
    format!(
        "[{}{}]",
        "#".repeat(filled),
        "-".repeat(width.saturating_sub(filled))
    )
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
    let report = pull_share_branch(Some(args.root), &args.branch, args.remote.as_deref())?;
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
