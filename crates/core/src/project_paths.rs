use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
pub(crate) use darc_paths::{encode_path_for_claude, normalize_project_path};

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
