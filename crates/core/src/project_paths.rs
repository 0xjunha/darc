use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};

/// Resolves the current project root, preferring the git toplevel when available.
pub(crate) fn current_project_root(current_dir: &Path) -> Result<PathBuf> {
    let root = try_git_output(current_dir, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or_else(|| current_dir.to_path_buf());
    fs::canonicalize(&root).with_context(|| format!("unable to canonicalize {}", root.display()))
}

/// Builds the full project path set from the current root, live worktrees, and known paths.
pub(crate) fn project_path_set(
    current_root: &Path,
    known_paths: &[PathBuf],
) -> Result<BTreeSet<PathBuf>> {
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
pub(crate) fn normalized_known_paths(
    current_root: &Path,
    known_paths: &[PathBuf],
) -> BTreeSet<PathBuf> {
    let current_root = normalize_project_path(current_root);
    known_paths
        .iter()
        .map(|path| normalize_project_path(path))
        .filter(|path| path != &current_root)
        .collect()
}

/// Returns the seed list stored into `known_paths` for a freshly initialized project.
pub(crate) fn seed_known_paths(current_root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = project_path_set(current_root, &[])?;
    paths.remove(&normalize_project_path(current_root));
    Ok(paths.into_iter().collect())
}

/// Encodes a project path using Claude's directory naming rule.
pub(crate) fn encode_path_for_claude(path: &Path) -> String {
    path.to_string_lossy().replace('/', "-")
}

/// Lists the current live git worktree paths for a repository or worktree root.
pub(crate) fn git_worktree_paths(project_root: &Path) -> Result<Vec<PathBuf>> {
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

/// Normalizes a project path using canonicalization when possible.
pub(crate) fn normalize_project_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_path_textually(path))
}

/// Parses the `git worktree list --porcelain` output into raw filesystem paths.
fn parse_git_worktree_output(output: &str) -> Vec<PathBuf> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect()
}

/// Executes a git command and returns trimmed UTF-8 stdout on success.
pub(crate) fn try_git_output(cwd: &Path, args: &[&str]) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn claude_encoding_replaces_path_separators() {
        let encoded = encode_path_for_claude(Path::new("/Users/example/src/memstack"));
        assert_eq!(encoded, "-Users-example-src-memstack");
    }

    #[test]
    fn textual_normalization_removes_dots_and_trailing_slashes() {
        let normalized = normalize_project_path(Path::new("/tmp/example/./old/../repo/"));

        assert_eq!(normalized, PathBuf::from("/tmp/example/repo"));
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
}
