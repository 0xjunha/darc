use anyhow::{Context, Result, bail};
use darc_agent::{AgentId, RuntimeKind, codex_provider_auth_unsupported_message};
use darc_paths::SourceKind;
use darc_wiki::{ProjectLayout, is_valid_domain_id, load_registry, parse_session_reference};

use super::DigestStartOptions;

/// Validates the new digest start request before any artifact is written.
pub(super) fn validate_digest_start_options(options: &DigestStartOptions) -> Result<()> {
    if options.session_refs.is_empty() {
        bail!("at least one --session-ref is required");
    }
    for session_ref in &options.session_refs {
        validate_session_ref(session_ref)?;
    }
    let agent = AgentId::parse(&options.agent_id).context("agent id is not supported")?;
    if matches!(agent, AgentId::Codex) && options.use_provider_auth {
        bail!(codex_provider_auth_unsupported_message());
    }
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
        if !registry.domains.contains(domain) {
            bail!("target domain `{domain}` is not defined in the project registry");
        }
    }
    Ok(())
}

/// Parses one selected session reference into its typed source kind and session id.
pub(super) fn parse_session_ref(session_ref: &str) -> Result<(SourceKind, &str)> {
    let Some(reference) = parse_session_reference(session_ref) else {
        if !session_ref.contains(':') {
            bail!("session ref `{session_ref}` must use the `<provider>:<session-id>` format");
        }
        bail!("session ref `{session_ref}` must start with `claude:` or `codex:`");
    };
    Ok((reference.provider, reference.session_id))
}
