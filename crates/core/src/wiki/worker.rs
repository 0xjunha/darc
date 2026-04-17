use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use darc_agent::{RuntimeCommand, build_runtime_command};
use darc_index::INDEX_DB_FILE_NAME;
use darc_query::TurnExistenceResolver;
use darc_wiki::{
    DigestProposal, DigestRuntimePrompt, EvidenceReference, MergeDigestArtifacts, ProjectLayout,
    ProjectRegistry, ProposalValidationOptions, RunId, RunPhase, RunState,
    build_digest_runtime_prompt, load_registry, load_run_state, merge_digest_proposal,
    validate_digest_proposal,
};

use super::{
    artifacts::{
        append_run_event, ensure_shared_text_artifact, write_bytes_artifact, write_terminal_result,
    },
    models::{DigestValidationArtifact, RunEvent, RuntimeExecution},
    runtime::{build_runtime_request, execute_runtime_command},
    state::{
        build_succeeded_run_state, cancel_requested, finalize_run_canceled, finalize_run_failed,
        is_finished_status, transition_worker_state, wait_for_worker_registration,
        with_locked_run_state,
    },
};
use crate::default_root_path;

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
    project_root: PathBuf,
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

/// Stores the parsed proposal plus validation summary carried into merge/write.
struct ValidatedProposal {
    proposal: DigestProposal,
    artifact: DigestValidationArtifact,
}

impl<'a> DigestWorker<'a> {
    /// Builds one digest worker wrapper from the configured Darc root and run id.
    fn new(root: Option<PathBuf>, project_id: &'a str, run_id: &'a RunId) -> Result<Self> {
        let root = root.unwrap_or_else(default_root_path);
        let (layout, project_root) =
            super::api::resolve_project_layout_and_root(Some(root.clone()), project_id)?;
        Ok(Self {
            root,
            project_root,
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

        let state = load_run_state(&self.layout, self.run_id)?;
        let runtime_execution = match self.execute_runtime(&state)? {
            Some(runtime_execution) => runtime_execution,
            None => return Ok(()),
        };

        let registry =
            load_registry(&self.layout).context("failed to load project wiki registry")?;
        let validated = match self.validate_proposal(&registry, &runtime_execution)? {
            Some(validated) => validated,
            None => return Ok(()),
        };

        self.complete(&validated, &runtime_execution)
    }

    /// Prepares the runtime command, executes it, and captures its proposal artifact.
    fn execute_runtime(&self, state: &RunState) -> Result<Option<RuntimeExecution>> {
        transition_worker_state(
            &self.layout,
            self.run_id,
            RunPhase::WaitingForAgent,
            Some(40),
            "Preparing agent runtime",
        )?;
        let proposal_schema_path = self.layout.digest_proposal_schema_path();
        let proposal_schema_lock_path = self.layout.digest_proposal_schema_lock_path();
        let prompt = build_digest_runtime_prompt(
            &self.root,
            self.project_id,
            self.run_id.as_str(),
            &state.selected_sessions,
            &state.target_categories,
            &state.target_domains,
        );
        ensure_shared_text_artifact(
            &proposal_schema_path,
            &proposal_schema_lock_path,
            &prompt.schema_json,
        )?;

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

        if runtime_execution.proposal_source.captures_stdout() {
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
            &self.layout,
            self.run_id,
            state,
            prompt,
            proposal_schema_path,
            &self.project_root,
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

    /// Validates the captured proposal artifact against Darc's schema and indexed evidence rules.
    fn validate_proposal(
        &self,
        registry: &ProjectRegistry,
        runtime_execution: &RuntimeExecution,
    ) -> Result<Option<ValidatedProposal>> {
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
        let evidence_resolver =
            match TurnExistenceResolver::open(&self.root.join(INDEX_DB_FILE_NAME)) {
                Ok(resolver) => resolver,
                Err(error) => {
                    return self.fail(WorkerFailure {
                        phase: RunPhase::ValidatingProposal,
                        headline: "Proposal validation failed",
                        error_code: "proposal_validation_failed",
                        error_message: error.to_string(),
                        runtime: Some(runtime_execution),
                        validation: DigestValidationArtifact {
                            attempted: true,
                            ..DigestValidationArtifact::default()
                        },
                        event_message: "Failed to prepare proposal evidence resolver".to_owned(),
                    });
                }
            };
        let mut resolved_evidence = BTreeMap::new();
        let artifact = match validate_digest_proposal(
            &proposal,
            &ProposalValidationOptions {
                project_id: self.project_id,
                run_id: self.run_id.as_str(),
                allowed_categories: &registry.categories,
                allowed_domains: &registry.domains,
            },
            &mut |reference: &EvidenceReference<'_>| {
                let cache_key = format!(
                    "{}:{}#{}",
                    reference.session.provider.directory_name(),
                    reference.session.session_id,
                    reference.turn_ordinal
                );
                if let Some(exists) = resolved_evidence.get(cache_key.as_str()) {
                    return Ok(*exists);
                }
                let exists = evidence_resolver
                    .turn_exists(
                        self.project_id,
                        reference.session.provider,
                        reference.session.session_id,
                        reference.turn_ordinal,
                    )
                    .map_err(|error| error.to_string())?;
                resolved_evidence.insert(cache_key, exists);
                Ok(exists)
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

        Ok(Some(ValidatedProposal { proposal, artifact }))
    }

    /// Merges canonical artifacts, then writes the terminal success state and result artifact.
    fn complete(
        &self,
        validated: &ValidatedProposal,
        runtime_execution: &RuntimeExecution,
    ) -> Result<()> {
        transition_worker_state(
            &self.layout,
            self.run_id,
            RunPhase::MergingEntries,
            Some(90),
            "Merging canonical wiki artifacts",
        )?;
        let merge = match merge_digest_proposal(&self.layout, self.run_id, &validated.proposal) {
            Ok(merge) => merge,
            Err(error) => {
                self.fail::<()>(WorkerFailure {
                    phase: RunPhase::MergingEntries,
                    headline: "Canonical artifact merge failed",
                    error_code: "artifact_merge_failed",
                    error_message: error.to_string(),
                    runtime: Some(runtime_execution),
                    validation: validated.artifact.clone(),
                    event_message: "Failed to merge canonical wiki artifacts".to_owned(),
                })?;
                return Ok(());
            }
        };
        append_run_event(
            &self.layout,
            self.run_id,
            RunEvent::info(RunPhase::MergingEntries, merge_event_message(&merge)),
        )?;

        transition_worker_state(
            &self.layout,
            self.run_id,
            RunPhase::WritingArtifacts,
            Some(100),
            "Writing final result artifacts",
        )?;
        with_locked_run_state(&self.layout, self.run_id, |state| {
            let final_state = build_succeeded_run_state(
                state.clone(),
                RunPhase::WritingArtifacts,
                "Wrote canonical wiki artifacts",
                &merge.created_entry_ids,
                &merge.updated_entry_ids,
                &merge.digest_id,
            );
            write_terminal_result(
                &self.layout,
                self.run_id,
                &final_state,
                Some(runtime_execution),
                validated.artifact.clone(),
                None,
            )?;
            *state = final_state.clone();
            Ok(final_state)
        })?;
        append_run_event(
            &self.layout,
            self.run_id,
            RunEvent::info(
                RunPhase::WritingArtifacts,
                format!(
                    "Persisted digest `{}` with {} created and {} updated decision trace(s)",
                    merge.digest_id,
                    merge.created_entry_ids.len(),
                    merge.updated_entry_ids.len()
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

/// Renders one concise merge summary for the run events log.
fn merge_event_message(merge: &MergeDigestArtifacts) -> String {
    format!(
        "Merged canonical artifacts into digest `{}` ({} created, {} updated)",
        merge.digest_id,
        merge.created_entry_ids.len(),
        merge.updated_entry_ids.len()
    )
}
