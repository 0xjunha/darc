use std::{
    fs,
    path::Path,
    path::PathBuf,
    process::Command,
    time::{Duration, SystemTime},
};

use anyhow::Result;
use darc_paths::current_utc_timestamp_at;
use darc_test_utils::unique_test_dir;
use darc_wiki::{
    RunId, RunPhase, RunState, RunStatus, build_digest_runtime_prompt, load_run_state,
    store_run_state,
};

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
            vec![build_project(&root, project_id, project_root.clone())],
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
            vec![build_project(&root, project_id, project_root.clone())],
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
        use_provider_auth: false,
        selected_sessions: Vec::new(),
        target_categories: vec!["architecture".to_owned()],
        target_domains: Vec::new(),
        progress_percent: None,
        headline: Some("Queued".to_owned()),
        proposal_path: None,
        proposal_schema_path: None,
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
            vec![build_project(&root, project_id, project_root.clone())],
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
        use_provider_auth: false,
        selected_sessions: vec!["codex:session-1".to_owned()],
        target_categories: Vec::new(),
        target_domains: Vec::new(),
        progress_percent: Some(20),
        headline: Some("Waiting".to_owned()),
        proposal_path: Some("proposal.json".to_owned()),
        proposal_schema_path: Some("context-wiki/proposal.schema.v1.json".to_owned()),
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

#[test]
fn stale_running_run_with_live_pid_stays_running() -> Result<()> {
    let root = unique_test_dir("core-wiki-live-stale");
    let project_root = root.join("repo");
    let project_id = "repo-123";
    fs::create_dir_all(&project_root)?;
    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![build_project(&root, project_id, project_root.clone())],
            SourcesConfig::default(),
        ),
    )?;

    let layout = ensure_project_wiki(Some(root.clone()), project_id)?;
    let run_id = RunId::new("cwrun_01live")?;
    let heartbeat_at = current_utc_timestamp_at(SystemTime::now() - Duration::from_secs(120));
    let mut child = Command::new("sleep").arg("30").spawn()?;
    let state = RunState {
        schema_version: 1,
        run_id: run_id.clone(),
        project_id: project_id.to_owned(),
        status: RunStatus::Running,
        phase: RunPhase::PreparingContext,
        created_at: heartbeat_at.clone(),
        started_at: Some(heartbeat_at.clone()),
        updated_at: heartbeat_at.clone(),
        finished_at: None,
        heartbeat_at: Some(heartbeat_at),
        requested_by: Some("desktop".to_owned()),
        request_source: Some("darc-desktop/0.1.0".to_owned()),
        attempt: 1,
        cancel_requested: false,
        pid: Some(child.id()),
        agent_id: Some("codex".to_owned()),
        runtime: Some("external_cli".to_owned()),
        model: Some("gpt-5.4".to_owned()),
        auth_profile: Some("openai/default".to_owned()),
        use_provider_auth: false,
        selected_sessions: vec!["codex:session-1".to_owned()],
        target_categories: Vec::new(),
        target_domains: Vec::new(),
        progress_percent: Some(20),
        headline: Some("Preparing agent runtime".to_owned()),
        proposal_path: Some("proposal.json".to_owned()),
        proposal_schema_path: Some("context-wiki/proposal.schema.v1.json".to_owned()),
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

    let loaded = load_project_wiki_run(Some(root.clone()), project_id, &run_id)?;
    assert_eq!(loaded.status, RunStatus::Running);
    assert_eq!(loaded.error_code, None);

    child.kill()?;
    let _ = child.wait();
    fs::remove_dir_all(&root)?;
    Ok(())
}

#[test]
fn runtime_request_uses_project_root_as_workdir() -> Result<()> {
    let root = unique_test_dir("core-wiki-runtime-workdir");
    let project_root = root.join("repo");
    let project_id = "repo-123";
    fs::create_dir_all(&project_root)?;
    write_config(
        &root,
        &SharedConfig::new(
            root.clone(),
            vec![build_project(&root, project_id, project_root.clone())],
            SourcesConfig::default(),
        ),
    )?;

    let layout = ensure_project_wiki(Some(root.clone()), project_id)?;
    let run_id = RunId::new("cwrun_01runtime")?;
    let state = RunState {
        schema_version: 1,
        run_id: run_id.clone(),
        project_id: project_id.to_owned(),
        status: RunStatus::Queued,
        phase: RunPhase::WaitingForAgent,
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
        agent_id: Some("codex".to_owned()),
        runtime: Some("external_cli".to_owned()),
        model: Some("gpt-5.4".to_owned()),
        auth_profile: None,
        use_provider_auth: false,
        selected_sessions: vec!["codex:session-1".to_owned()],
        target_categories: Vec::new(),
        target_domains: Vec::new(),
        progress_percent: None,
        headline: Some("Queued".to_owned()),
        proposal_path: None,
        proposal_schema_path: Some("context-wiki/proposal.schema.v1.json".to_owned()),
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
    let prompt = build_digest_runtime_prompt(
        &root,
        project_id,
        run_id.as_str(),
        &state.selected_sessions,
        &state.target_categories,
        &state.target_domains,
    );
    let schema_path = layout.digest_proposal_schema_path();

    let request = super::runtime::build_runtime_request(
        &layout,
        &run_id,
        &state,
        &prompt,
        &schema_path,
        &project_root,
    )?;

    assert_eq!(request.workdir, project_root);
    assert_eq!(request.schema_path, layout.digest_proposal_schema_path());

    fs::remove_dir_all(&root)?;
    Ok(())
}

#[test]
fn recent_preparing_context_run_stays_running_on_read_side() -> Result<()> {
    let root = unique_test_dir("core-wiki-recent");
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
    let run_id = RunId::new("cwrun_01recent")?;
    let heartbeat_at = current_utc_timestamp_at(SystemTime::now() - Duration::from_secs(10));
    let state = RunState {
        schema_version: 1,
        run_id: run_id.clone(),
        project_id: project_id.to_owned(),
        status: RunStatus::Running,
        phase: RunPhase::PreparingContext,
        created_at: heartbeat_at.clone(),
        started_at: Some(heartbeat_at.clone()),
        updated_at: heartbeat_at.clone(),
        finished_at: None,
        heartbeat_at: Some(heartbeat_at),
        requested_by: Some("desktop".to_owned()),
        request_source: Some("darc-desktop/0.1.0".to_owned()),
        attempt: 1,
        cancel_requested: false,
        pid: Some(999_999),
        agent_id: Some("codex".to_owned()),
        runtime: Some("external_cli".to_owned()),
        model: Some("gpt-5.4".to_owned()),
        auth_profile: Some("openai/default".to_owned()),
        use_provider_auth: false,
        selected_sessions: vec!["codex:session-1".to_owned()],
        target_categories: Vec::new(),
        target_domains: Vec::new(),
        progress_percent: Some(20),
        headline: Some("Preparing agent runtime".to_owned()),
        proposal_path: Some("proposal.json".to_owned()),
        proposal_schema_path: Some("context-wiki/proposal.schema.v1.json".to_owned()),
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

    let loaded = load_project_wiki_run(Some(root.clone()), project_id, &run_id)?;
    assert_eq!(loaded.status, RunStatus::Running);

    let wiki = load_project_wiki(Some(root.clone()), project_id)?;
    assert_eq!(wiki.runs.len(), 1);
    assert_eq!(wiki.runs[0].status, RunStatus::Running);

    fs::remove_dir_all(&root)?;
    Ok(())
}

#[test]
fn legacy_reading_turns_phase_loads_as_preparing_context() -> Result<()> {
    let root = unique_test_dir("core-wiki-legacy-reading-turns");
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
    let run_id = RunId::new("cwrun_01legacy")?;
    fs::create_dir_all(layout.run_dir(&run_id))?;
    fs::write(
        layout.run_state_path(&run_id),
        concat!(
            "schema_version = 1\n",
            "run_id = \"cwrun_01legacy\"\n",
            "project_id = \"repo-123\"\n",
            "status = \"running\"\n",
            "phase = \"reading_turns\"\n",
            "created_at = \"2026-04-13T10:00:00Z\"\n",
            "updated_at = \"2026-04-13T10:00:00Z\"\n",
            "attempt = 1\n",
            "cancel_requested = false\n",
            "selected_sessions = [\"codex:session-1\"]\n",
            "target_categories = []\n",
            "target_domains = []\n",
            "created_entry_ids = []\n",
            "updated_entry_ids = []\n",
        ),
    )?;

    let loaded = load_project_wiki_run(Some(root.clone()), project_id, &run_id)?;
    assert_eq!(loaded.phase, RunPhase::PreparingContext);
    assert!(!loaded.use_provider_auth);

    store_run_state(&layout, &loaded)?;
    let rewritten = fs::read_to_string(layout.run_state_path(&run_id))?;
    assert!(rewritten.contains("phase = \"preparing_context\""));
    assert!(!rewritten.contains("reading_turns"));

    fs::remove_dir_all(&root)?;
    Ok(())
}

#[test]
fn runtime_prepare_failure_still_writes_terminal_result() -> Result<()> {
    let root = unique_test_dir("core-wiki-worker-failure");
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

    let prepared = prepare_project_wiki_digest_start(
        Some(root.clone()),
        project_id,
        &DigestStartOptions {
            session_refs: vec!["claude:session-1".to_owned()],
            agent_id: "claude".to_owned(),
            runtime: "external_cli".to_owned(),
            model: "gpt-5.4".to_owned(),
            auth_profile: None,
            use_provider_auth: false,
            requested_by: None,
            request_source: None,
            target_categories: Vec::new(),
            target_domains: Vec::new(),
        },
    )?;
    mark_project_wiki_digest_started(
        Some(root.clone()),
        project_id,
        &prepared.run_id,
        std::process::id(),
    )?;

    let layout = ensure_project_wiki(Some(root.clone()), project_id)?;
    let mut state = load_run_state(&layout, &prepared.run_id)?;
    state.model = None;
    store_run_state(&layout, &state)?;

    run_project_wiki_digest_worker(Some(root.clone()), project_id, &prepared.run_id)?;

    let run = load_run_state(&layout, &prepared.run_id)?;
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.error_code.as_deref(), Some("runtime_prepare_failed"));
    let result = fs::read_to_string(layout.run_result_path(&prepared.run_id))?;
    assert!(result.contains("\"status\": \"failed\""));
    assert!(result.contains("\"error_code\": \"runtime_prepare_failed\""));

    fs::remove_dir_all(&root)?;
    Ok(())
}

#[test]
fn spawn_failure_writes_terminal_result() -> Result<()> {
    let root = unique_test_dir("core-wiki-spawn-failure");
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

    let prepared = prepare_project_wiki_digest_start(
        Some(root.clone()),
        project_id,
        &DigestStartOptions {
            session_refs: vec!["claude:session-1".to_owned()],
            agent_id: "claude".to_owned(),
            runtime: "external_cli".to_owned(),
            model: "gpt-5.4".to_owned(),
            auth_profile: None,
            use_provider_auth: false,
            requested_by: None,
            request_source: None,
            target_categories: Vec::new(),
            target_domains: Vec::new(),
        },
    )?;

    fail_project_wiki_digest_start(
        Some(root.clone()),
        project_id,
        &prepared.run_id,
        "failed to spawn worker",
    )?;

    let layout = ensure_project_wiki(Some(root.clone()), project_id)?;
    let run = load_project_wiki_run(Some(root.clone()), project_id, &prepared.run_id)?;
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.error_code.as_deref(), Some("worker_spawn_failed"));
    let result = fs::read_to_string(layout.run_result_path(&prepared.run_id))?;
    assert!(result.contains("\"status\": \"failed\""));
    assert!(result.contains("\"error_code\": \"worker_spawn_failed\""));

    fs::remove_dir_all(&root)?;
    Ok(())
}

#[test]
fn codex_digest_start_rejects_provider_auth_opt_in() -> Result<()> {
    let root = unique_test_dir("core-wiki-codex-provider-auth");
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

    let error = prepare_project_wiki_digest_start(
        Some(root.clone()),
        project_id,
        &DigestStartOptions {
            session_refs: vec!["codex:session-1".to_owned()],
            agent_id: "codex".to_owned(),
            runtime: "external_cli".to_owned(),
            model: "gpt-5.4".to_owned(),
            auth_profile: None,
            use_provider_auth: true,
            requested_by: None,
            request_source: None,
            target_categories: Vec::new(),
            target_domains: Vec::new(),
        },
    )
    .expect_err("codex provider-auth opt-in should be rejected");
    assert!(
        error
            .to_string()
            .contains("does not expose a documented per-run API-key/provider-auth selector")
    );

    fs::remove_dir_all(&root)?;
    Ok(())
}
