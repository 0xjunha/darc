use std::{
    fs::{self, DirEntry},
    path::{Path, PathBuf},
};

use crate::{Result, WikiError};

/// Lists one directory in lexicographic path order for deterministic reads.
pub(crate) fn read_dir_sorted(path: &Path) -> Result<Vec<DirEntry>> {
    let mut entries = fs::read_dir(path)
        .map_err(|source| WikiError::ReadDir {
            path: path.to_path_buf(),
            source,
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|source| WikiError::ReadDir {
            path: path.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.path());
    Ok(entries)
}

/// Recursively collects Markdown file paths in lexicographic order.
pub(crate) fn collect_markdown_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }

    collect_markdown_files_into(root, &mut files)?;
    files.sort();
    Ok(files)
}

/// Recursively visits directories and records Markdown files into the output vector.
fn collect_markdown_files_into(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in read_dir_sorted(root)? {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| WikiError::ReadDir {
            path: root.to_path_buf(),
            source,
        })?;
        if file_type.is_dir() {
            collect_markdown_files_into(&path, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            files.push(path);
        }
    }
    Ok(())
}
