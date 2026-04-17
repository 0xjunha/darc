use std::path::Path;

use crate::proposal::{DIGEST_PROPOSAL_OUTPUT_SCHEMA_JSON, DIGEST_PROPOSAL_SCHEMA};

/// Stores the shared prompt contract for one digest runtime invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestRuntimePrompt {
    pub prompt: String,
    pub schema_json: String,
}

/// Builds the shared digest prompt and schema payload for one runtime invocation.
pub fn build_digest_runtime_prompt(
    darc_root: &Path,
    project_id: &str,
    run_id: &str,
    selected_session_refs: &[String],
    target_categories: &[String],
    target_domains: &[String],
) -> DigestRuntimePrompt {
    let darc_root = shell_quote(darc_root.to_string_lossy().as_ref());
    let selected_session_refs =
        render_metadata_list("selected_session_refs", selected_session_refs);
    let target_categories = render_metadata_list("target_categories", target_categories);
    let target_domains = render_metadata_list("target_domains", target_domains);
    DigestRuntimePrompt {
        prompt: format!(
            concat!(
                "You are generating a Context Wiki digest proposal for Darc.\n\n",
                "Return exactly one JSON object that matches the runtime-provided output schema.\n",
                "Do not return Markdown, prose, code fences, or commentary.\n\n",
                "Run metadata\n",
                "project_id: {project_id}\n",
                "run_id: {run_id}\n",
                "{selected_session_refs}\n",
                "{target_categories}\n",
                "{target_domains}\n\n",
                "Rules:\n",
                "- Set `schema` to `{schema}`.\n",
                "- Set `project_id` to `{project_id}`.\n",
                "- Set `run_id` to `{run_id}`.\n",
                "- The only allowed entry type is `decision_trace`.\n",
                "- The only allowed operation is `create`.\n",
                "- Categories and domains must come from the registry the extractor reads at runtime.\n",
                "- Treat `target_categories` and `target_domains` as prioritization hints only.\n",
                "- Evidence references must use `<provider>:<session-id>#<turn-ordinal>`.\n",
                "- `selected_session_refs` are focus hints, not a hard scope boundary.\n",
                "- The extractor may read and cite evidence from non-seed sessions.\n",
                "- Prefer `session-bundle --view narrative` for seed-session deep reads.\n",
                "- Prefer `query turns --grep` plus `--context` for discovery before expanding to full session reads.\n",
                "- Always check `wiki registry` and `wiki entries` before proposing duplicates.\n",
                "- It is valid to return zero entries when the inspected evidence does not contain durable decisions.\n",
                "- Always include `run_summary`, even when `entries` is empty.\n",
                "- Set `run_summary.extracted_decision_count` to the number of entries you return.\n\n",
                "Curated playbook\n",
                "Bootstrap registry before drafting entries:\n",
                "```bash\n",
                "darc query wiki registry --root {darc_root} --project-id <project_id> --json\n",
                "```\n",
                "Read each seed session deeply with narrative turn view:\n",
                "```bash\n",
                "darc query session-bundle --root {darc_root} --project-id <project_id> --provider <provider> --session-id <session_id> --view narrative --json\n",
                "```\n",
                "Search decision-shaped language across sessions before expanding to full reads:\n",
                "```bash\n",
                "darc query turns --root {darc_root} --project-id <project_id> --grep \"<text>\" --role both --context 1 --view oneline --json\n",
                "darc query turns --root {darc_root} --project-id <project_id> --grep \"<text>\" --role both --context 1 --touched-path \"<glob>\" --view oneline --json\n",
                "```\n",
                "Follow file arcs across sessions:\n",
                "```bash\n",
                "darc query sessions --root {darc_root} --project-id <project_id> --touched-path \"<glob>\" --json\n",
                "darc query files --root {darc_root} --project-id <project_id> --path \"<glob>\" --json\n",
                "darc query files --root {darc_root} --project-id <project_id> --co-touched-with \"<path>\" --limit 20 --json\n",
                "darc query session-files --root {darc_root} --project-id <project_id> --provider <provider> --session-id <session_id> --json\n",
                "```\n",
                "Check existing wiki coverage before proposing duplicates:\n",
                "```bash\n",
                "darc query wiki entries --root {darc_root} --project-id <project_id> --grep \"<text>\" --json\n",
                "darc query wiki entries --root {darc_root} --project-id <project_id> --evidence-ref <provider>:<session-id>#<turn-ordinal> --json\n",
                "darc query wiki entries --root {darc_root} --project-id <project_id> --covers-session <provider>:<session-id> --json\n",
                "```\n",
                "Verify claims against the repository and history when needed:\n",
                "```bash\n",
                "rg -n \"<text>\" <path-or-glob>\n",
                "git log -- <path>\n",
                "git show <rev>\n",
                "git diff <rev_a>..<rev_b> -- <path>\n",
                "```\n"
            ),
            darc_root = darc_root,
            schema = DIGEST_PROPOSAL_SCHEMA,
            project_id = project_id,
            run_id = run_id,
            selected_session_refs = selected_session_refs,
            target_categories = target_categories,
            target_domains = target_domains,
        ),
        schema_json: DIGEST_PROPOSAL_OUTPUT_SCHEMA_JSON.to_owned(),
    }
}

/// Renders one run-metadata list block for the digest prompt.
fn render_metadata_list(label: &str, values: &[String]) -> String {
    if values.is_empty() {
        format!("{label}: []")
    } else {
        format!("{label}:\n- {}", values.join("\n- "))
    }
}

/// Quotes one shell argument for prompt examples.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::build_digest_runtime_prompt;

    #[test]
    fn build_digest_runtime_prompt_renders_metadata_lists() {
        let prompt = build_digest_runtime_prompt(
            Path::new("/tmp/darc-root"),
            "repo-123",
            "cwrun_123",
            &["codex:session-1".to_owned(), "claude:session-2".to_owned()],
            &["product".to_owned(), "architecture".to_owned()],
            &["query-protocol".to_owned(), "runtime".to_owned()],
        );
        assert!(prompt.prompt.contains(concat!(
            "Run metadata\n",
            "project_id: repo-123\n",
            "run_id: cwrun_123\n",
            "selected_session_refs:\n",
            "- codex:session-1\n",
            "- claude:session-2\n",
            "target_categories:\n",
            "- product\n",
            "- architecture\n",
            "target_domains:\n",
            "- query-protocol\n",
            "- runtime"
        )));
    }

    #[test]
    fn build_digest_runtime_prompt_renders_empty_metadata_lists_inline() {
        let prompt = build_digest_runtime_prompt(
            Path::new("/tmp/darc-root"),
            "repo-123",
            "cwrun_123",
            &[],
            &[],
            &[],
        );
        assert!(prompt.prompt.contains("selected_session_refs: []"));
        assert!(prompt.prompt.contains("target_categories: []"));
        assert!(prompt.prompt.contains("target_domains: []"));
    }

    #[test]
    fn build_digest_runtime_prompt_includes_curated_playbook_and_new_scope_rules() {
        let prompt = build_digest_runtime_prompt(
            Path::new("/tmp/darc root"),
            "repo-123",
            "cwrun_123",
            &[],
            &[],
            &[],
        );
        assert!(prompt.prompt.contains("Curated playbook"));
        assert!(prompt.prompt.contains(
            "darc query wiki registry --root '/tmp/darc root' --project-id <project_id> --json"
        ));
        assert!(prompt.prompt.contains(
            "darc query session-bundle --root '/tmp/darc root' --project-id <project_id> --provider <provider> --session-id <session_id> --view narrative --json"
        ));
        assert!(
            prompt
                .prompt
                .contains("`selected_session_refs` are focus hints, not a hard scope boundary")
        );
        assert!(
            prompt
                .prompt
                .contains("The extractor may read and cite evidence from non-seed sessions.")
        );
        assert!(!prompt.prompt.contains("Context bundle:"));
        assert!(!prompt.prompt.contains("only source of truth"));
    }
}
