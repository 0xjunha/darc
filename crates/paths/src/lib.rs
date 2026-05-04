use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Identifies which upstream tool produced one archived session tree.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    #[serde(alias = "Claude")]
    Claude,
    #[serde(alias = "Codex")]
    Codex,
}

impl SourceKind {
    /// Returns the stable directory name used for archived sessions.
    pub fn directory_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// Returns a human-readable name for the source kind.
    pub fn title(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }
}

/// Returns whether one project id is safe to use as a path component.
pub fn is_valid_project_id(project_id: &str) -> bool {
    !project_id.is_empty()
        && project_id
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

/// Returns one concrete file-access path after trimming structured path syntax.
pub fn normalize_access_path_candidate(path: &str) -> Option<String> {
    let path = trim_access_path_candidate(path);
    is_concrete_access_path_literal(path).then(|| path.to_owned())
}

/// Returns one concrete shell-derived path after dropping shell syntax artifacts.
pub fn normalize_shell_access_path_candidate(path: &str) -> Option<String> {
    let path = trim_access_path_candidate(path);
    (is_concrete_access_path_literal(path) && is_shell_concrete_access_path_literal(path))
        .then(|| path.to_owned())
}

/// Returns whether one extracted access path is concrete enough for analytics.
pub fn is_concrete_access_path(path: &str) -> bool {
    normalize_access_path_candidate(path).is_some()
}

/// Trims quoting and whitespace from one candidate path literal.
fn trim_access_path_candidate(path: &str) -> &str {
    path.trim().trim_matches(['"', '\'']).trim()
}

/// Returns whether one trimmed path candidate is a concrete file target.
fn is_concrete_access_path_literal(path: &str) -> bool {
    !path.is_empty() && !matches!(path, "." | ".." | "-" | "/dev/null")
}

/// Returns whether one trimmed shell path candidate is not command syntax.
fn is_shell_concrete_access_path_literal(path: &str) -> bool {
    !matches!(
        path,
        "EOF" | "PATCH" | "[" | "]" | "{" | "}" | "(" | ")" | "!" | "=" | "!=" | "==" | "<" | ">"
    ) && !((path.starts_with('-') || path.starts_with('(')) && !path.contains('/'))
        && !path_looks_fd_duplication_fragment(path)
        && !path.contains('$')
        && !path.contains('*')
        && !path.contains('?')
        && !path_looks_shell_redirection(path)
}

/// Returns whether one token is shell redirection syntax instead of a path.
fn path_looks_shell_redirection(path: &str) -> bool {
    let body = path.trim_start_matches(|ch: char| ch.is_ascii_digit());
    if body.is_empty() {
        return false;
    }
    matches!(body, "<<" | "<<-" | "<<<")
        || body.starts_with("<<")
        || body.starts_with("&>")
        || body.starts_with('>')
        || body.starts_with("<>")
        || body.starts_with('<')
}

/// Returns whether one token is an fd duplication fragment with shell punctuation.
fn path_looks_fd_duplication_fragment(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('&') else {
        return false;
    };
    let rest = rest.trim_end_matches(')');
    !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit())
}

/// Returns the current UTC timestamp formatted in Darc's stable ISO 8601 shape.
pub fn current_utc_timestamp() -> String {
    current_utc_timestamp_at(SystemTime::now())
}

/// Returns one UTC timestamp for the provided system time.
pub fn current_utc_timestamp_at(timestamp: SystemTime) -> String {
    let duration = timestamp.duration_since(UNIX_EPOCH).unwrap_or_default();
    let total_seconds = duration.as_secs();
    let days = i64::try_from(total_seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_of_day = total_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Parses one Darc UTC ISO 8601 timestamp back into `SystemTime`.
pub fn parse_utc_timestamp(value: &str) -> Option<SystemTime> {
    let value = value.strip_suffix('Z')?;
    let (date, time) = value.split_once('T')?;
    let mut date_parts = date.split('-');
    let year = date_parts.next()?.parse::<i64>().ok()?;
    let month = date_parts.next()?.parse::<u32>().ok()?;
    let day = date_parts.next()?.parse::<u32>().ok()?;
    if date_parts.next().is_some() {
        return None;
    }

    let mut time_parts = time.split(':');
    let hour = time_parts.next()?.parse::<u64>().ok()?;
    let minute = time_parts.next()?.parse::<u64>().ok()?;
    let second = time_parts.next()?.parse::<u64>().ok()?;
    if time_parts.next().is_some() {
        return None;
    }

    let days = days_from_civil(year, month, day)?;
    let days = u64::try_from(days).ok()?;
    let seconds_of_day = hour
        .checked_mul(3_600)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)?;
    Some(UNIX_EPOCH + Duration::from_secs(days.checked_mul(86_400)?.checked_add(seconds_of_day)?))
}

/// Resolves one query time bound from canonical UTC text or `<days>d` shorthand.
pub fn resolve_query_time_bound(value: &str) -> std::result::Result<String, String> {
    resolve_query_time_bound_at(value, SystemTime::now())
}

/// Resolves one query time bound against one fixed clock for deterministic tests.
pub fn resolve_query_time_bound_at(
    value: &str,
    now: SystemTime,
) -> std::result::Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("time bound must not be empty".to_owned());
    }
    if let Some(days) = value.strip_suffix('d') {
        return resolve_relative_query_days(days, now);
    }
    if parse_utc_timestamp(value).is_some() {
        return Ok(value.to_owned());
    }
    Err(format!(
        "time bound `{value}` must be a UTC ISO-8601 timestamp like `2026-04-07T00:00:00Z` or `<days>d` like `5d`"
    ))
}

/// Resolves one `<days>d` shorthand into a canonical UTC timestamp string.
fn resolve_relative_query_days(days: &str, now: SystemTime) -> std::result::Result<String, String> {
    let days = days
        .parse::<u64>()
        .map_err(|_| format!("invalid day shorthand `{days}d`"))?;
    let delta_seconds = days
        .checked_mul(86_400)
        .ok_or_else(|| "relative day shorthand overflowed".to_owned())?;
    let unix_seconds = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(delta_seconds);
    Ok(current_utc_timestamp_at(
        UNIX_EPOCH + Duration::from_secs(unix_seconds),
    ))
}

/// Encodes a project path using Claude's directory naming rule.
pub fn encode_path_for_claude(path: &Path) -> String {
    path.to_string_lossy().replace('/', "-")
}

/// Resolves the current project root, preferring the git toplevel when available.
pub fn current_project_root(current_dir: &Path) -> Result<PathBuf> {
    let root = try_git_output(current_dir, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or_else(|| current_dir.to_path_buf());
    fs::canonicalize(&root).with_context(|| format!("unable to canonicalize {}", root.display()))
}

/// Normalizes a project path using canonicalization when possible.
pub fn normalize_project_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_path_textually(path))
}

/// Builds the full project path set from the current root, live worktrees, and known paths.
pub fn project_path_set(current_root: &Path, known_paths: &[PathBuf]) -> Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::new();
    paths.insert(normalize_project_path(current_root));
    for path in git_worktree_paths(current_root)? {
        paths.insert(path);
    }
    for path in known_paths {
        paths.insert(normalize_project_path(path));
    }
    Ok(paths)
}

/// Normalizes stored known paths while excluding the primary project root.
pub fn normalized_known_paths(current_root: &Path, known_paths: &[PathBuf]) -> BTreeSet<PathBuf> {
    let current_root = normalize_project_path(current_root);
    known_paths
        .iter()
        .map(|path| normalize_project_path(path))
        .filter(|path| path != &current_root)
        .collect()
}

/// Returns the seed list stored into `known_paths` for a freshly initialized project.
pub fn seed_known_paths(current_root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = project_path_set(current_root, &[])?;
    paths.remove(&normalize_project_path(current_root));
    Ok(paths.into_iter().collect())
}

/// Lists the current live git worktree paths for a repository or worktree root.
pub fn git_worktree_paths(project_root: &Path) -> Result<Vec<PathBuf>> {
    if !project_root.exists() {
        return Ok(Vec::new());
    }

    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_root)
        .output()
        .with_context(|| {
            format!(
                "failed to run git worktree list in {}",
                project_root.display()
            )
        })?;
    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout =
        String::from_utf8(output.stdout).context("git worktree list returned non-UTF-8")?;
    Ok(parse_git_worktree_output(&stdout)
        .into_iter()
        .map(|path| normalize_project_path(&path))
        .collect())
}

/// Executes a git command and returns trimmed UTF-8 stdout on success.
pub fn try_git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    Some(value.to_owned())
}

/// Normalizes a path textually when canonicalization is not possible.
fn normalize_path_textually(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    let absolute = path.is_absolute();

    if absolute {
        normalized.push(Path::new("/"));
    }

    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::Normal(segment) => normalized.push(segment),
            std::path::Component::ParentDir => {
                if !normalized.pop() && !absolute {
                    normalized.push("..");
                }
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        if absolute {
            PathBuf::from("/")
        } else {
            PathBuf::from(".")
        }
    } else {
        normalized
    }
}

/// Converts one Unix-day count into a UTC civil date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// Converts one civil UTC date into the Unix-day count used by the timestamp formatter.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

/// Parses the `git worktree list --porcelain` output into raw filesystem paths.
fn parse_git_worktree_output(output: &str) -> Vec<PathBuf> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_path_for_claude_replaces_path_separators() {
        let encoded = encode_path_for_claude(Path::new("/Users/example/src/darc"));
        assert_eq!(encoded, "-Users-example-src-darc");
    }

    #[test]
    fn normalize_project_path_removes_dots_and_trailing_slashes() {
        let normalized = normalize_project_path(Path::new("/tmp/example/./old/../repo/"));

        assert_eq!(normalized, PathBuf::from("/tmp/example/repo"));
    }

    #[test]
    fn project_path_seed_includes_root_once() -> Result<()> {
        let root = PathBuf::from("/tmp/example");
        let paths = project_path_set(&root, std::slice::from_ref(&root))?;

        assert_eq!(paths.len(), 1);
        assert!(paths.contains(&root));

        Ok(())
    }

    #[test]
    fn normalized_known_paths_excludes_primary_root() {
        let root = PathBuf::from("/tmp/example");
        let paths = normalized_known_paths(
            &root,
            &[
                root.clone(),
                PathBuf::from("/tmp/example/"),
                PathBuf::from("/tmp/worktree"),
            ],
        );

        assert_eq!(paths, BTreeSet::from([PathBuf::from("/tmp/worktree")]));
    }

    #[test]
    fn worktree_parser_reads_porcelain_paths() {
        let output = "\
worktree /tmp/main
HEAD 123
branch refs/heads/main

worktree /tmp/wt
HEAD 456
branch refs/heads/feature
";
        let paths = parse_git_worktree_output(output);

        assert_eq!(
            paths,
            vec![PathBuf::from("/tmp/main"), PathBuf::from("/tmp/wt")]
        );
    }

    #[test]
    fn project_id_validation_rejects_path_escape_text() {
        assert!(is_valid_project_id("repo-abc123"));
        assert!(!is_valid_project_id("../../escape"));
        assert!(!is_valid_project_id("repo_abc123"));
        assert!(!is_valid_project_id("Repo-abc123"));
        assert!(!is_valid_project_id(""));
    }

    #[test]
    fn access_path_candidate_accepts_structured_filename_characters() {
        assert_eq!(
            normalize_access_path_candidate("  'src/main.rs'  ").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            normalize_access_path_candidate("src/$file.rs").as_deref(),
            Some("src/$file.rs")
        );
        assert_eq!(
            normalize_access_path_candidate("a-w").as_deref(),
            Some("a-w")
        );
        assert_eq!(
            normalize_access_path_candidate("--check").as_deref(),
            Some("--check")
        );
        assert!(normalize_access_path_candidate("/dev/null").is_none());
    }

    #[test]
    fn shell_access_path_candidate_rejects_shell_syntax_artifacts() {
        assert_eq!(
            normalize_shell_access_path_candidate("src/main.rs").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            normalize_shell_access_path_candidate("+x").as_deref(),
            Some("+x")
        );
        assert_eq!(
            normalize_shell_access_path_candidate("a-w").as_deref(),
            Some("a-w")
        );
        assert_eq!(
            normalize_shell_access_path_candidate("u+x").as_deref(),
            Some("u+x")
        );
        assert!(normalize_shell_access_path_candidate("--check").is_none());
        assert!(normalize_shell_access_path_candidate("2>&1").is_none());
        assert!(normalize_shell_access_path_candidate("&1)").is_none());
        assert!(normalize_shell_access_path_candidate("(RUST_LOG=debug").is_none());
        assert!(normalize_shell_access_path_candidate("$tmp/Cargo.toml").is_none());
        assert!(normalize_shell_access_path_candidate("src/$file.rs").is_none());
        assert!(normalize_shell_access_path_candidate("!=").is_none());
        assert!(normalize_shell_access_path_candidate("/dev/null").is_none());
    }

    #[test]
    fn utc_timestamp_round_trips() {
        let timestamp = UNIX_EPOCH + Duration::from_secs(1_744_022_096);
        let text = current_utc_timestamp_at(timestamp);

        assert_eq!(text, "2025-04-07T10:34:56Z");
        assert_eq!(parse_utc_timestamp(&text), Some(timestamp));
    }

    #[test]
    fn query_time_bounds_accept_canonical_utc_and_relative_days() {
        let now = UNIX_EPOCH + Duration::from_secs(1_744_022_096);

        assert_eq!(
            resolve_query_time_bound_at("2025-04-07T10:34:56Z", now),
            Ok("2025-04-07T10:34:56Z".to_owned())
        );
        assert_eq!(
            resolve_query_time_bound_at("5d", now),
            Ok("2025-04-02T10:34:56Z".to_owned())
        );
    }

    #[test]
    fn query_time_bounds_reject_invalid_absolute_timestamps() {
        let now = UNIX_EPOCH + Duration::from_secs(1_744_022_096);

        assert!(resolve_query_time_bound_at("2026-99-99T00:00:00Z", now).is_err());
        assert!(resolve_query_time_bound_at("2026-04-07T00:00:00+09:00", now).is_err());
        assert!(resolve_query_time_bound_at("2026-04-07T00:00:00.123Z", now).is_err());
    }
}
