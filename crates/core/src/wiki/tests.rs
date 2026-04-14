use std::{fs, path::Path, path::PathBuf};

use anyhow::Result;
use darc_test_utils::unique_test_dir;
use darc_wiki::{RunId, RunPhase, RunState, RunStatus, store_run_state};

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

    let run_state = RunState {
        schema_version: 1,
        run_id: RunId::new("cwrun_01backend")?,
        project_id: project_id.to_owned(),
        status: RunStatus::Queued,
        phase: RunPhase::PreparingContext,
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

#[test]
fn stale_running_run_is_repaired_to_interrupted() -> Result<()> {
    let root = unique_test_dir("core-wiki-stale");
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
    let run_id = RunId::new("cwrun_01stale")?;
    let state = RunState {
        schema_version: 1,
        run_id: run_id.clone(),
        project_id: project_id.to_owned(),
        status: RunStatus::Running,
        phase: RunPhase::WaitingForAgent,
        created_at: "2026-04-13T10:00:00Z".to_owned(),
        started_at: Some("2026-04-13T10:00:01Z".to_owned()),
        updated_at: "2026-04-13T10:00:02Z".to_owned(),
        finished_at: None,
        heartbeat_at: Some("2026-04-13T10:00:02Z".to_owned()),
        requested_by: Some("desktop".to_owned()),
        request_source: Some("darc-desktop/0.1.0".to_owned()),
        attempt: 1,
        cancel_requested: false,
        pid: Some(999_999),
        agent_id: Some("codex".to_owned()),
        runtime: Some("external_cli".to_owned()),
        model: Some("gpt-5.4".to_owned()),
        auth_profile: Some("openai/default".to_owned()),
        selected_sessions: vec!["codex:session-1".to_owned()],
        target_categories: Vec::new(),
        target_domains: Vec::new(),
        progress_percent: Some(20),
        headline: Some("Waiting".to_owned()),
        proposal_path: Some("proposal.json".to_owned()),
        result_path: Some("result.json".to_owned()),
        events_path: Some("events.jsonl".to_owned()),
        stdout_log_path: Some("agent.stdout.log".to_owned()),
        stderr_log_path: Some("agent.stderr.log".to_owned()),
        created_entry_ids: Vec::new(),
        updated_entry_ids: Vec::new(),
        digest_id: None,
        error_code: None,
        error_message: None,
    };
    store_run_state(&layout, &state)?;

    let repaired = load_project_wiki_run(Some(root.clone()), project_id, &run_id)?;
    assert_eq!(repaired.status, RunStatus::Interrupted);
    assert_eq!(repaired.error_code.as_deref(), Some("worker_interrupted"));

    fs::remove_dir_all(&root)?;
    Ok(())
}
