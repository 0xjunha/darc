use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::SystemTime,
};

use anyhow::{Context, Result, bail};
use darc_agent::{AgentId, ProposalOutputSource, RuntimeCommand, RuntimeKind, RuntimeRequest};
use darc_paths::current_utc_timestamp;
use darc_wiki::{
    DigestRuntimePrompt, ProjectLayout, RunId, RunPhase, RunState, RunStatus, load_run_state,
};
use serde_json::Value;

use super::{
    RUN_HEARTBEAT_INTERVAL, RUN_POLL_INTERVAL, RUNTIME_CANCEL_GRACE_PERIOD,
    artifacts::append_run_event,
    models::{RunEvent, RuntimeExecution},
    state::{cancel_requested, update_run_state},
};

/// Builds one runtime request from the persisted run state and prepared prompt.
pub(super) fn build_runtime_request(
    layout: &ProjectLayout,
    run_id: &RunId,
    state: &RunState,
    prompt: &DigestRuntimePrompt,
    schema_path: &Path,
    project_root: &Path,
) -> Result<RuntimeRequest> {
    let agent = AgentId::parse(state.agent_id.as_deref().unwrap_or_default())
        .context("run state is missing a supported agent id")?;
    let runtime = RuntimeKind::parse(state.runtime.as_deref().unwrap_or_default())
        .context("run state is missing a supported runtime kind")?;
    Ok(RuntimeRequest {
        agent,
        runtime,
        model: state.model.clone().unwrap_or_default(),
        auth_profile: state.auth_profile.clone(),
        use_provider_auth: state.use_provider_auth,
        prompt: prompt.prompt.clone(),
        schema_json: prompt.schema_json.clone(),
        darc_root: layout.context().darc_root.clone(),
        workdir: project_root.to_path_buf(),
        schema_path: schema_path.to_path_buf(),
        proposal_path: layout.run_proposal_path(run_id),
    })
}

/// Executes one prepared runtime command while streaming logs and preserving worker heartbeats.
pub(super) fn execute_runtime_command(
    layout: &ProjectLayout,
    run_id: &RunId,
    command: RuntimeCommand,
) -> Result<RuntimeExecution> {
    let mut child_command = Command::new(&command.program);
    child_command.args(&command.args);
    for name in &command.env_remove {
        child_command.env_remove(name);
    }
    let mut child = child_command
        .current_dir(&command.workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {}", command.display_name))?;
    let stdin_handle = write_process_stdin(
        child
            .stdin
            .take()
            .context("runtime child stdin pipe was not captured")?,
        command.stdin,
    );
    let stdout_handle = capture_process_stream(
        child
            .stdout
            .take()
            .context("runtime child stdout pipe was not captured")?,
        layout.run_stdout_log_path(run_id),
    );
    let stderr_handle = capture_process_stream(
        child
            .stderr
            .take()
            .context("runtime child stderr pipe was not captured")?,
        layout.run_stderr_log_path(run_id),
    );

    let mut last_heartbeat = SystemTime::now();
    let mut cancel_noted = false;
    let mut cancel_sent_at = None;
    let exit_status = loop {
        if let Some(status) = child.try_wait().context("failed to poll runtime child")? {
            break status;
        }

        if last_heartbeat.elapsed().unwrap_or_default() >= RUN_HEARTBEAT_INTERVAL {
            update_run_state(layout, run_id, |state| {
                let now = current_utc_timestamp();
                if state.cancel_requested {
                    state.status = RunStatus::CancelRequested;
                    state.headline = Some("Cancel requested; waiting for runtime exit".to_owned());
                }
                state.updated_at = now.clone();
                state.heartbeat_at = Some(now);
            })?;
            last_heartbeat = SystemTime::now();
        }

        let state = load_run_state(layout, run_id)?;
        if cancel_requested(layout, run_id, &state)? && !cancel_noted {
            terminate_runtime_process(&mut child, &command.display_name)
                .context("failed to terminate runtime after cancel requested")?;
            append_run_event(
                layout,
                run_id,
                RunEvent::info(
                    RunPhase::WaitingForAgent,
                    "Cancel requested; sent termination to runtime process".to_owned(),
                ),
            )?;
            cancel_noted = true;
            cancel_sent_at = Some(SystemTime::now());
        }
        if let Some(cancel_sent_at) = cancel_sent_at
            && cancel_sent_at.elapsed().unwrap_or_default() >= RUNTIME_CANCEL_GRACE_PERIOD
        {
            bail!(
                "runtime process did not exit within {:?} after cancellation",
                RUNTIME_CANCEL_GRACE_PERIOD
            );
        }

        thread::sleep(RUN_POLL_INTERVAL);
    };
    stdin_handle
        .join()
        .map_err(|_| anyhow::anyhow!("runtime stdin writer thread panicked"))??;

    let stdout = stdout_handle
        .join()
        .map_err(|_| anyhow::anyhow!("runtime stdout capture thread panicked"))??;
    let stderr = stderr_handle
        .join()
        .map_err(|_| anyhow::anyhow!("runtime stderr capture thread panicked"))??;
    let proposal_bytes = match &command.proposal_output {
        ProposalOutputSource::Stdout => Some(stdout.clone()),
        ProposalOutputSource::StdoutJsonField(field_name) => {
            capture_stdout_json_field(&stdout, field_name).or_else(|| Some(stdout.clone()))
        }
        ProposalOutputSource::File(path) if path.exists() => {
            Some(fs::read(path).with_context(|| format!("failed to read {}", path.display()))?)
        }
        ProposalOutputSource::File(_) => None,
    };
    Ok(RuntimeExecution {
        display_name: command.display_name,
        proposal_source: command.proposal_output,
        exit_code: exit_status.code(),
        stdout,
        stderr,
        proposal_bytes,
    })
}

/// Extracts one JSON field from stdout when the runtime wraps structured output with metadata.
fn capture_stdout_json_field(stdout: &[u8], field_name: &str) -> Option<Vec<u8>> {
    let value: Value = serde_json::from_slice(stdout).ok()?;
    let structured_output = value.get(field_name)?;
    serde_json::to_vec(structured_output).ok()
}

/// Writes one runtime prompt payload into the child stdin stream.
fn write_process_stdin<W>(mut writer: W, input: Vec<u8>) -> thread::JoinHandle<Result<()>>
where
    W: Write + Send + 'static,
{
    thread::spawn(move || {
        if let Err(error) = writer.write_all(&input)
            && error.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(error).context("failed to write runtime stdin payload");
        }
        if let Err(error) = writer.flush()
            && error.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(error).context("failed to flush runtime stdin payload");
        }
        Ok(())
    })
}

/// Captures one runtime output stream into both a durable log file and an in-memory buffer.
fn capture_process_stream<R>(mut reader: R, path: PathBuf) -> thread::JoinHandle<Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let mut collected = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            let read = reader
                .read(&mut buffer)
                .with_context(|| format!("failed to read {}", path.display()))?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read])
                .with_context(|| format!("failed to append {}", path.display()))?;
            collected.extend_from_slice(&buffer[..read]);
        }
        file.flush()
            .with_context(|| format!("failed to flush {}", path.display()))?;
        Ok(collected)
    })
}

/// Terminates one runtime child process after the user requests cancellation.
fn terminate_runtime_process(child: &mut Child, display_name: &str) -> Result<()> {
    match child.kill() {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::NotFound
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("failed to kill {display_name}")),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::capture_stdout_json_field;

    #[test]
    fn capture_stdout_json_field_extracts_structured_output() {
        let stdout = br#"{"result":"done","structured_output":{"schema":"demo","entries":[]}}"#;
        let proposal = capture_stdout_json_field(stdout, "structured_output")
            .expect("structured output should be captured");
        let proposal: serde_json::Value = serde_json::from_slice(&proposal).unwrap();
        assert_eq!(proposal, json!({"schema": "demo", "entries": []}));
    }

    #[test]
    fn capture_stdout_json_field_returns_none_for_missing_field() {
        let stdout = br#"{"result":"done"}"#;
        assert!(capture_stdout_json_field(stdout, "structured_output").is_none());
    }
}
