use crate::proposal::{DIGEST_PROPOSAL_OUTPUT_SCHEMA_JSON, DIGEST_PROPOSAL_SCHEMA};

/// Stores the shared prompt contract for one digest runtime invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestRuntimePrompt {
    pub prompt: String,
    pub schema_json: String,
}

/// Builds the shared digest prompt and schema payload for one runtime invocation.
pub fn build_digest_runtime_prompt(
    context_json: &str,
    project_id: &str,
    run_id: &str,
) -> DigestRuntimePrompt {
    DigestRuntimePrompt {
        prompt: format!(
            concat!(
                "You are generating a Context Wiki digest proposal for Darc.\n\n",
                "Return exactly one JSON object that matches the provided output schema.\n",
                "Do not return Markdown, prose, code fences, or commentary.\n\n",
                "Rules:\n",
                "- Set `schema` to `{schema}`.\n",
                "- Set `project_id` to `{project_id}`.\n",
                "- Set `run_id` to `{run_id}`.\n",
                "- The only allowed entry type is `decision_trace`.\n",
                "- The only allowed operation is `create`.\n",
                "- Use only categories from `registry.categories`.\n",
                "- Use only domains from `registry.domains`.\n",
                "- Treat `target_domains` as prioritization hints, not as new allowed domains.\n",
                "- Evidence references must use `<provider>:<session-id>#<turn-ordinal>` and only reference selected sessions.\n",
                "- It is valid to return zero entries when the context does not contain durable decisions.\n",
                "- Always include `run_summary`, even when `entries` is empty.\n",
                "- Set `run_summary.extracted_decision_count` to the number of entries you return.\n\n",
                "Context bundle:\n{context_json}\n"
            ),
            schema = DIGEST_PROPOSAL_SCHEMA,
            project_id = project_id,
            run_id = run_id,
            context_json = context_json,
        ),
        schema_json: DIGEST_PROPOSAL_OUTPUT_SCHEMA_JSON.to_owned(),
    }
}
