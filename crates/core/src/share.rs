use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result, bail};
pub use darc_share::{
    ShareFetchReport, ShareIdentity, ShareKeyInfo, ShareMergeReport, SharePullReport,
    SharePushProgress, SharePushReport, ShareUploadKind,
};
use darc_share::{ShareProjectContext, ShareRecipient, ShareRemote, ShareSettings};
pub use darc_store::{SharePolicy, ShareState, ShareStatus};

use crate::{
    active_project::load_active_project,
    config::{ShareConfig, ShareRecipientConfig, ShareRemoteConfig, SharedConfig},
    constants::CONFIG_FILE_NAME,
    project::{load_normalized_shared_config, write_shared_config},
    query::{resolve_query_project, resolve_query_session_for_project},
};

/// Stores one active Darc project resolved for share operations.
#[derive(Debug, Clone)]
struct ResolvedShareProject {
    config: SharedConfig,
    context: ShareProjectContext,
}

/// Stores the configured share remotes and recipients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareConfigReport {
    pub remotes: Vec<ShareRemoteConfig>,
    pub recipients: Vec<ShareRecipientConfig>,
}

/// Stores the workspace config needed by share config commands.
struct ResolvedShareConfig {
    config: SharedConfig,
    config_path: PathBuf,
}

/// Returns the sharing status for the active project.
pub fn share_status(root: Option<PathBuf>) -> Result<ShareStatus> {
    let resolved = resolve_share_project(root)?;
    darc_share::share_status(&resolved.context)
}

/// Ensures the local share key exists and returns its public key.
pub fn share_key(root: Option<PathBuf>) -> Result<ShareKeyInfo> {
    let resolved_root = resolve_root_path(root.unwrap_or_else(crate::default_root_path));
    darc_share::ensure_share_key(&resolved_root)
}

/// Returns the local share identity derived from Git config and the Darc share key.
pub fn share_identity(root: Option<PathBuf>) -> Result<ShareIdentity> {
    let resolved = resolve_share_project(root)?;
    darc_share::local_share_identity(&resolved.context)
}

/// Updates the active project's default share policy.
pub fn set_share_policy(root: Option<PathBuf>, policy: SharePolicy) -> Result<()> {
    let resolved = resolve_share_project(root)?;
    darc_share::update_share_policy(&resolved.context, policy)
}

/// Includes all local sessions in the active project for sharing.
pub fn include_all_sessions(root: Option<PathBuf>) -> Result<()> {
    let resolved = resolve_share_project(root)?;
    darc_share::include_all_sessions(&resolved.context)
}

/// Excludes all local sessions in the active project from sharing.
pub fn exclude_all_sessions(root: Option<PathBuf>) -> Result<()> {
    let resolved = resolve_share_project(root)?;
    darc_share::exclude_all_sessions(&resolved.context)
}

/// Updates one session's explicit share state.
pub fn set_session_share_state(
    root: Option<PathBuf>,
    provider: Option<crate::SourceKind>,
    session_id: &str,
    state: ShareState,
) -> Result<usize> {
    let resolved = resolve_share_project(root.clone())?;
    let query_project = resolve_query_project(root, Some(&resolved.context.project_id))?;
    let session = resolve_query_session_for_project(&query_project, provider, session_id)?;
    darc_share::update_session_share_state(
        &resolved.context,
        session.provider,
        &session.session_id,
        state,
    )
}

/// Lists the configured share remotes and recipients.
pub fn share_config(root: Option<PathBuf>) -> Result<ShareConfigReport> {
    let resolved = resolve_share_config(root)?;
    Ok(ShareConfigReport {
        remotes: resolved.config.share.remotes,
        recipients: resolved.config.share.recipients,
    })
}

/// Returns one share remote URL suitable for terminal display.
pub fn share_remote_display_url(url: &str) -> String {
    darc_share::sanitize_git_url_for_display(url)
}

/// Adds or updates one named Darc share remote in config.toml.
pub fn add_share_remote(root: Option<PathBuf>, name: String, url: String) -> Result<()> {
    if name.trim().is_empty() {
        bail!("remote name must not be empty");
    }
    if url.trim().is_empty() {
        bail!("remote URL must not be empty");
    }
    let mut resolved = resolve_share_config(root)?;
    let mut settings = share_settings_from_config(&resolved.config.share);
    darc_share::upsert_remote(
        &mut settings,
        ShareRemote {
            name: name.trim().to_owned(),
            url: url.trim().to_owned(),
        },
    );
    resolved.config.share = share_config_from_settings(settings);
    write_shared_config(&resolved.config_path, &resolved.config)
}

/// Adds one age recipient to config.toml.
pub fn add_share_recipient(root: Option<PathBuf>, recipient: String) -> Result<()> {
    let recipient = recipient.trim();
    if recipient.is_empty() {
        bail!("recipient must not be empty");
    }
    darc_share::validate_share_recipient(recipient)?;
    let mut resolved = resolve_share_config(root)?;
    let mut settings = share_settings_from_config(&resolved.config.share);
    darc_share::add_recipient(
        &mut settings,
        ShareRecipient {
            recipient: recipient.to_owned(),
        },
    );
    resolved.config.share = share_config_from_settings(settings);
    write_shared_config(&resolved.config_path, &resolved.config)
}

/// Removes one age recipient from config.toml.
pub fn remove_share_recipient(root: Option<PathBuf>, recipient: &str) -> Result<bool> {
    let mut resolved = resolve_share_config(root)?;
    let mut settings = share_settings_from_config(&resolved.config.share);
    let removed = darc_share::remove_recipient(&mut settings, recipient);
    resolved.config.share = share_config_from_settings(settings);
    write_shared_config(&resolved.config_path, &resolved.config)?;
    Ok(removed)
}

/// Pushes the active project's selected shared sessions to one Darc share branch.
pub fn push_share_branch(
    root: Option<PathBuf>,
    branch: &str,
    remote_name: Option<&str>,
) -> Result<SharePushReport> {
    let resolved = resolve_share_project(root)?;
    let settings = share_settings_from_config(&resolved.config.share);
    darc_share::push_share_branch(&resolved.context, &settings, branch, remote_name)
}

/// Pushes the active project's selected shared sessions while emitting progress events.
pub fn push_share_branch_with_progress<F>(
    root: Option<PathBuf>,
    branch: &str,
    remote_name: Option<&str>,
    progress: F,
) -> Result<SharePushReport>
where
    F: FnMut(SharePushProgress),
{
    let resolved = resolve_share_project(root)?;
    let settings = share_settings_from_config(&resolved.config.share);
    darc_share::push_share_branch_with_progress(
        &resolved.context,
        &settings,
        branch,
        remote_name,
        progress,
    )
}

/// Fetches one Darc share branch into the local share cache.
pub fn fetch_share_branch(
    root: Option<PathBuf>,
    branch: &str,
    remote_name: Option<&str>,
) -> Result<ShareFetchReport> {
    let resolved = resolve_share_project(root)?;
    let settings = share_settings_from_config(&resolved.config.share);
    darc_share::fetch_share_branch(&resolved.context, &settings, branch, remote_name)
}

/// Imports the fetched state of one Darc share branch into the local index.
pub fn merge_share_branch(
    root: Option<PathBuf>,
    branch: &str,
    remote_name: Option<&str>,
) -> Result<ShareMergeReport> {
    let resolved = resolve_share_project(root)?;
    let settings = share_settings_from_config(&resolved.config.share);
    darc_share::merge_share_branch(&resolved.context, &settings, branch, remote_name)
}

/// Fetches and imports one Darc share branch.
pub fn pull_share_branch(
    root: Option<PathBuf>,
    branch: &str,
    remote_name: Option<&str>,
) -> Result<SharePullReport> {
    let resolved = resolve_share_project(root)?;
    let settings = share_settings_from_config(&resolved.config.share);
    darc_share::pull_share_branch(&resolved.context, &settings, branch, remote_name)
}

/// Resolves the active project and adapts it to the share crate context.
fn resolve_share_project(root: Option<PathBuf>) -> Result<ResolvedShareProject> {
    let requested_root = root.unwrap_or_else(crate::default_root_path);
    let resolved_root = resolve_root_path(requested_root);
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    let active = load_active_project(&current_dir, &resolved_root)?;
    Ok(ResolvedShareProject {
        config: active.config,
        context: ShareProjectContext {
            root: resolved_root.clone(),
            index_db_path: resolved_root.join(darc_store::INDEX_DB_FILE_NAME),
            project_id: active.project.id,
            project_name: active.project.name,
            local_path: active.project.local_path,
            git_upstream: active.project.git_upstream,
        },
    })
}

/// Resolves the workspace config without requiring an active project.
fn resolve_share_config(root: Option<PathBuf>) -> Result<ResolvedShareConfig> {
    let requested_root = root.unwrap_or_else(crate::default_root_path);
    let resolved_root = resolve_root_path(requested_root);
    let config_path = resolved_root.join(CONFIG_FILE_NAME);
    if !config_path.exists() {
        bail!(
            "shared config not found at {}\nrun `darc init --root {}` from a project root first",
            config_path.display(),
            resolved_root.display()
        );
    }
    Ok(ResolvedShareConfig {
        config: load_normalized_shared_config(&config_path)?,
        config_path,
    })
}

/// Converts persisted config into the share crate's config model.
fn share_settings_from_config(config: &ShareConfig) -> ShareSettings {
    ShareSettings {
        remotes: config
            .remotes
            .iter()
            .map(|remote| ShareRemote {
                name: remote.name.clone(),
                url: remote.url.clone(),
            })
            .collect(),
        recipients: config
            .recipients
            .iter()
            .map(|recipient| ShareRecipient {
                recipient: recipient.recipient.clone(),
            })
            .collect(),
    }
}

/// Converts share crate settings back into persisted config.
fn share_config_from_settings(settings: ShareSettings) -> ShareConfig {
    ShareConfig {
        remotes: settings
            .remotes
            .into_iter()
            .map(|remote| ShareRemoteConfig {
                name: remote.name,
                url: remote.url,
            })
            .collect(),
        recipients: settings
            .recipients
            .into_iter()
            .map(|recipient| ShareRecipientConfig {
                recipient: recipient.recipient,
            })
            .collect(),
    }
}

/// Returns the best-effort resolved filesystem path for one Darc root input.
fn resolve_root_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return fs::canonicalize(&path).unwrap_or(path);
    }

    let joined = env::current_dir()
        .map(|current_dir| current_dir.join(&path))
        .unwrap_or(path);
    fs::canonicalize(&joined).unwrap_or(joined)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::config::SourcesConfig;

    /// Creates one unique temporary Darc root for share config tests.
    fn unique_test_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("darc-core-{label}-{}-{nanos}", std::process::id()))
    }

    /// Writes a minimal workspace config with no registered projects.
    fn write_empty_config(root: &std::path::Path) -> Result<()> {
        fs::create_dir_all(root)?;
        write_shared_config(
            &root.join(CONFIG_FILE_NAME),
            &SharedConfig::new(root.to_path_buf(), Vec::new(), SourcesConfig::default()),
        )
    }

    #[test]
    fn workspace_share_config_commands_do_not_require_active_project() -> Result<()> {
        let root = unique_test_root("share-config-no-active-project");
        write_empty_config(&root)?;
        let recipient = darc_share::ensure_share_key(&root)?.public_key;

        add_share_remote(
            Some(root.clone()),
            "team".to_owned(),
            "https://example.invalid/team/darc-share.git".to_owned(),
        )?;
        add_share_recipient(Some(root.clone()), recipient.clone())?;
        let report = share_config(Some(root.clone()))?;
        let removed = remove_share_recipient(Some(root.clone()), &recipient)?;

        assert_eq!(report.remotes.len(), 1);
        assert_eq!(report.remotes[0].name, "team");
        assert_eq!(report.recipients.len(), 1);
        assert_eq!(report.recipients[0].recipient, recipient);
        assert!(removed);

        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn add_share_recipient_rejects_invalid_age_recipient() -> Result<()> {
        let root = unique_test_root("share-config-invalid-recipient");
        write_empty_config(&root)?;

        let error = add_share_recipient(Some(root.clone()), "not-an-age-recipient".to_owned())
            .expect_err("invalid recipient should be rejected");
        let report = share_config(Some(root.clone()))?;

        assert!(error.to_string().contains("invalid"));
        assert!(report.recipients.is_empty());

        fs::remove_dir_all(root)?;
        Ok(())
    }
}
