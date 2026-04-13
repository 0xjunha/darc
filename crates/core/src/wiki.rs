use std::path::PathBuf;

use anyhow::{Context, Result};
use darc_wiki::{
    ContextWikiLayout, ProjectLayout, ProjectRegistry, RunId, RunState, list_digests, list_entries,
    list_runs, load_registry, load_run_state, store_run_state,
};

use crate::{default_root_path, project::registered_projects};

/// Collects the empty or populated read-side wiki payload for one configured project.
#[derive(Debug, Clone)]
pub struct ProjectWikiData {
    pub project_id: String,
    pub layout: ProjectLayout,
    pub registry: ProjectRegistry,
    pub entries: Vec<darc_wiki::EntrySummary>,
    pub digests: Vec<darc_wiki::DigestSummary>,
    pub runs: Vec<darc_wiki::RunSummary>,
}

/// Ensures the per-project wiki directory tree exists for one configured project.
pub fn ensure_project_wiki(root: Option<PathBuf>, project_id: &str) -> Result<ProjectLayout> {
    let layout = resolve_project_layout(root, project_id)?;
    let top_level = context_wiki_layout(&layout);
    top_level.ensure_root()?;
    load_registry(&layout).context("failed to initialize project wiki registry")?;
    Ok(layout)
}

/// Loads the read-side wiki payload after ensuring the project layout exists.
pub fn load_project_wiki(root: Option<PathBuf>, project_id: &str) -> Result<ProjectWikiData> {
    let layout = ensure_project_wiki(root, project_id)?;
    Ok(ProjectWikiData {
        project_id: project_id.to_owned(),
        registry: load_registry(&layout).context("failed to load project wiki registry")?,
        entries: list_entries(&layout).context("failed to list wiki entries")?,
        digests: list_digests(&layout).context("failed to list wiki digests")?,
        runs: list_runs(&layout).context("failed to list wiki runs")?,
        layout,
    })
}

/// Loads one durable wiki run state for one configured project.
pub fn load_project_wiki_run(
    root: Option<PathBuf>,
    project_id: &str,
    run_id: &RunId,
) -> Result<RunState> {
    let layout = ensure_project_wiki(root, project_id)?;
    load_run_state(&layout, run_id).context("failed to load wiki run state")
}

/// Stores one durable wiki run state for one configured project.
pub fn store_project_wiki_run(
    root: Option<PathBuf>,
    project_id: &str,
    run_state: &RunState,
) -> Result<()> {
    let layout = ensure_project_wiki(root, project_id)?;
    store_run_state(&layout, run_state).context("failed to store wiki run state")
}

/// Resolves one validated project wiki layout from the configured Darc root.
fn resolve_project_layout(root: Option<PathBuf>, project_id: &str) -> Result<ProjectLayout> {
    let root = root.unwrap_or_else(default_root_path);
    let project = registered_projects(&root)?
        .into_iter()
        .find(|project| project.id == project_id)
        .with_context(|| format!("project id `{project_id}` was not found in the shared config"))?;
    Ok(ContextWikiLayout::new(root).project_layout(project.id))
}

/// Rebuilds the top-level wiki layout from one project-specific layout.
fn context_wiki_layout(layout: &ProjectLayout) -> ContextWikiLayout {
    let darc_root = layout
        .root
        .ancestors()
        .nth(3)
        .expect("project wiki layout should have the darc root as an ancestor")
        .to_path_buf();
    ContextWikiLayout::new(darc_root)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, path::PathBuf};

    use anyhow::Result;
    use darc_test_utils::unique_test_dir;

    use super::*;
    use crate::{
        config::{ProjectConfig, SharedConfig, SourcesConfig},
        constants::CONFIG_FILE_NAME,
    };

    /// Writes one minimal shared config fixture for wiki backend tests.
    fn write_config(root: &Path, config: &SharedConfig) -> Result<()> {
        fs::create_dir_all(root)?;
        fs::write(root.join(CONFIG_FILE_NAME), toml::to_string_pretty(config)?)?;
        Ok(())
    }

    /// Builds one minimal configured project fixture for wiki backend tests.
    fn build_project(root: &Path, project_id: &str, project_root: PathBuf) -> ProjectConfig {
        ProjectConfig {
            id: project_id.to_owned(),
            name: "repo".to_owned(),
            local_path: project_root,
            git_upstream: None,
            sessions_root: root.join(format!("projects/{project_id}/sessions")),
            known_paths: Vec::new(),
        }
    }

    #[test]
    fn backend_creates_empty_project_wiki_and_lists_zero_state() -> Result<()> {
        let root = unique_test_dir("core-wiki-empty");
        let project_root = root.join("repo");
        let project_id = "repo-123";
        fs::create_dir_all(&project_root)?;
        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![build_project(&root, project_id, project_root)],
                SourcesConfig::default(),
            ),
        )?;

        let layout = ensure_project_wiki(Some(root.clone()), project_id)?;
        assert_eq!(
            layout.root,
            root.join("context-wiki").join("projects").join(project_id)
        );
        assert!(root.join("context-wiki/VERSION").exists());
        assert!(!root.join("context-wiki/context-wiki").exists());
        assert!(layout.registry_dir.exists());
        assert!(layout.categories_path.exists());
        assert!(layout.domains_path.exists());
        assert!(layout.entries_dir.exists());
        assert!(layout.digests_dir.exists());
        assert!(layout.runs_dir.exists());

        let wiki = load_project_wiki(Some(root.clone()), project_id)?;
        assert_eq!(wiki.project_id, project_id);
        assert_eq!(wiki.registry.categories, crate::DEFAULT_CATEGORY_IDS);
        assert!(wiki.registry.domains.is_empty());
        assert!(wiki.entries.is_empty());
        assert!(wiki.digests.is_empty());
        assert!(wiki.runs.is_empty());

        fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn backend_round_trips_run_state_through_core_wiring() -> Result<()> {
        let root = unique_test_dir("core-wiki-run");
        let project_root = root.join("repo");
        let project_id = "repo-123";
        fs::create_dir_all(&project_root)?;
        write_config(
            &root,
            &SharedConfig::new(
                root.clone(),
                vec![build_project(&root, project_id, project_root)],
                SourcesConfig::default(),
            ),
        )?;

        let run_state = crate::RunState {
            schema_version: 1,
            run_id: crate::RunId::new("cwrun_01backend")?,
            project_id: project_id.to_owned(),
            status: crate::RunStatus::Queued,
            phase: crate::RunPhase::PreparingContext,
            created_at: "2026-04-13T10:00:00Z".to_owned(),
            started_at: None,
            updated_at: "2026-04-13T10:00:00Z".to_owned(),
            finished_at: None,
            heartbeat_at: None,
            requested_by: Some("desktop".to_owned()),
            request_source: Some("darc-desktop/0.1.0".to_owned()),
            attempt: 1,
            cancel_requested: false,
            pid: None,
            agent_id: None,
            runtime: None,
            model: None,
            auth_profile: None,
            selected_sessions: Vec::new(),
            target_categories: vec!["architecture".to_owned()],
            target_domains: Vec::new(),
            progress_percent: None,
            headline: Some("Queued".to_owned()),
            proposal_path: None,
            result_path: None,
            events_path: None,
            stdout_log_path: None,
            stderr_log_path: None,
            created_entry_ids: Vec::new(),
            updated_entry_ids: Vec::new(),
            digest_id: None,
            error_code: None,
            error_message: None,
        };

        store_project_wiki_run(Some(root.clone()), project_id, &run_state)?;

        let loaded = load_project_wiki_run(Some(root.clone()), project_id, &run_state.run_id)?;
        assert_eq!(loaded, run_state);

        let wiki = load_project_wiki(Some(root.clone()), project_id)?;
        assert_eq!(wiki.runs.len(), 1);
        assert_eq!(wiki.runs[0].run_id, run_state.run_id);
        assert_eq!(wiki.runs[0].headline.as_deref(), Some("Queued"));

        fs::remove_dir_all(&root)?;
        Ok(())
    }
}
