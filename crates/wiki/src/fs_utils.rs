use std::{
    fs::{self, DirEntry},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{Result, WikiError};

static UNIQUE_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

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

/// Writes one string through a temp sibling path and atomically renames it into place.
pub(crate) fn write_string_atomically(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().ok_or_else(|| WikiError::WriteFile {
        path: path.to_path_buf(),
        source: std::io::Error::other("missing parent directory"),
    })?;
    fs::create_dir_all(parent).map_err(|source| WikiError::CreateDir {
        path: parent.to_path_buf(),
        source,
    })?;
    let temp_path = unique_sibling_path(path);
    fs::write(&temp_path, content).map_err(|source| WikiError::WriteFile {
        path: temp_path.clone(),
        source,
    })?;
    rename_atomically(&temp_path, path)
}

/// Builds one unique sibling temp path for atomic file replacement.
fn unique_sibling_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("darc-temp");
    path.with_file_name(format!(
        ".{file_name}.darc-tmp-{}-{}",
        std::process::id(),
        UNIQUE_PATH_COUNTER.fetch_add(1, Ordering::Relaxed),
    ))
}

/// Renames one temp file into place while preserving replacement behavior on every platform.
fn rename_atomically(temp_path: &Path, destination: &Path) -> Result<()> {
    match fs::rename(temp_path, destination) {
        Ok(()) => Ok(()),
        Err(error) if destination.exists() => replace_existing_file(temp_path, destination, error),
        Err(source) => Err(WikiError::WriteFile {
            path: destination.to_path_buf(),
            source,
        }),
    }
}

/// Replaces one existing file while restoring the original if the second rename fails.
fn replace_existing_file(
    temp_path: &Path,
    destination: &Path,
    rename_error: std::io::Error,
) -> Result<()> {
    let backup_path = unique_sibling_path(destination);
    fs::rename(destination, &backup_path).map_err(|source| WikiError::WriteFile {
        path: destination.to_path_buf(),
        source,
    })?;

    match fs::rename(temp_path, destination) {
        Ok(()) => fs::remove_file(&backup_path).map_err(|source| WikiError::WriteFile {
            path: backup_path,
            source,
        }),
        Err(source) => {
            let _ = fs::rename(&backup_path, destination);
            Err(WikiError::WriteFile {
                path: destination.to_path_buf(),
                source: std::io::Error::new(
                    source.kind(),
                    format!(
                        "failed to rename {} to {} after replacing existing file: {source}; previous error: {rename_error}",
                        temp_path.display(),
                        destination.display()
                    ),
                ),
            })
        }
    }
}
