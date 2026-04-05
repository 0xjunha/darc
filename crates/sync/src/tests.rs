use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use darc_paths::{encode_path_for_claude, normalize_project_path};

use super::{engine::*, manifest::Manifest, *};
use crate::utils::{format_system_time_utc, replace_existing_file, unique_sibling_path};

fn unique_test_dir(prefix: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "test-{prefix}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos()
    ))
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().context("missing parent directory")?;
    fs::create_dir_all(parent)?;
    fs::write(path, content)?;
    Ok(())
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run git {:?} in {}", args, cwd.display()))?;
    if output.status.success() {
        return Ok(());
    }

    bail!(
        "git {:?} failed in {}: {}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn init_git_repo(path: &Path, remote: &str) -> Result<()> {
    fs::create_dir_all(path)?;
    run_git(path, &["init"])?;
    run_git(path, &["config", "user.name", "Darc Test"])?;
    run_git(path, &["config", "user.email", "darc-tests@example.com"])?;
    run_git(path, &["config", "commit.gpgsign", "false"])?;
    run_git(path, &["remote", "add", "origin", remote])
}

fn sample_request(
    project_root: &Path,
    sessions_root: &Path,
    claude_projects: &Path,
    codex_home: &Path,
    codex_sessions_root: &Path,
) -> SyncRequest {
    let project_root = normalize_project_path(project_root);
    SyncRequest {
        project_name: "darc".into(),
        project_root: project_root.clone(),
        sessions_root: sessions_root.to_path_buf(),
        primary_project_path: project_root.clone(),
        stored_known_paths: BTreeSet::new(),
        project_paths: BTreeSet::from([project_root]),
        other_project_paths: BTreeSet::new(),
        project_upstream: None,
        sources: vec![SourceKind::Claude, SourceKind::Codex],
        claude: Some(ClaudeSource {
            include_subagents: true,
            projects_root: claude_projects.to_path_buf(),
        }),
        codex: Some(CodexSource {
            home: codex_home.to_path_buf(),
            sessions_root: codex_sessions_root.to_path_buf(),
        }),
    }
}

/// Shared filesystem fixture for integration tests that exercise prepare and execute sync.
struct TestWorkspace {
    root: PathBuf,
    project_root: PathBuf,
    sessions_root: PathBuf,
    claude_projects: PathBuf,
    codex_home: PathBuf,
    codex_sessions_root: PathBuf,
    canonical_project_root: PathBuf,
}

impl TestWorkspace {
    fn new(prefix: &str) -> Result<Self> {
        let root = unique_test_dir(prefix);
        let project_root = root.join("repo");
        let sessions_root = root.join("archive").join("sessions");
        let claude_projects = root.join("claude").join("projects");
        let codex_home = root.join("codex");
        let codex_sessions_root = codex_home.join("sessions");
        fs::create_dir_all(&project_root)?;
        fs::create_dir_all(&claude_projects)?;
        fs::create_dir_all(&codex_sessions_root)?;
        let canonical_project_root = fs::canonicalize(&project_root)?;
        Ok(Self {
            root,
            project_root,
            sessions_root,
            claude_projects,
            codex_home,
            codex_sessions_root,
            canonical_project_root,
        })
    }

    fn default_request(&self) -> SyncRequest {
        sample_request(
            &self.project_root,
            &self.sessions_root,
            &self.claude_projects,
            &self.codex_home,
            &self.codex_sessions_root,
        )
    }
}

#[test]
fn extract_codex_session_meta_normalizes_cwd_textually() -> Result<()> {
    let dir = unique_test_dir("codex-meta");
    let rollout = dir.join("rollout.jsonl");
    write_file(
        &rollout,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\",\"cwd\":\"/tmp/demo/./worktree/../repo/\",\"cli_version\":\"0.118.0\"}}\n",
    )?;

    let meta = extract_codex_session_meta(&rollout)?.context("missing meta")?;

    assert_eq!(meta.cwd, PathBuf::from("/tmp/demo/repo"));
    assert_eq!(meta.session_id, "x");

    Ok(())
}

#[test]
fn extract_codex_session_meta_accepts_legacy_rollouts_without_cli_version() -> Result<()> {
    let dir = unique_test_dir("codex-meta-legacy");
    let rollout = dir.join("rollout.jsonl");
    write_file(
        &rollout,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\",\"cwd\":\"/tmp/demo/repo\"}}\n",
    )?;

    let meta = extract_codex_session_meta(&rollout)?.context("missing meta")?;

    assert_eq!(meta.session_id, "x");
    assert_eq!(meta.cwd, PathBuf::from("/tmp/demo/repo"));

    Ok(())
}

#[test]
fn extract_codex_session_meta_ignores_non_meta_first_line() -> Result<()> {
    let dir = unique_test_dir("codex-non-meta");
    let rollout = dir.join("rollout.jsonl");
    write_file(
        &rollout,
        "{\"type\":\"message\",\"payload\":{\"text\":\"hello\"}}\n",
    )?;

    let meta = extract_codex_session_meta(&rollout)?;

    assert!(meta.is_none());

    Ok(())
}

#[test]
fn matching_claude_dirs_uses_exact_names_only() -> Result<()> {
    let root = unique_test_dir("claude-match");
    fs::create_dir_all(root.join("-tmp-main"))?;
    fs::create_dir_all(root.join("-tmp-main-extra"))?;
    let mut warnings = Vec::new();

    let matches = matching_claude_dirs(
        &root,
        &BTreeSet::from(["-tmp-main".to_owned()]),
        &mut warnings,
    )?;

    assert_eq!(matches, vec![root.join("-tmp-main")]);
    assert!(warnings.is_empty());

    Ok(())
}

#[test]
fn discover_claude_sessions_ignores_orphan_directories() -> Result<()> {
    let root = unique_test_dir("claude-orphan");
    let project_root = root.join("repo");
    let canonical_project_root = {
        fs::create_dir_all(&project_root)?;
        fs::canonicalize(&project_root)?
    };
    let projects_root = root.join("claude/projects");
    let session_id = "885a05b8-f731-4fde-bfdb-a24ce28dc9c3";
    let project_dir = projects_root.join(encode_path_for_claude(&canonical_project_root));
    let mut warnings = Vec::new();

    write_file(
        &project_dir.join(format!("{session_id}.jsonl")),
        "{\"type\":\"message\"}\n",
    )?;
    write_file(
        &project_dir.join(format!("{session_id}/subagents/agent-a1.jsonl")),
        "{\"type\":\"subagent\"}\n",
    )?;
    write_file(&project_dir.join("orphan/note.txt"), "orphan\n")?;

    let discovery = discover_claude_sessions(
        &ClaudeSource {
            include_subagents: true,
            projects_root,
        },
        &BTreeSet::from([canonical_project_root]),
        &mut warnings,
    )?;

    assert_eq!(discovery.sessions.len(), 1);
    assert_eq!(discovery.auxiliary.len(), 1);
    assert_eq!(discovery.auxiliary[0].parent_session, session_id);
    assert!(warnings.is_empty());

    Ok(())
}

#[test]
fn codex_duplicate_resolution_prefers_larger_then_newer_match() {
    let matching = CodexCandidate {
        session_id: "id".into(),
        source_path: PathBuf::from("/tmp/a"),
        archive_path: PathBuf::from("codex/a.jsonl"),
        repo_root: PathBuf::from("/tmp/project"),
        matches_project: true,
        size: 10,
        mtime_ms: 20,
    };
    let larger = CodexCandidate {
        size: 20,
        mtime_ms: 10,
        ..matching.clone()
    };
    let other_project = CodexCandidate {
        size: 100,
        matches_project: false,
        ..matching.clone()
    };

    let candidates = [matching, larger.clone(), other_project];
    let winner = select_codex_candidate(&candidates).expect("winner");

    assert_eq!(winner.size, larger.size);
}

#[test]
fn codex_duplicate_resolution_uses_stable_source_path_tie_break() {
    let left = CodexCandidate {
        session_id: "id".into(),
        source_path: PathBuf::from("/tmp/a"),
        archive_path: PathBuf::from("codex/a.jsonl"),
        repo_root: PathBuf::from("/tmp/project"),
        matches_project: true,
        size: 10,
        mtime_ms: 20,
    };
    let right = CodexCandidate {
        source_path: PathBuf::from("/tmp/b"),
        archive_path: PathBuf::from("codex/b.jsonl"),
        ..left.clone()
    };

    let winner = select_codex_candidate(&[left, right.clone()]).expect("winner");

    assert_eq!(winner.source_path, right.source_path);
}

#[test]
fn utc_formatter_handles_unix_epoch() -> Result<()> {
    let formatted = format_system_time_utc(UNIX_EPOCH)?;

    assert_eq!(formatted, "1970-01-01T00:00:00Z");

    Ok(())
}

#[test]
fn replace_existing_file_swaps_in_new_content() -> Result<()> {
    let dir = unique_test_dir("replace-existing");
    let destination = dir.join("session.jsonl");
    let temp_path = dir.join("session.jsonl.tmp");
    fs::create_dir_all(&dir)?;
    write_file(&destination, "old\n")?;
    write_file(&temp_path, "new\n")?;

    replace_existing_file(
        &temp_path,
        &destination,
        std::io::Error::other("simulated rename failure"),
    )?;

    assert_eq!(fs::read_to_string(&destination)?, "new\n");
    assert!(!temp_path.exists());

    Ok(())
}

#[test]
fn replace_existing_file_restores_destination_on_failure() -> Result<()> {
    let dir = unique_test_dir("replace-restore");
    let destination = dir.join("session.jsonl");
    let missing_temp = dir.join("missing.tmp");
    fs::create_dir_all(&dir)?;
    write_file(&destination, "old\n")?;

    let error = replace_existing_file(
        &missing_temp,
        &destination,
        std::io::Error::other("simulated rename failure"),
    )
    .expect_err("replacement should fail when temp path is missing");

    assert!(
        error
            .to_string()
            .contains("after moving the existing file aside")
    );
    assert_eq!(fs::read_to_string(&destination)?, "old\n");

    Ok(())
}

#[test]
fn unique_sibling_path_is_distinct_per_call() {
    let path = Path::new("/tmp/session.jsonl");
    let first = unique_sibling_path(path, "tmp");
    let second = unique_sibling_path(path, "tmp");
    let backup = unique_sibling_path(path, "bak");

    assert_ne!(first, second);
    assert_ne!(first, backup);
    assert_ne!(second, backup);
}

#[test]
fn prepare_and_execute_sync_copies_sessions_and_updates_manifest() -> Result<()> {
    let ws = TestWorkspace::new("sync-exec")?;
    let claude_session_id = "885a05b8-f731-4fde-bfdb-a24ce28dc9c3";
    let codex_sessions = ws.codex_sessions_root.join("2026/03/31");
    let codex_archived = ws.codex_home.join("archived_sessions");
    fs::create_dir_all(&codex_sessions)?;
    fs::create_dir_all(&codex_archived)?;

    let encoded = encode_path_for_claude(&ws.canonical_project_root);
    let claude_dir = ws.claude_projects.join(encoded);
    let claude_session = claude_dir.join(format!("{claude_session_id}.jsonl"));
    let claude_aux = claude_dir.join(format!("{claude_session_id}/subagents/agent-a1.jsonl"));
    write_file(&claude_session, "{\"type\":\"message\"}\n")?;
    write_file(&claude_aux, "{\"type\":\"subagent\"}\n")?;

    let rollout_name = "rollout-2026-03-31T11-24-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl";
    let active_rollout = codex_sessions.join(rollout_name);
    let archived_rollout = codex_archived.join(rollout_name);
    write_file(
        &active_rollout,
        &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
            ws.canonical_project_root.display()
        ),
    )?;
    write_file(
        &archived_rollout,
        &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n{{\"type\":\"message\"}}\n",
            ws.canonical_project_root.display()
        ),
    )?;

    let plan = prepare_sync(ws.default_request())?;

    assert_eq!(plan.sessions_to_copy(), 2);
    assert_eq!(plan.auxiliary_to_copy(), 1);
    assert!(plan.new_known_paths.is_empty());

    let report = execute_sync(plan)?;

    assert_eq!(report.sessions_copied, 2);
    assert_eq!(report.auxiliary_copied, 1);
    assert!(report.manifest_written);

    let manifest_path = ws.sessions_root.join(".manifest.json");
    let manifest: Manifest = serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;
    let claude_entry = manifest
        .sessions
        .get(claude_session_id)
        .context("missing Claude manifest entry")?;
    assert_eq!(
        claude_entry.archive_path,
        PathBuf::from(format!(
            "claude/{claude_session_id}/{claude_session_id}.jsonl"
        ))
    );
    let codex_entry = manifest
        .sessions
        .get("019d3415-0b9c-7dc3-88e0-e9cb7a789e3f")
        .context("missing Codex manifest entry")?;
    assert_eq!(codex_entry.provider, SourceKind::Codex);
    assert_eq!(
        codex_entry.source_path, archived_rollout,
        "duplicate resolution should keep the larger archived rollout",
    );
    assert!(
        ws.sessions_root
            .join(format!(
                "claude/{claude_session_id}/{claude_session_id}.jsonl"
            ))
            .exists()
    );
    assert!(
        ws.sessions_root
            .join(format!(
                "claude/{claude_session_id}/subagents/agent-a1.jsonl"
            ))
            .exists()
    );

    let second_plan = prepare_sync(ws.default_request())?;
    assert_eq!(second_plan.sessions_to_copy(), 0);
    assert_eq!(second_plan.auxiliary_to_copy(), 0);
    assert_eq!(second_plan.sessions_unchanged, 2);
    assert_eq!(second_plan.auxiliary_unchanged, 1);
    assert!(!second_plan.manifest_written());

    Ok(())
}

#[test]
fn prepare_sync_copies_legacy_codex_rollout_without_cli_version() -> Result<()> {
    let ws = TestWorkspace::new("sync-codex-legacy-cli-version")?;
    let codex_sessions = ws.codex_sessions_root.join("2026/04/01");
    fs::create_dir_all(&codex_sessions)?;

    let rollout_name = "rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl";
    write_file(
        &codex_sessions.join(rollout_name),
        &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"message\"}}\n",
            ws.canonical_project_root.display()
        ),
    )?;

    let plan = prepare_sync(ws.default_request())?;

    assert_eq!(plan.sessions_to_copy(), 1);
    assert!(plan.warnings.is_empty());

    let report = execute_sync(plan)?;
    assert_eq!(report.sessions_copied, 1);
    assert!(
        ws.sessions_root
            .join(format!("codex/{rollout_name}"))
            .exists()
    );

    Ok(())
}

#[test]
fn prepare_sync_learns_codex_checkout_with_same_upstream() -> Result<()> {
    let ws = TestWorkspace::new("sync-codex-known-path")?;
    let remote = "https://example.com/acme/darc.git";
    let related_root = ws.root.join("repo-b");
    let related_subdir = related_root.join("nested");
    let codex_sessions = ws.codex_sessions_root.join("2026/04/01");
    fs::create_dir_all(&codex_sessions)?;
    init_git_repo(&ws.project_root, remote)?;
    init_git_repo(&related_root, remote)?;
    fs::create_dir_all(&related_subdir)?;
    let canonical_related_root = fs::canonicalize(&related_root)?;

    let rollout_name = "rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl";
    write_file(
        &codex_sessions.join(rollout_name),
        &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n{{\"type\":\"message\"}}\n",
            related_subdir.display()
        ),
    )?;

    let mut request = ws.default_request();
    request.project_upstream = Some(remote.into());

    let plan = prepare_sync(request)?;

    assert_eq!(plan.project_root, ws.canonical_project_root);
    assert_eq!(plan.sessions_to_copy(), 1);
    assert_eq!(plan.new_known_paths, vec![canonical_related_root.clone()]);
    assert_eq!(plan.persisted_known_paths(), &[canonical_related_root]);

    Ok(())
}

#[test]
fn prepare_sync_skips_mismatched_codex_session_ids() -> Result<()> {
    let ws = TestWorkspace::new("sync-codex-id-mismatch")?;
    let codex_sessions = ws.codex_sessions_root.join("2026/04/01");
    fs::create_dir_all(&codex_sessions)?;

    write_file(
        &codex_sessions
            .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e40\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n{{\"type\":\"message\"}}\n",
            ws.canonical_project_root.display()
        ),
    )?;

    let plan = prepare_sync(ws.default_request())?;

    assert_eq!(plan.sessions_to_copy(), 0);
    assert_eq!(plan.sessions_unchanged, 0);
    assert!(
        plan.warnings
            .iter()
            .any(|warning| warning.contains("mismatched Codex session ids"))
    );

    Ok(())
}

#[test]
fn prepare_sync_accepts_payload_only_codex_session_id() -> Result<()> {
    let ws = TestWorkspace::new("sync-codex-payload-only-id")?;
    let codex_sessions = ws.codex_sessions_root.join("2026/04/01");
    fs::create_dir_all(&codex_sessions)?;

    let rollout_name = "rollout-invalid.jsonl";
    write_file(
        &codex_sessions.join(rollout_name),
        &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n{{\"type\":\"message\"}}\n",
            ws.canonical_project_root.display()
        ),
    )?;

    let plan = prepare_sync(ws.default_request())?;

    assert_eq!(plan.sessions_to_copy(), 1);
    assert!(plan.warnings.is_empty());

    let report = execute_sync(plan)?;

    assert_eq!(report.sessions_copied, 1);
    assert!(
        ws.sessions_root
            .join(format!("codex/{rollout_name}"))
            .exists()
    );

    Ok(())
}

#[test]
fn prepare_sync_skips_codex_session_in_other_projects_live_worktree() -> Result<()> {
    let ws = TestWorkspace::new("sync-codex-other-worktree")?;
    let remote = "https://example.com/acme/darc.git";
    let repo_b_root = ws.root.join("repo-b");
    let repo_b_worktree = ws.root.join("repo-b-wt");
    let codex_sessions = ws.codex_sessions_root.join("2026/04/01");
    fs::create_dir_all(&codex_sessions)?;
    init_git_repo(&ws.project_root, remote)?;
    init_git_repo(&repo_b_root, remote)?;

    run_git(&repo_b_root, &["commit", "--allow-empty", "-m", "init"])?;
    run_git(
        &repo_b_root,
        &[
            "worktree",
            "add",
            repo_b_worktree.to_str().expect("UTF-8 path"),
            "-b",
            "wt-branch",
        ],
    )?;
    let canonical_worktree = fs::canonicalize(&repo_b_worktree)?;

    write_file(
        &codex_sessions
            .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n{{\"type\":\"message\"}}\n",
            canonical_worktree.display()
        ),
    )?;

    let mut request = ws.default_request();
    request.project_upstream = Some(remote.into());
    request.other_project_paths = darc_paths::project_path_set(&repo_b_root, &[])?;

    let plan = prepare_sync(request)?;

    assert_eq!(plan.sessions_to_copy(), 0);
    assert!(plan.new_known_paths.is_empty());
    assert!(plan.warnings.is_empty());

    Ok(())
}

#[test]
fn prepare_sync_skips_codex_checkout_owned_by_other_project() -> Result<()> {
    let ws = TestWorkspace::new("sync-codex-other-project")?;
    let remote = "https://example.com/acme/darc.git";
    let related_root = ws.root.join("repo-b");
    let related_subdir = related_root.join("nested");
    let codex_sessions = ws.codex_sessions_root.join("2026/04/01");
    fs::create_dir_all(&codex_sessions)?;
    init_git_repo(&ws.project_root, remote)?;
    init_git_repo(&related_root, remote)?;
    fs::create_dir_all(&related_subdir)?;

    write_file(
        &codex_sessions
            .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
        &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n{{\"type\":\"message\"}}\n",
            related_subdir.display()
        ),
    )?;

    let mut request = ws.default_request();
    request.project_upstream = Some(remote.into());
    request.other_project_paths = darc_paths::project_path_set(&related_root, &[])?;

    let plan = prepare_sync(request)?;

    assert_eq!(plan.sessions_to_copy(), 0);
    assert!(plan.new_known_paths.is_empty());
    assert!(plan.warnings.is_empty());

    Ok(())
}
