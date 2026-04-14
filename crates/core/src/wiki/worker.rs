use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};
use darc_agent::{RuntimeCommand, build_runtime_command};
use darc_paths::current_utc_timestamp;
use darc_wiki::{
    DigestProposal, DigestRuntimePrompt, EntryFrontmatter, ProjectLayout, ProjectRegistry,
    ProposalValidationOptions, RunId, RunPhase, RunState, build_digest_runtime_prompt,
    list_entries, load_entry, load_registry, load_run_state, validate_digest_proposal,
};

use super::{
    PROPOSAL_SCHEMA_FILE_NAME, RUN_CONTEXT_SCHEMA, RUN_HEARTBEAT_INTERVAL,
    artifacts::{
        append_run_event, write_bytes_artifact, write_json_artifact, write_terminal_result,
        write_text_artifact,
    },
    context::{build_allowed_domains, build_allowed_evidence_refs, load_selected_session_context},
    models::{DigestContextArtifact, DigestValidationArtifact, RunEvent, RuntimeExecution},
    runtime::{build_runtime_request, execute_runtime_command},
    state::{
        cancel_requested, finalize_run_canceled, finalize_run_failed, finalize_run_succeeded,
        is_finished_status, refresh_worker_heartbeat, transition_worker_state,
        wait_for_worker_registration,
    },
};
use crate::{default_root_path, query::query_sessions};

/// Runs the hidden digest worker loop for one existing run.
pub(super) fn run_project_wiki_digest_worker(
    root: Option<PathBuf>,
    project_id: &str,
    run_id: &RunId,
) -> Result<()> {
    DigestWorker::new(root, project_id, run_id)?.run()
}

/// Coordinates the multi-phase digest worker lifecycle for one run.
struct DigestWorker<'a> {
    root: PathBuf,
    project_id: &'a str,
    run_id: &'a RunId,
    layout: ProjectLayout,
}

/// Stores one terminal worker failure payload before durable result writing.
struct WorkerFailure<'a> {
    phase: RunPhase,
    headline: &'a str,
    error_code: &'a str,
    error_message: String,
    runtime: Option<&'a RuntimeExecution>,
    validation: DigestValidationArtifact,
    event_message: String,
}

impl<'a> DigestWorker<'a> {
    /// Builds one digest worker wrapper from the configured Darc root and run id.
    fn new(root: Option<PathBuf>, project_id: &'a str, run_id: &'a RunId) -> Result<Self> {
        let root = root.unwrap_or_else(default_root_path);
        let layout = super::api::resolve_project_layout(Some(root.clone()), project_id)?;
        Ok(Self {
            root,
            project_id,
            run_id,
            layout,
        })
    }

    /// Executes the full digest worker state machine for one run.
    fn run(&self) -> Result<()> {
        match self.run_inner() {
            Ok(()) => Ok(()),
            Err(error) => {
                self.fail_unhandled_error(&error)?;
                Err(error)
            }
        }
    }

    /// Executes the full digest worker state machine for one run without outer error finalization.
    fn run_inner(&self) -> Result<()> {
        let state = wait_for_worker_registration(&self.layout, self.run_id)?;
        if is_finished_status(state.status) {
            return Ok(());
        }

        let registry =
            load_registry(&self.layout).context("failed to load project wiki registry")?;
        let context = match self.build_context(&state, &registry)? {
            Some(context) => context,
            None => return Ok(()),
        };

        let state = load_run_state(&self.layout, self.run_id)?;
        let runtime_execution = match self.execute_runtime(&state, &context)? {
            Some(runtime_execution) => runtime_execution,
            None => return Ok(()),
        };

        let state = load_run_state(&self.layout, self.run_id)?;
        let validation =
            match self.validate_proposal(&state, &registry, &context, &runtime_execution)? {
                Some(validation) => validation,
                None => return Ok(()),
            };

        self.complete(&validation, &runtime_execution)
    }

    /// Builds and persists the digest context artifact before runtime invocation.
    fn build_context(
        &self,
        state: &RunState,
        registry: &ProjectRegistry,
    ) -> Result<Option<DigestContextArtifact>> {
        transition_worker_state(
            &self.layout,
            self.run_id,
            RunPhase::ReadingTurns,
            Some(10),
            "Preparing context bundle",
        )?;
        let session_summaries =
            match query_sessions(Some(self.root.clone()), self.project_id, None, None)
                .context("failed to load indexed session summaries for digest context")
            {
                Ok(data) => data.sessions,
                Err(error) => {
                    return self.fail(WorkerFailure {
                        phase: RunPhase::ReadingTurns,
                        headline: "Context build failed",
                        error_code: "context_build_failed",
                        error_message: error.to_string(),
                        runtime: None,
                        validation: DigestValidationArtifact::default(),
                        event_message: "Failed to build digest context".to_owned(),
                    });
                }
            };
        append_run_event(
            &self.layout,
            self.run_id,
            RunEvent::info(
                RunPhase::ReadingTurns,
                format!(
                    "Resolved {} selected session reference(s)",
                    state.selected_sessions.len()
                ),
            ),
        )?;

        let selected_sessions = state.selected_sessions.clone();
        let total_sessions = selected_sessions.len().max(1);
        let mut total_turns = 0_usize;
        let mut context_sessions = Vec::with_capacity(selected_sessions.len());
        let mut last_heartbeat = SystemTime::now();
        for (index, session_ref) in selected_sessions.iter().enumerate() {
            transition_worker_state(
                &self.layout,
                self.run_id,
                RunPhase::ReadingTurns,
                Some(10 + (((index + 1) * 25) / total_sessions) as u8),
                &format!(
                    "Reading narrative turns for session {} of {}",
                    index + 1,
                    total_sessions
                ),
            )?;
            let session = match load_selected_session_context(
                &self.root,
                self.project_id,
                session_ref,
                &session_summaries,
                || {
                    if last_heartbeat.elapsed().unwrap_or_default() >= RUN_HEARTBEAT_INTERVAL {
                        refresh_worker_heartbeat(&self.layout, self.run_id)?;
                        last_heartbeat = SystemTime::now();
                    }
                    Ok(())
                },
            ) {
                Ok(session) => session,
                Err(error) => {
                    return self.fail(WorkerFailure {
                        phase: RunPhase::ReadingTurns,
                        headline: "Context build failed",
                        error_code: "context_build_failed",
                        error_message: error.to_string(),
                        runtime: None,
                        validation: DigestValidationArtifact::default(),
                        event_message: format!(
                            "Failed to load session context for `{session_ref}`"
                        ),
                    });
                }
            };
            total_turns += session.turns.len();
            append_run_event(
                &self.layout,
                self.run_id,
                RunEvent::info(
                    RunPhase::ReadingTurns,
                    format!(
                        "Loaded {} narrative turn(s) from `{session_ref}`",
                        session.turns.len()
                    ),
                ),
            )?;
            context_sessions.push(session);

            let current_state = load_run_state(&self.layout, self.run_id)?;
            if cancel_requested(&self.layout, self.run_id, &current_state)? {
                return self.cancel(RunPhase::ReadingTurns, None, None);
            }
        }

        let context = DigestContextArtifact {
            schema: RUN_CONTEXT_SCHEMA.to_owned(),
            project_id: self.project_id.to_owned(),
            run_id: self.run_id.to_string(),
            selected_sessions: state.selected_sessions.clone(),
            target_categories: state.target_categories.clone(),
            target_domains: state.target_domains.clone(),
            registry: registry.clone(),
            sessions: context_sessions,
            generated_at: current_utc_timestamp(),
        };
        write_json_artifact(&self.layout.run_context_path(self.run_id), &context)?;
        append_run_event(
            &self.layout,
            self.run_id,
            RunEvent::info(
                RunPhase::ReadingTurns,
                format!(
                    "Loaded {total_turns} narrative turn(s) across {} session(s)",
                    state.selected_sessions.len()
                ),
            ),
        )?;

        let current_state = load_run_state(&self.layout, self.run_id)?;
        if cancel_requested(&self.layout, self.run_id, &current_state)? {
            return self.cancel(RunPhase::ReadingTurns, None, None);
        }

        Ok(Some(context))
    }

    /// Prepares the runtime command, executes it, and captures its proposal artifact.
    fn execute_runtime(
        &self,
        state: &RunState,
        context: &DigestContextArtifact,
    ) -> Result<Option<RuntimeExecution>> {
        transition_worker_state(
            &self.layout,
            self.run_id,
            RunPhase::WaitingForAgent,
            Some(40),
            "Preparing agent runtime",
        )?;
        let proposal_schema_path = self
            .layout
            .run_dir(self.run_id)
            .join(PROPOSAL_SCHEMA_FILE_NAME);
        let context_json = serde_json::to_string_pretty(context)
            .context("failed to serialize digest context JSON")?;
        let prompt =
            build_digest_runtime_prompt(&context_json, &context.project_id, &context.run_id);
        write_text_artifact(&proposal_schema_path, &prompt.schema_json)?;

        let runtime_command =
            match self.prepare_runtime_command(state, &prompt, &proposal_schema_path)? {
                Some(runtime_command) => runtime_command,
                None => return Ok(None),
            };

        transition_worker_state(
            &self.layout,
            self.run_id,
            RunPhase::WaitingForAgent,
            Some(50),
            &format!("Invoking {}", runtime_command.display_name),
        )?;
        append_run_event(
            &self.layout,
            self.run_id,
            RunEvent::info(
                RunPhase::WaitingForAgent,
                format!("Started {}", runtime_command.display_name),
            ),
        )?;

        let runtime_execution =
            match execute_runtime_command(&self.layout, self.run_id, runtime_command) {
                Ok(runtime_execution) => runtime_execution,
                Err(error) => {
                    return self.fail(WorkerFailure {
                        phase: RunPhase::WaitingForAgent,
                        headline: "Agent runtime invocation failed",
                        error_code: "runtime_invocation_failed",
                        error_message: error.to_string(),
                        runtime: None,
                        validation: DigestValidationArtifact::default(),
                        event_message: "Agent runtime invocation failed".to_owned(),
                    });
                }
            };

        let current_state = load_run_state(&self.layout, self.run_id)?;
        if cancel_requested(&self.layout, self.run_id, &current_state)? {
            let note = runtime_execution.proposal_bytes.is_some().then_some(
                "Proposal capture completed but validation was skipped after cancel request"
                    .to_owned(),
            );
            return self.cancel(RunPhase::WaitingForAgent, Some(&runtime_execution), note);
        }

        if runtime_execution.exit_code != Some(0) {
            return self.fail(WorkerFailure {
                phase: RunPhase::WaitingForAgent,
                headline: "Agent runtime invocation failed",
                error_code: "runtime_invocation_failed",
                error_message: format!(
                    "{} exited with code {}",
                    runtime_execution.display_name,
                    runtime_execution
                        .exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "unknown".to_owned())
                ),
                runtime: Some(&runtime_execution),
                validation: DigestValidationArtifact::default(),
                event_message: "Agent runtime exited unsuccessfully".to_owned(),
            });
        }

        let Some(proposal_bytes) = runtime_execution.proposal_bytes.as_ref() else {
            return self.fail(WorkerFailure {
                phase: RunPhase::WaitingForAgent,
                headline: "Proposal artifact missing",
                error_code: "proposal_missing",
                error_message: "agent runtime did not produce a proposal artifact".to_owned(),
                runtime: Some(&runtime_execution),
                validation: DigestValidationArtifact::default(),
                event_message: "Agent runtime did not produce a proposal artifact".to_owned(),
            });
        };

        if matches!(
            runtime_execution.proposal_source,
            darc_agent::ProposalOutputSource::Stdout
        ) {
            write_bytes_artifact(&self.layout.run_proposal_path(self.run_id), proposal_bytes)?;
        }

        Ok(Some(runtime_execution))
    }

    /// Prepares the runtime command from the persisted run state and prompt payload.
    fn prepare_runtime_command(
        &self,
        state: &RunState,
        prompt: &DigestRuntimePrompt,
        proposal_schema_path: &Path,
    ) -> Result<Option<RuntimeCommand>> {
        let runtime_request = match build_runtime_request(
            &self.root,
            self.project_id,
            &self.layout,
            self.run_id,
            state,
            prompt,
            proposal_schema_path,
        ) {
            Ok(runtime_request) => runtime_request,
            Err(error) => {
                return self.fail(WorkerFailure {
                    phase: RunPhase::WaitingForAgent,
                    headline: "Agent runtime preparation failed",
                    error_code: "runtime_prepare_failed",
                    error_message: error.to_string(),
                    runtime: None,
                    validation: DigestValidationArtifact::default(),
                    event_message: "Failed to prepare digest runtime command".to_owned(),
                });
            }
        };
        let runtime_command = match build_runtime_command(&runtime_request) {
            Ok(runtime_command) => runtime_command,
            Err(error) => {
                return self.fail(WorkerFailure {
                    phase: RunPhase::WaitingForAgent,
                    headline: "Agent runtime preparation failed",
                    error_code: "runtime_prepare_failed",
                    error_message: error.to_string(),
                    runtime: None,
                    validation: DigestValidationArtifact::default(),
                    event_message: "Failed to prepare digest runtime command".to_owned(),
                });
            }
        };
        Ok(Some(runtime_command))
    }

    /// Validates the captured proposal artifact against Darc's schema and allowlists.
    fn validate_proposal(
        &self,
        state: &RunState,
        registry: &ProjectRegistry,
        context: &DigestContextArtifact,
        runtime_execution: &RuntimeExecution,
    ) -> Result<Option<DigestValidationArtifact>> {
        transition_worker_state(
            &self.layout,
            self.run_id,
            RunPhase::ValidatingProposal,
            Some(80),
            "Validating proposal artifact",
        )?;

        let proposal_bytes = runtime_execution
            .proposal_bytes
            .as_ref()
            .expect("runtime execution should include proposal bytes before validation");
        let proposal_text = match String::from_utf8(proposal_bytes.clone()) {
            Ok(proposal_text) => proposal_text,
            Err(error) => {
                return self.fail(WorkerFailure {
                    phase: RunPhase::ValidatingProposal,
                    headline: "Proposal artifact is not UTF-8",
                    error_code: "proposal_not_utf8",
                    error_message: error.to_string(),
                    runtime: Some(runtime_execution),
                    validation: DigestValidationArtifact::default(),
                    event_message: "Proposal artifact is not valid UTF-8".to_owned(),
                });
            }
        };
        let proposal = match serde_json::from_str::<DigestProposal>(&proposal_text) {
            Ok(proposal) => proposal,
            Err(error) => {
                return self.fail(WorkerFailure {
                    phase: RunPhase::ValidatingProposal,
                    headline: "Proposal artifact is invalid JSON",
                    error_code: "proposal_json_invalid",
                    error_message: error.to_string(),
                    runtime: Some(runtime_execution),
                    validation: DigestValidationArtifact {
                        attempted: true,
                        ..DigestValidationArtifact::default()
                    },
                    event_message: "Proposal artifact could not be parsed as JSON".to_owned(),
                });
            }
        };
        let existing_entries = match self.load_existing_entries_for_validation() {
            Ok(existing_entries) => existing_entries,
            Err(error) => {
                return self.fail(WorkerFailure {
                    phase: RunPhase::ValidatingProposal,
                    headline: "Proposal validation setup failed",
                    error_code: "proposal_validation_setup_failed",
                    error_message: error.to_string(),
                    runtime: Some(runtime_execution),
                    validation: DigestValidationArtifact {
                        attempted: true,
                        ..DigestValidationArtifact::default()
                    },
                    event_message: "Failed to load existing wiki entries for validation".to_owned(),
                });
            }
        };
        let allowed_domains = build_allowed_domains(registry, state);
        let allowed_evidence_refs = build_allowed_evidence_refs(context);
        let validation = match validate_digest_proposal(
            &proposal,
            &ProposalValidationOptions {
                project_id: self.project_id,
                run_id: self.run_id.as_str(),
                allowed_categories: &registry.categories,
                allowed_domains: &allowed_domains,
                allowed_evidence_refs: &allowed_evidence_refs,
                existing_entries: &existing_entries,
            },
        ) {
            Ok(summary) => DigestValidationArtifact {
                attempted: true,
                valid: true,
                entry_count: Some(summary.entry_count),
                run_summary_title: Some(summary.run_summary_title),
                extracted_decision_count: Some(summary.extracted_decision_count),
                errors: Vec::new(),
            },
            Err(errors) => {
                let validation = DigestValidationArtifact {
                    attempted: true,
                    valid: false,
                    entry_count: Some(proposal.entries.len()),
                    run_summary_title: Some(proposal.run_summary.title.clone()),
                    extracted_decision_count: Some(proposal.run_summary.extracted_decision_count),
                    errors: errors.into_errors(),
                };
                return self.fail(WorkerFailure {
                    phase: RunPhase::ValidatingProposal,
                    headline: "Proposal validation failed",
                    error_code: "proposal_validation_failed",
                    error_message: "proposal artifact failed validation".to_owned(),
                    runtime: Some(runtime_execution),
                    validation,
                    event_message: "Proposal artifact failed validation".to_owned(),
                });
            }
        };

        Ok(Some(validation))
    }

    /// Loads canonical entry frontmatter so proposal validation can reject duplicates.
    fn load_existing_entries_for_validation(&self) -> Result<Vec<EntryFrontmatter>> {
        list_entries(&self.layout)?
            .into_iter()
            .map(|entry| Ok(load_entry(&entry.path)?.frontmatter))
            .collect()
    }

    /// Writes the terminal success state and result artifact for a validated proposal.
    fn complete(
        &self,
        validation: &DigestValidationArtifact,
        runtime_execution: &RuntimeExecution,
    ) -> Result<()> {
        transition_worker_state(
            &self.layout,
            self.run_id,
            RunPhase::WritingArtifacts,
            Some(100),
            "Writing validation result",
        )?;
        let final_state = finalize_run_succeeded(
            &self.layout,
            self.run_id,
            RunPhase::WritingArtifacts,
            "Validated proposal artifacts",
        )?;
        write_terminal_result(
            &self.layout,
            self.run_id,
            &final_state,
            Some(runtime_execution),
            validation.clone(),
            Some(
                "Canonical merge is deferred; this run succeeded after proposal validation only"
                    .to_owned(),
            ),
        )?;
        append_run_event(
            &self.layout,
            self.run_id,
            RunEvent::info(
                RunPhase::WritingArtifacts,
                format!(
                    "Validated proposal with {} decision trace(s)",
                    validation.extracted_decision_count.unwrap_or_default()
                ),
            ),
        )?;
        Ok(())
    }

    /// Finalizes one failed worker step, writes the terminal result, and emits a warning event.
    fn fail<T>(&self, failure: WorkerFailure<'_>) -> Result<Option<T>> {
        let final_state = finalize_run_failed(
            &self.layout,
            self.run_id,
            failure.phase,
            failure.headline,
            failure.error_code,
            &failure.error_message,
        )?;
        write_terminal_result(
            &self.layout,
            self.run_id,
            &final_state,
            failure.runtime,
            failure.validation,
            None,
        )?;
        append_run_event(
            &self.layout,
            self.run_id,
            RunEvent::warn(failure.phase, failure.event_message),
        )?;
        Ok(None)
    }

    /// Finalizes one canceled worker step and writes the terminal result.
    fn cancel<T>(
        &self,
        phase: RunPhase,
        runtime: Option<&RuntimeExecution>,
        note: Option<String>,
    ) -> Result<Option<T>> {
        let final_state =
            finalize_run_canceled(&self.layout, self.run_id, phase, "Digest run canceled")?;
        write_terminal_result(
            &self.layout,
            self.run_id,
            &final_state,
            runtime,
            DigestValidationArtifact::default(),
            note,
        )?;
        append_run_event(
            &self.layout,
            self.run_id,
            RunEvent::info(phase, "Digest run canceled".to_owned()),
        )?;
        Ok(None)
    }

    /// Finalizes one unexpected worker error so early setup failures still leave durable artifacts.
    fn fail_unhandled_error(&self, error: &anyhow::Error) -> Result<()> {
        let phase = match load_run_state(&self.layout, self.run_id) {
            Ok(state) => {
                if is_finished_status(state.status) {
                    return Ok(());
                }
                state.phase
            }
            Err(_) => RunPhase::PreparingContext,
        };
        let final_state = finalize_run_failed(
            &self.layout,
            self.run_id,
            phase,
            "Digest worker failed",
            "worker_failed",
            &error.to_string(),
        )?;
        write_terminal_result(
            &self.layout,
            self.run_id,
            &final_state,
            None,
            DigestValidationArtifact::default(),
            None,
        )?;
        append_run_event(
            &self.layout,
            self.run_id,
            RunEvent::warn(phase, "Digest worker failed before completion".to_owned()),
        )?;
        Ok(())
    }
}
