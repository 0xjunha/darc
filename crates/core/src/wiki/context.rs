use std::path::Path;

use anyhow::{Context, Result, bail};
use darc_agent::{AgentId, RuntimeKind};
use darc_paths::SourceKind;
use darc_wiki::{
    ProjectLayout, ProjectRegistry, is_valid_domain_id, load_registry, parse_session_reference,
};
use serde::ser::{Serialize, SerializeStruct, Serializer};

use super::{
    DigestStartOptions,
    models::{DigestContextArtifact, DigestContextSession},
};
use crate::query::{SessionSummary, TurnDetail, TurnDetailOptions, query_session_turn_details};

/// Validates the new digest start request before any artifact is written.
pub(super) fn validate_digest_start_options(options: &DigestStartOptions) -> Result<()> {
    if options.session_refs.is_empty() {
        bail!("at least one --session-ref is required");
    }
    for session_ref in &options.session_refs {
        validate_session_ref(session_ref)?;
    }
    AgentId::parse(&options.agent_id).context("agent id is not supported")?;
    RuntimeKind::parse(&options.runtime).context("runtime is not supported")?;
    if options.model.trim().is_empty() {
        bail!("model must not be empty");
    }
    Ok(())
}

/// Validates one `provider:session-id` reference used to select wiki digest sessions.
pub(super) fn validate_session_ref(session_ref: &str) -> Result<()> {
    let (_, session_id) = parse_session_ref(session_ref)?;
    if session_id.trim().is_empty() {
        bail!("session ref `{session_ref}` must include a non-empty session id");
    }
    Ok(())
}

/// Validates the target categories and domains recorded for one digest request.
pub(super) fn validate_digest_targets(
    layout: &ProjectLayout,
    options: &DigestStartOptions,
) -> Result<()> {
    let registry = load_registry(layout)?;
    for category in &options.target_categories {
        if !registry.categories.contains(category) {
            bail!("target category `{category}` is not defined in the project registry");
        }
    }
    for domain in &options.target_domains {
        if !is_valid_domain_id(domain) {
            bail!("target domain `{domain}` must use lowercase slug format");
        }
        if !registry.domains.contains(domain) {
            bail!("target domain `{domain}` is not defined in the project registry");
        }
    }
    Ok(())
}

/// Parses one selected session reference into its typed source kind and session id.
pub(super) fn parse_session_ref(session_ref: &str) -> Result<(SourceKind, &str)> {
    let Some(reference) = parse_session_reference(session_ref) else {
        if !session_ref.contains(':') {
            bail!("session ref `{session_ref}` must use the `<provider>:<session-id>` format");
        }
        bail!("session ref `{session_ref}` must start with `claude:` or `codex:`");
    };
    Ok((reference.provider, reference.session_id))
}

/// Loads one selected session plus its narrative turn details for the digest context bundle.
pub(super) fn load_selected_session_context<F>(
    root: &Path,
    project_id: &str,
    session_ref: &str,
    session_summaries: &[SessionSummary],
    mut on_turn_progress: F,
) -> Result<DigestContextSession>
where
    F: FnMut() -> Result<()>,
{
    let (provider, session_id) = parse_session_ref(session_ref)?;
    let session = session_summaries
        .iter()
        .find(|session| session.provider == provider && session.session_id == session_id)
        .cloned()
        .with_context(|| format!("selected session `{session_ref}` was not found in the index"))?;
    on_turn_progress()?;
    let turns = query_session_turn_details(
        Some(root.to_path_buf()),
        project_id,
        provider,
        session_id,
        TurnDetailOptions {
            include_raw: false,
            include_insights: false,
            narrative: true,
        },
    )
    .with_context(|| format!("failed to load narrative turns for `{session_ref}`"))?;
    on_turn_progress()?;
    Ok(DigestContextSession { session, turns })
}

/// Builds the compact runtime context JSON passed to one digest extraction agent.
pub(super) fn build_runtime_context_json(context: &DigestContextArtifact) -> Result<String> {
    serde_json::to_string(&RuntimePromptContext { context })
        .context("failed to serialize compact digest runtime context JSON")
}

/// Builds the allowed proposal domain list from the persisted project registry.
pub(super) fn build_allowed_domains(registry: &ProjectRegistry) -> Vec<String> {
    registry.domains.clone()
}

/// Serializes the extraction-only prompt view for one digest context artifact.
struct RuntimePromptContext<'a> {
    context: &'a DigestContextArtifact,
}

impl Serialize for RuntimePromptContext<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("RuntimePromptContext", 7)?;
        state.serialize_field("project_id", &self.context.project_id)?;
        state.serialize_field("run_id", &self.context.run_id)?;
        state.serialize_field("selected_sessions", &self.context.selected_sessions)?;
        state.serialize_field("target_categories", &self.context.target_categories)?;
        state.serialize_field("target_domains", &self.context.target_domains)?;
        state.serialize_field(
            "registry",
            &RuntimePromptRegistry {
                registry: &self.context.registry,
            },
        )?;
        state.serialize_field(
            "sessions",
            &self
                .context
                .sessions
                .iter()
                .map(|session| RuntimePromptSession { session })
                .collect::<Vec<_>>(),
        )?;
        state.end()
    }
}

/// Serializes the registry fields needed by one digest extraction prompt.
struct RuntimePromptRegistry<'a> {
    registry: &'a ProjectRegistry,
}

impl Serialize for RuntimePromptRegistry<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("RuntimePromptRegistry", 2)?;
        state.serialize_field("categories", &self.registry.categories)?;
        state.serialize_field("domains", &self.registry.domains)?;
        state.end()
    }
}

/// Serializes one selected session for the digest extraction prompt.
struct RuntimePromptSession<'a> {
    session: &'a DigestContextSession,
}

impl Serialize for RuntimePromptSession<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("RuntimePromptSession", 3)?;
        state.serialize_field("provider", &self.session.session.provider)?;
        state.serialize_field("session_id", &self.session.session.session_id)?;
        state.serialize_field(
            "turns",
            &self
                .session
                .turns
                .iter()
                .map(|turn| RuntimePromptTurn { turn })
                .collect::<Vec<_>>(),
        )?;
        state.end()
    }
}

/// Serializes one narrative turn for the digest extraction prompt.
struct RuntimePromptTurn<'a> {
    turn: &'a TurnDetail,
}

impl Serialize for RuntimePromptTurn<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("RuntimePromptTurn", 6)?;
        state.serialize_field("turn_ordinal", &self.turn.turn_ordinal)?;
        state.serialize_field("started_at", &self.turn.started_at)?;
        state.serialize_field("completed_at", &self.turn.completed_at)?;
        state.serialize_field("user_message", &self.turn.user_message)?;
        state.serialize_field("final_answer_text", &self.turn.final_answer_text)?;
        state.serialize_field("steps", &self.turn.steps)?;
        state.end()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Result;
    use darc_index::open_index_database;
    use darc_paths::SourceKind;
    use darc_test_utils::{
        IndexedSessionFixture, IndexedTurnFixture, insert_indexed_session, insert_indexed_turn,
        unique_test_dir,
    };
    use serde_json::Value;

    use super::{build_runtime_context_json, load_selected_session_context};
    use crate::wiki::models::{DigestContextArtifact, DigestContextSession};
    use crate::{
        config::{ProjectConfig, SharedConfig, SourcesConfig},
        constants::CONFIG_FILE_NAME,
        query::query_sessions,
    };

    /// Writes one minimal config fixture for one project-scoped context test.
    fn write_config(root: &std::path::Path, project_id: &str) -> Result<()> {
        fs::create_dir_all(root)?;
        let project_root = root.join("repo");
        fs::create_dir_all(&project_root)?;
        fs::write(
            root.join(CONFIG_FILE_NAME),
            toml::to_string_pretty(&SharedConfig::new(
                root.to_path_buf(),
                vec![ProjectConfig {
                    id: project_id.to_owned(),
                    name: "repo".to_owned(),
                    local_path: project_root,
                    git_upstream: None,
                    sessions_root: root.join(format!("projects/{project_id}/sessions")),
                    known_paths: Vec::new(),
                }],
                SourcesConfig::default(),
            ))?,
        )?;
        Ok(())
    }

    #[test]
    fn runtime_context_json_omits_persisted_runtime_noise() -> Result<()> {
        let root = unique_test_dir("runtime-context-json");
        let project_id = "repo-123";
        write_config(&root, project_id)?;
        let connection = open_index_database(&root.join("index.sqlite"))?;
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new(project_id, SourceKind::Codex, "session-1", "/tmp/repo"),
        )?;
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture::new(
                project_id,
                SourceKind::Codex,
                "session-1",
                4,
                "2026-04-13T10:00:00Z",
                "completed",
                r##"[{"type":"tool_call","timestamp":"2026-04-13T10:00:10Z","call_id":"call-1","name":"Read","arguments":"{\"file_path\":\"README.md\"}"},{"type":"tool_call_output","timestamp":"2026-04-13T10:00:11Z","call_id":"call-1","output":"# README"}]"##,
            ),
        )?;

        let session_summaries = query_sessions(Some(root.clone()), project_id, None, None, None)?;
        let session = load_selected_session_context(
            &root,
            project_id,
            "codex:session-1",
            &session_summaries.sessions,
            || Ok(()),
        )?;
        let context = DigestContextArtifact {
            schema: "darc.wiki.digest.context.v1".to_owned(),
            project_id: project_id.to_owned(),
            run_id: "cwrun_123".to_owned(),
            selected_sessions: vec!["codex:session-1".to_owned()],
            target_categories: vec!["product".to_owned()],
            target_domains: vec!["query".to_owned()],
            registry: darc_wiki::ProjectRegistry {
                schema_version: 1,
                categories: vec!["product".to_owned()],
                domains: vec!["query".to_owned()],
            },
            sessions: vec![DigestContextSession {
                session: session.session,
                turns: session.turns,
            }],
            generated_at: "2026-04-13T10:02:00Z".to_owned(),
        };

        let json = build_runtime_context_json(&context)?;
        let value: Value = serde_json::from_str(&json)?;

        assert_eq!(value["project_id"], "repo-123");
        assert!(value.get("schema").is_none());
        assert!(value.get("generated_at").is_none());
        assert!(value["registry"].get("schema_version").is_none());
        assert!(value["sessions"][0].get("cwd").is_none());
        assert!(value["sessions"][0]["turns"][0].get("project_id").is_none());
        assert!(
            value["sessions"][0]["turns"][0]
                .get("raw_steps_json")
                .is_none()
        );
        assert!(value["sessions"][0]["turns"][0].get("insights").is_none());
        assert!(matches!(
            &value["sessions"][0]["turns"][0]["steps"][0],
            Value::Object(step) if step.get("arguments").is_some()
                && step["arguments"] == Value::String(String::new())
        ));

        fs::remove_dir_all(root)?;
        Ok(())
    }
}
