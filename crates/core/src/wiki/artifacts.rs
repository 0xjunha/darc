use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use darc_agent::ProposalOutputSource;
use darc_wiki::{ProjectLayout, RunId, RunState, write_string_atomically};
use serde::Serialize;

use super::{
    RUN_RESULT_SCHEMA,
    models::{
        DigestResultArtifact, DigestRuntimeArtifact, DigestValidationArtifact, RunEvent,
        RuntimeExecution,
    },
};

/// Writes one JSON artifact file with pretty formatting.
pub(super) fn write_json_artifact<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let content =
        serde_json::to_string_pretty(value).context("failed to serialize JSON artifact")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, format!("{content}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Ensures one shared UTF-8 artifact exists with the expected bytes without overwriting mismatches.
pub(super) fn ensure_shared_text_artifact(
    path: &Path,
    lock_path: &Path,
    content: &str,
) -> Result<()> {
    if file_bytes_match(path, content.as_bytes())? {
        return Ok(());
    }

    let _lock = lock_shared_artifact(lock_path)?;
    if file_bytes_match(path, content.as_bytes())? {
        return Ok(());
    }
    if path.exists() {
        bail!(
            "shared artifact {} already exists with different bytes; refusing to overwrite it",
            path.display()
        );
    }

    write_string_atomically(path, content)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Writes one opaque byte artifact file after ensuring its parent directory exists.
pub(super) fn write_bytes_artifact(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

/// Creates one empty file after ensuring its parent directory exists.
pub(super) fn touch_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    Ok(())
}

/// Compares one file's current bytes with the desired content bytes.
fn file_bytes_match(path: &Path, expected: &[u8]) -> Result<bool> {
    match fs::read(path) {
        Ok(existing) => Ok(existing == expected),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(source).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Locks one shared artifact mutation path using the standard blocking file-lock pattern.
fn lock_shared_artifact(lock_path: &Path) -> Result<File> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    file.lock()
        .with_context(|| format!("failed to lock {}", lock_path.display()))?;
    Ok(file)
}

/// Returns the basename string for one run artifact path.
pub(super) fn relative_artifact_name(path: PathBuf) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .expect("artifact paths should have a filename")
}

/// Appends one JSONL run event for progress reporting and debugging.
pub(super) fn append_run_event(
    layout: &ProjectLayout,
    run_id: &RunId,
    event: RunEvent,
) -> Result<()> {
    let path = layout.run_events_path(run_id);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let line = serde_json::to_string(&event).context("failed to serialize run event")?;
    writeln!(file, "{line}").with_context(|| format!("failed to append {}", path.display()))
}

/// Writes one terminal digest result artifact from the finalized run state.
pub(super) fn write_terminal_result(
    layout: &ProjectLayout,
    run_id: &RunId,
    state: &RunState,
    runtime: Option<&RuntimeExecution>,
    validation: DigestValidationArtifact,
    note: Option<String>,
) -> Result<()> {
    let completed_at = state
        .finished_at
        .clone()
        .unwrap_or_else(|| state.updated_at.clone());
    write_digest_result(
        layout,
        run_id,
        &build_result_artifact(state, completed_at, runtime, validation, note),
    )
}

/// Writes one digest result artifact into the durable run directory.
fn write_digest_result(
    layout: &ProjectLayout,
    run_id: &RunId,
    artifact: &DigestResultArtifact,
) -> Result<()> {
    write_json_artifact(&layout.run_result_path(run_id), artifact)
}

/// Builds one digest result artifact from the finalized runtime and validation outcomes.
fn build_result_artifact(
    state: &RunState,
    completed_at: String,
    runtime: Option<&RuntimeExecution>,
    validation: DigestValidationArtifact,
    note: Option<String>,
) -> DigestResultArtifact {
    DigestResultArtifact {
        schema: RUN_RESULT_SCHEMA.to_owned(),
        project_id: state.project_id.clone(),
        run_id: state.run_id.to_string(),
        status: state.status,
        completed_at,
        error_code: state.error_code.clone(),
        error_message: state.error_message.clone(),
        runtime: DigestRuntimeArtifact {
            agent_id: state.agent_id.clone(),
            runtime: state.runtime.clone(),
            model: state.model.clone(),
            auth_profile: state.auth_profile.clone(),
            display_name: runtime.map(|runtime| runtime.display_name.clone()),
            exit_code: runtime.and_then(|runtime| runtime.exit_code),
            stdout_bytes: runtime.map_or(0, |runtime| runtime.stdout.len()),
            stderr_bytes: runtime.map_or(0, |runtime| runtime.stderr.len()),
            proposal_source: runtime.map(runtime_proposal_source_name),
            proposal_captured: runtime.is_some_and(|runtime| runtime.proposal_bytes.is_some()),
        },
        validation,
        note,
    }
}

/// Returns the durable proposal-source label for one runtime execution.
fn runtime_proposal_source_name(runtime: &RuntimeExecution) -> String {
    match runtime.proposal_source {
        ProposalOutputSource::Stdout | ProposalOutputSource::StdoutJsonField(_) => {
            "stdout".to_owned()
        }
        ProposalOutputSource::File(_) => "file".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, thread};

    use anyhow::Result;
    use darc_test_utils::unique_test_dir;

    use super::ensure_shared_text_artifact;

    /// Verifies that matching shared schema writers converge on one stable file.
    #[test]
    fn shared_text_artifact_allows_concurrent_identical_writers() -> Result<()> {
        let root = unique_test_dir("shared-schema-concurrent");
        let path = root.join("proposal.schema.v1.json");
        let lock_path = root.join("proposal.schema.v1.json.lock");
        let handles = (0..8)
            .map(|_| {
                let path = path.clone();
                let lock_path = lock_path.clone();
                thread::spawn(move || {
                    ensure_shared_text_artifact(&path, &lock_path, "shared-schema")
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().expect("writer thread panicked")?;
        }
        assert_eq!(fs::read_to_string(&path)?, "shared-schema");

        fs::remove_dir_all(&root)?;
        Ok(())
    }

    /// Verifies that a shared schema file is treated as immutable once initialized.
    #[test]
    fn shared_text_artifact_rejects_mismatched_existing_bytes() -> Result<()> {
        let root = unique_test_dir("shared-schema-mismatch");
        let path = root.join("proposal.schema.v1.json");
        let lock_path = root.join("proposal.schema.v1.json.lock");

        ensure_shared_text_artifact(&path, &lock_path, "alpha")?;
        let error = ensure_shared_text_artifact(&path, &lock_path, "beta").unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(fs::read_to_string(&path)?, "alpha");

        fs::remove_dir_all(&root)?;
        Ok(())
    }
}
