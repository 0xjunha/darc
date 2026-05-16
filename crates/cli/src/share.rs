use anyhow::{Result, bail};
use darc_core::{
    SharePolicy, ShareState, add_share_recipient, add_share_remote, exclude_all_sessions,
    fetch_share_branch, include_all_sessions, merge_share_branch, pull_share_branch,
    push_share_branch, remove_share_recipient, set_session_share_state, set_share_policy,
    share_config, share_identity, share_key, share_remote_display_url, share_status,
};

use crate::args::{
    RemoteArgs, RemoteCommands, ShareArgs, ShareBranchArgs, ShareCommands, SharePolicyArg,
    ShareRecipientCommands, ShareSessionSelectionArgs,
};
use crate::query_commands::provider_arg_to_source_kind;

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
    let report = push_share_branch(Some(args.root), &args.branch, args.remote.as_deref())?;
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
