use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use directories::BaseDirs;
use walkdir::WalkDir;

use super::types::DetectedRolloutSource;
use crate::{
    SourceKind,
    constants::{CLAUDE_DEFAULT_DIR, CODEX_DEFAULT_DIR},
};

/// Detects all supported upstream rollout sources on the local machine.
pub(super) fn detect_sources(base_dirs: &BaseDirs) -> Result<Vec<DetectedRolloutSource>> {
    Ok([detect_claude(base_dirs)?, detect_codex(base_dirs)?]
        .into_iter()
        .flatten()
        .collect())
}

/// Detects the local Claude projects tree and counts matching session files.
fn detect_claude(base_dirs: &BaseDirs) -> Result<Option<DetectedRolloutSource>> {
    let home = base_dirs.home_dir().join(CLAUDE_DEFAULT_DIR);
    if !home.exists() {
        return Ok(None);
    }

    let root = home.join("projects");
    let (rollout_files, subagent_rollout_files) = count_rollouts(&root, SourceKind::Claude)?;

    Ok(Some(DetectedRolloutSource {
        home,
        kind: SourceKind::Claude,
        root,
        rollout_files,
        subagent_rollout_files,
    }))
}

/// Detects the local Codex sessions tree and counts matching rollout files.
fn detect_codex(base_dirs: &BaseDirs) -> Result<Option<DetectedRolloutSource>> {
    let home = codex_home(base_dirs.home_dir());
    if !home.exists() {
        return Ok(None);
    }

    let root = home.join("sessions");
    let (rollout_files, subagent_rollout_files) = count_rollouts(&root, SourceKind::Codex)?;

    Ok(Some(DetectedRolloutSource {
        home,
        kind: SourceKind::Codex,
        root,
        rollout_files,
        subagent_rollout_files,
    }))
}

/// Returns the effective Codex home directory, honoring `CODEX_HOME` when set.
pub(super) fn codex_home(home_dir: &Path) -> PathBuf {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir.join(CODEX_DEFAULT_DIR))
}

/// Counts rollout files for a source kind and tracks Claude subagent files separately.
fn count_rollouts(root: &Path, kind: SourceKind) -> Result<(usize, usize)> {
    let mut rollout_files = 0;
    let mut subagent_rollout_files = 0;
    if !root.exists() {
        return Ok((rollout_files, subagent_rollout_files));
    }

    for entry in WalkDir::new(root) {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }

        match kind {
            SourceKind::Codex => {
                let is_rollout = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"));
                if is_rollout {
                    rollout_files += 1;
                }
            }
            SourceKind::Claude => {
                let is_subagent = path
                    .components()
                    .any(|component| component.as_os_str() == "subagents");
                if is_subagent {
                    subagent_rollout_files += 1;
                }
                rollout_files += 1;
            }
        }
    }

    Ok((rollout_files, subagent_rollout_files))
}
