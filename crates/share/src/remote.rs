use super::*;

/// Resolves the remote URL for one share operation.
pub(crate) fn resolve_remote(
    context: &ShareProjectContext,
    settings: &ShareSettings,
    remote_name: Option<&str>,
) -> Result<ResolvedRemote> {
    if let Some(remote_name) = remote_name {
        let remote = settings
            .remotes
            .iter()
            .find(|remote| remote.name == remote_name)
            .with_context(|| format!("Darc share remote `{remote_name}` is not configured"))?;
        return resolved_remote(&context.local_path, &remote.name, &remote.url);
    }
    if let Some(url) = context.git_upstream.clone() {
        return resolved_remote(&context.local_path, DEFAULT_REMOTE_NAME, &url);
    }
    let url = origin_configured_remote_url(&context.local_path)
        .context("active project has no git_upstream and no origin remote")?;
    resolved_remote(&context.local_path, DEFAULT_REMOTE_NAME, &url)
}

/// Builds one remote target from a configured URL and one rewritten URL lookup.
pub(crate) fn resolved_remote(
    project_path: &Path,
    name: &str,
    url: &str,
) -> Result<ResolvedRemote> {
    validate_share_remote_url(url)?;
    let resolved_url = resolved_remote_url(project_path, url)?;
    let cache_url = cache_remote_url_from_resolved(&resolved_url)?;
    Ok(ResolvedRemote {
        name: name.to_owned(),
        display_url: sanitize_git_url_for_display(url),
        resolved_url,
        cache_url,
        #[cfg(test)]
        url: url.to_owned(),
    })
}

/// Returns the canonical shared-project key for one active project.
pub(crate) fn project_key(context: &ShareProjectContext) -> Result<String> {
    let url = if let Some(url) = context.git_upstream.as_deref() {
        resolved_remote_url(&context.local_path, url)?
    } else {
        origin_effective_remote_url(&context.local_path)
            .context("active project has no git_upstream and no origin remote")?
    };
    Ok(format!("git:{}", normalize_git_url(&url)?))
}

/// Normalizes one Git URL enough for Darc project matching.
pub(crate) fn normalize_git_url(url: &str) -> Result<String> {
    let trimmed = strip_url_query_fragment(url.trim())
        .trim_end_matches('/')
        .trim_end_matches(".git");
    if let Some(normalized) = normalize_scp_like_git_url(trimmed) {
        return Ok(normalized);
    }
    if let Some(normalized) = normalize_scheme_git_url(trimmed, "ssh://", "https") {
        return Ok(normalized);
    }
    if let Some(normalized) = normalize_scheme_git_url(trimmed, "https://", "https") {
        return Ok(normalized);
    }
    if let Some(normalized) = normalize_scheme_git_url(trimmed, "http://", "http") {
        return Ok(normalized);
    }
    bail!(
        "Darc share project keys require an ssh, https, or http Git remote; refusing to publish unsupported or local remote `{}` in visible share metadata",
        sanitize_git_url_for_display(trimmed)
    )
}

/// Removes URL query and fragment suffixes before URLs become visible metadata.
pub(crate) fn strip_url_query_fragment(url: &str) -> &str {
    url.find(['?', '#']).map_or(url, |index| &url[..index])
}

/// Normalizes one SSH scp-like Git URL.
pub(crate) fn normalize_scp_like_git_url(url: &str) -> Option<String> {
    if url.contains("://") {
        return None;
    }
    let (user_host, path) = url.split_once(':')?;
    let host = user_host
        .rsplit_once('@')
        .map_or(user_host, |(_, host)| host);
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!(
        "https://{}/{}",
        host.to_ascii_lowercase(),
        path.trim_start_matches('/')
    ))
}

/// Normalizes one scheme Git URL while removing credential userinfo.
pub(crate) fn normalize_scheme_git_url(
    url: &str,
    input_scheme: &str,
    output_scheme: &str,
) -> Option<String> {
    if !url
        .get(..input_scheme.len())?
        .eq_ignore_ascii_case(input_scheme)
    {
        return None;
    }
    let rest = &url[input_scheme.len()..];
    let (authority, path) = rest.split_once('/')?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .to_ascii_lowercase();
    Some(format!(
        "{output_scheme}://{host}/{}",
        path.trim_start_matches('/')
    ))
}

/// Returns a remote URL suitable for terminal output.
pub fn sanitize_git_url_for_display(url: &str) -> String {
    let trimmed = strip_url_query_fragment(url.trim());
    if let Some((scheme, rest)) = trimmed.split_once("://") {
        let (authority, path) = rest
            .split_once('/')
            .map_or((rest, None), |(authority, path)| (authority, Some(path)));
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        return path.map_or_else(
            || format!("{scheme}://{host}"),
            |path| format!("{scheme}://{host}/{path}"),
        );
    }
    if let Some((user_host, path)) = trimmed.split_once(':')
        && !trimmed.contains("://")
        && let Some((user, host)) = user_host.rsplit_once('@')
    {
        return format!("{user}@{host}:{path}");
    }
    trimmed.to_owned()
}

/// Rejects share remote URLs that would persist credential-bearing URL parts.
pub fn validate_share_remote_url(url: &str) -> Result<()> {
    let trimmed = url.trim();
    let display_url = sanitize_git_url_for_display(trimmed);
    if trimmed.contains(['?', '#']) {
        bail!(
            "share remote URL `{display_url}` must not include query strings or fragments; configure Git credentials outside the URL"
        );
    }
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return Ok(());
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let scheme = scheme.to_ascii_lowercase();
    let userinfo = authority.rsplit_once('@').map(|(userinfo, _)| userinfo);
    if userinfo.is_some_and(|userinfo| {
        matches!(scheme.as_str(), "http" | "https") || scheme != "ssh" || userinfo.contains(':')
    }) {
        bail!(
            "share remote URL `{display_url}` must not include URL credentials; configure Git credentials outside the URL"
        );
    }
    Ok(())
}
