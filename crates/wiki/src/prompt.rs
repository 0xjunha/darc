use std::path::Path;

use crate::proposal::{DIGEST_PROPOSAL_OUTPUT_SCHEMA_JSON, DIGEST_PROPOSAL_SCHEMA};

const DIGEST_RUNTIME_PROMPT_TEMPLATE: &str = include_str!("../templates/digest_runtime_prompt.md");

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
        prompt: render_digest_runtime_prompt_template([
            ("darc_root", darc_root.as_str()),
            ("schema", DIGEST_PROPOSAL_SCHEMA),
            ("project_id", project_id),
            ("run_id", run_id),
            ("selected_session_refs", selected_session_refs.as_str()),
            ("target_categories", target_categories.as_str()),
            ("target_domains", target_domains.as_str()),
        ]),
        schema_json: DIGEST_PROPOSAL_OUTPUT_SCHEMA_JSON.to_owned(),
    }
}

/// Renders the embedded digest runtime prompt markdown template.
fn render_digest_runtime_prompt_template<'a>(
    replacements: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> String {
    let mut rendered = DIGEST_RUNTIME_PROMPT_TEMPLATE.to_owned();
    for (key, value) in replacements {
        rendered = rendered.replace(&format!("{{{{{key}}}}}"), value);
    }
    debug_assert!(
        !rendered.contains("{{"),
        "digest runtime prompt template contains unexpanded placeholders"
    );
    rendered
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
                .contains(
                    "The extractor may inspect and cite non-seed sessions, but every proposed entry must stay anchored in at least one evidence reference from `selected_session_refs`."
                )
        );
        assert!(!prompt.prompt.contains("Context bundle:"));
        assert!(!prompt.prompt.contains("only source of truth"));
    }

    #[test]
    fn build_digest_runtime_prompt_includes_finality_and_plain_language_rules() {
        let prompt = build_digest_runtime_prompt(
            Path::new("/tmp/darc-root"),
            "repo-123",
            "cwrun_123",
            &["codex:session-1".to_owned()],
            &[],
            &[],
        );
        assert!(
            prompt
                .prompt
                .contains("Capture only decisions that were actually chosen and still shape the current codebase or project state.")
        );
        assert!(
            prompt
                .prompt
                .contains("Do not record ideas that were only discussed, proposed, partially implemented, later discarded, or later reversed.")
        );
        assert!(
            prompt
                .prompt
                .contains("Use plain language. Prefer short sentences, common words, and concrete wording over jargon or inflated abstractions.")
        );
        assert!(
            prompt
                .prompt
                .contains("Prefer zero entries over weak, speculative, or routine entries.")
        );
    }
}
