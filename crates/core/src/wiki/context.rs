use std::path::Path;

use anyhow::{Context, Result, bail};
use darc_agent::{AgentId, RuntimeKind};
use darc_paths::SourceKind;
use darc_wiki::{ProjectLayout, ProjectRegistry, RunState, is_valid_domain_id, load_registry};

use super::{
    DigestStartOptions,
    models::{DigestContextArtifact, DigestContextSession},
};
use crate::query::{SessionSummary, TurnDetailOptions, query_turn, query_turns};

/// Validates the new digest start request before any artifact is written.
pub(super) fn validate_digest_start_options(options: &DigestStartOptions) -> Result<()> {
    if options.session_refs.is_empty() {
        bail!("at least one --session-ref is required");
    }
    for session_ref in &options.session_refs {
        validate_session_ref(session_ref)?;
    }
    AgentId::parse(&options.agent_id).context("agent id is not supported")?;
    RuntimeKind::parse(&options.runtime).context("runtime is not supported")?;
    if options.model.trim().is_empty() {
        bail!("model must not be empty");
    }
    Ok(())
}

/// Validates one `provider:session-id` reference used to select wiki digest sessions.
pub(super) fn validate_session_ref(session_ref: &str) -> Result<()> {
    let (_, session_id) = parse_session_ref(session_ref)?;
    if session_id.trim().is_empty() {
        bail!("session ref `{session_ref}` must include a non-empty session id");
    }
    Ok(())
}

/// Validates the target categories and domains recorded for one digest request.
pub(super) fn validate_digest_targets(
    layout: &ProjectLayout,
    options: &DigestStartOptions,
) -> Result<()> {
    let registry = load_registry(layout)?;
    for category in &options.target_categories {
        if !registry.categories.contains(category) {
            bail!("target category `{category}` is not defined in the project registry");
        }
    }
    for domain in &options.target_domains {
        if !is_valid_domain_id(domain) {
            bail!("target domain `{domain}` must use lowercase slug format");
        }
    }
    Ok(())
}

/// Parses one selected session reference into its typed source kind and session id.
pub(super) fn parse_session_ref(session_ref: &str) -> Result<(SourceKind, &str)> {
    let Some((provider, session_id)) = session_ref.split_once(':') else {
        bail!("session ref `{session_ref}` must use the `<provider>:<session-id>` format");
    };
    let provider = match provider {
        "claude" => SourceKind::Claude,
        "codex" => SourceKind::Codex,
        _ => bail!("session ref `{session_ref}` must start with `claude:` or `codex:`"),
    };
    Ok((provider, session_id))
}

/// Loads one selected session plus its narrative turn details for the digest context bundle.
pub(super) fn load_selected_session_context<F>(
    root: &Path,
    project_id: &str,
    session_ref: &str,
    session_summaries: &[SessionSummary],
    mut on_turn_progress: F,
) -> Result<DigestContextSession>
where
    F: FnMut() -> Result<()>,
{
    let (provider, session_id) = parse_session_ref(session_ref)?;
    let session = session_summaries
        .iter()
        .find(|session| session.provider == provider && session.session_id == session_id)
        .cloned()
        .with_context(|| format!("selected session `{session_ref}` was not found in the index"))?;
    let turn_summaries = query_turns(Some(root.to_path_buf()), project_id, provider, session_id)
        .with_context(|| format!("failed to load indexed turns for `{session_ref}`"))?;
    let mut turns = Vec::with_capacity(turn_summaries.turns.len());
    for turn in turn_summaries.turns {
        on_turn_progress()?;
        turns.push(
            query_turn(
                Some(root.to_path_buf()),
                project_id,
                provider,
                session_id,
                turn.turn_ordinal,
                TurnDetailOptions {
                    include_raw: false,
                    include_insights: true,
                    narrative: true,
                },
            )
            .with_context(|| {
                format!(
                    "failed to load narrative turn {} for `{session_ref}`",
                    turn.turn_ordinal
                )
            })?,
        );
        on_turn_progress()?;
    }
    Ok(DigestContextSession { session, turns })
}

/// Builds the allowed proposal domain list from persisted registry and run-target hints.
pub(super) fn build_allowed_domains(registry: &ProjectRegistry, state: &RunState) -> Vec<String> {
    let mut domains = registry.domains.clone();
    for domain in &state.target_domains {
        if !domains.contains(domain) {
            domains.push(domain.clone());
        }
    }
    domains
}

/// Builds the exact evidence-reference allowlist from the loaded digest context.
pub(super) fn build_allowed_evidence_refs(context: &DigestContextArtifact) -> Vec<String> {
    context
        .sessions
        .iter()
        .flat_map(|session| {
            session.turns.iter().map(|turn| {
                format!(
                    "{}:{}#{}",
                    session.session.provider.directory_name(),
                    session.session.session_id,
                    turn.turn_ordinal
                )
            })
        })
        .collect()
}
