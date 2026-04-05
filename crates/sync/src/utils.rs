use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use serde::Serialize;

static UNIQUE_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Stores file metadata used for change detection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FileSnapshot {
    pub(crate) size: u64,
    pub(crate) mtime_ms: u64,
}

/// Reads stable copy-comparison metadata from a source file.
pub(crate) fn file_snapshot(path: &Path) -> anyhow::Result<FileSnapshot> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("failed to read modified time for {}", path.display()))?;

    Ok(FileSnapshot {
        size: metadata.len(),
        mtime_ms: system_time_to_millis(modified)?,
    })
}

/// Builds a unique sibling path for atomic replace temp and backup files.
pub(crate) fn unique_sibling_path(path: &Path, kind: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "darc-temp".to_owned());
    path.with_file_name(format!(
        ".{file_name}.darc-{kind}-{}-{}",
        std::process::id(),
        UNIQUE_PATH_COUNTER.fetch_add(1, Ordering::Relaxed),
    ))
}

/// Copies a file via a temp sibling path and renames it into place.
pub(crate) fn copy_file_atomically(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let parent = destination
        .parent()
        .with_context(|| format!("missing parent directory for {}", destination.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let temp_path = unique_sibling_path(destination, "tmp");
    fs::copy(source, &temp_path).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            temp_path.display()
        )
    })?;
    rename_atomically(&temp_path, destination)
}

/// Writes JSON content through a temp sibling path and renames it into place.
pub(crate) fn write_json_atomically<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let content = serde_json::to_vec_pretty(value).context("failed to serialize JSON")?;
    write_bytes_atomically(path, &content)
}

/// Writes raw bytes through a temp sibling path and renames it into place.
fn write_bytes_atomically(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("missing parent directory for {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let temp_path = unique_sibling_path(path, "tmp");
    fs::write(&temp_path, content)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    rename_atomically(&temp_path, path)
}

/// Renames a temp path into place, replacing an existing target when necessary.
fn rename_atomically(temp_path: &Path, destination: &Path) -> anyhow::Result<()> {
    match fs::rename(temp_path, destination) {
        Ok(()) => Ok(()),
        Err(error) if destination.exists() => replace_existing_file(temp_path, destination, error),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to rename {} to {}",
                temp_path.display(),
                destination.display()
            )
        }),
    }
}

/// Replaces an existing file while keeping the original available until the swap succeeds.
pub(crate) fn replace_existing_file(
    temp_path: &Path,
    destination: &Path,
    rename_error: std::io::Error,
) -> anyhow::Result<()> {
    let backup_path = unique_sibling_path(destination, "bak");
    fs::rename(destination, &backup_path).with_context(|| {
        format!(
            "failed to move {} aside after renaming {} to {} failed: {rename_error}",
            destination.display(),
            temp_path.display(),
            destination.display()
        )
    })?;

    match fs::rename(temp_path, destination) {
        Ok(()) => fs::remove_file(&backup_path)
            .with_context(|| format!("failed to remove backup {}", backup_path.display())),
        Err(error) => match fs::rename(&backup_path, destination) {
            Ok(()) => Err(error).with_context(|| {
                format!(
                    "failed to rename {} to {} after moving the existing file aside",
                    temp_path.display(),
                    destination.display()
                )
            }),
            Err(restore_error) => bail!(
                "failed to rename {} to {} after moving aside {}; also failed to restore backup {}: {error}; {restore_error}",
                temp_path.display(),
                destination.display(),
                destination.display(),
                backup_path.display()
            ),
        },
    }
}

/// Converts a `SystemTime` to Unix epoch milliseconds.
fn system_time_to_millis(time: SystemTime) -> anyhow::Result<u64> {
    let millis = time
        .duration_since(UNIX_EPOCH)
        .context("time was before the Unix epoch")?
        .as_millis();
    u64::try_from(millis).context("millisecond timestamp overflowed u64")
}

/// Formats a `SystemTime` as a UTC RFC 3339 timestamp.
pub(crate) fn format_system_time_utc(time: SystemTime) -> anyhow::Result<String> {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .context("time was before the Unix epoch")?
        .as_secs();
    let days = i64::try_from(seconds / 86_400).context("day count overflowed i64")?;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;

    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

/// Converts days since the Unix epoch into a UTC calendar date.
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}
