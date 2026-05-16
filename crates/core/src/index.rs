use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
pub use darc_index::{IndexReport, SkippedCodexRollout, SkippedRollout};
use darc_index::{ProjectIndexRequest, index_project_archived_sessions};
use darc_paths::SourceKind;
use darc_store::{
    INDEX_DB_FILE_NAME, preserve_index_sharing_state, remove_index_database, replace_index_database,
};

use crate::{
    active_project::{ActiveProject, load_active_project},
    default_root_path,
    project::registered_projects,
};

/// Collects optional provider filters for the indexing workflow.
#[derive(Debug, Clone, Default)]
pub struct IndexOptions {
    pub provider_filter: Vec<SourceKind>,
}

/// Reports a workspace-wide index rebuild.
#[derive(Debug, Clone)]
pub struct WorkspaceIndexReport {
    pub root: PathBuf,
    pub index_db_path: PathBuf,
    pub providers: Vec<SourceKind>,
    pub projects: Vec<IndexReport>,
}

impl WorkspaceIndexReport {
    /// Returns the total currently indexed session count across rebuilt projects.
    pub fn sessions_currently_indexed(&self) -> usize {
        self.projects
            .iter()
            .map(|project| project.sessions_currently_indexed)
            .sum()
    }

    /// Returns the total currently indexed turn count across rebuilt projects.
    pub fn turns_currently_indexed(&self) -> usize {
        self.projects
            .iter()
            .map(|project| project.turns_currently_indexed)
            .sum()
    }

    /// Returns the total number of skipped rollout files across rebuilt projects.
    pub fn skipped_rollout_count(&self) -> usize {
        self.projects
            .iter()
            .map(|project| project.skipped_rollouts.len())
            .sum()
    }
}

/// Indexes archived sessions for the active project into SQLite.
pub fn index_project_sessions(root: Option<PathBuf>, options: IndexOptions) -> Result<IndexReport> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    index_project_sessions_from(
        &current_dir,
        root.unwrap_or_else(default_root_path),
        &selected_index_providers(&options.provider_filter),
    )
}

/// Indexes archived Codex rollouts for the active project into SQLite.
pub fn index_project_codex_turns(root: Option<PathBuf>) -> Result<IndexReport> {
    index_project_sessions(
        root,
        IndexOptions {
            provider_filter: vec![SourceKind::Codex],
        },
    )
}

/// Rebuilds the shared SQLite index from every configured project's archived sessions.
pub fn rebuild_workspace_index(root: Option<PathBuf>) -> Result<WorkspaceIndexReport> {
    let root = root.unwrap_or_else(default_root_path);
    let providers = selected_index_providers(&[]);
    let projects = registered_projects(&root)?;
    if projects.is_empty() {
        anyhow::bail!("no configured darc projects found under {}", root.display());
    }

    let index_db_path = root.join(INDEX_DB_FILE_NAME);
    let temp_index_db_path = rebuild_index_database_path(&index_db_path)?;
    remove_index_database(&temp_index_db_path)?;

    let rebuild_result = rebuild_workspace_index_into(&projects, &providers, &temp_index_db_path);
    let mut reports = match rebuild_result {
        Ok(reports) => reports,
        Err(error) => {
            let _cleanup_result = remove_index_database(&temp_index_db_path);
            return Err(error);
        }
    };

    preserve_index_sharing_state(&index_db_path, &temp_index_db_path)?;
    replace_index_database(&temp_index_db_path, &index_db_path)?;
    for report in &mut reports {
        report.index_db_path.clone_from(&index_db_path);
    }

    Ok(WorkspaceIndexReport {
        root,
        index_db_path,
        providers,
        projects: reports,
    })
}

/// Returns the CLI rebuild command for the Darc root that owns one index database.
pub fn index_rebuild_command(index_db_path: &Path) -> String {
    let Some(root) = index_db_path.parent() else {
        return "darc index --rebuild".to_owned();
    };
    if root == default_root_path() {
        "darc index --rebuild".to_owned()
    } else {
        format!(
            "darc index --rebuild --root {}",
            shell_quote(&root.display().to_string())
        )
    }
}

/// Returns one POSIX-shell-safe single-quoted string.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Rebuilds every configured project into one target SQLite index path.
fn rebuild_workspace_index_into(
    projects: &[crate::config::ProjectConfig],
    providers: &[SourceKind],
    index_db_path: &Path,
) -> Result<Vec<IndexReport>> {
    let mut reports = Vec::with_capacity(projects.len());
    for project in projects {
        let request = ProjectIndexRequest {
            project_id: project.id.clone(),
            project_name: project.name.clone(),
            project_root: project.local_path.clone(),
            sessions_root: project.sessions_root.clone(),
            index_db_path: index_db_path.to_path_buf(),
        };
        reports.push(
            index_project_archived_sessions(&request, providers)
                .with_context(|| format!("failed to index project `{}`", project.name))?,
        );
    }

    Ok(reports)
}

/// Indexes archived provider rollouts for one explicit current directory and darc root.
pub(crate) fn index_project_sessions_from(
    current_dir: &Path,
    root: PathBuf,
    providers: &[SourceKind],
) -> Result<IndexReport> {
    let active_project = load_active_project(current_dir, &root)?;
    index_project_sessions_for_active_project(active_project, root, providers)
}

/// Indexes archived provider rollouts for one already-resolved active project.
pub(crate) fn index_project_sessions_for_active_project(
    active_project: ActiveProject,
    root: PathBuf,
    providers: &[SourceKind],
) -> Result<IndexReport> {
    let request = ProjectIndexRequest {
        project_id: active_project.project.id,
        project_name: active_project.project.name,
        project_root: active_project.current_root,
        sessions_root: active_project.project.sessions_root,
        index_db_path: root.join(INDEX_DB_FILE_NAME),
    };
    index_project_archived_sessions(&request, providers)
}

/// Resolves the selected provider list for one indexing run.
pub(crate) fn selected_index_providers(filter: &[SourceKind]) -> Vec<SourceKind> {
    if filter.is_empty() {
        return vec![SourceKind::Claude, SourceKind::Codex];
    }

    filter
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Returns a unique temporary SQLite path next to the shared index database.
fn rebuild_index_database_path(index_db_path: &Path) -> Result<PathBuf> {
    let file_name = index_db_path.file_name().with_context(|| {
        format!(
            "index database path {} is missing a filename",
            index_db_path.display()
        )
    })?;
    let mut temp_name = OsString::from(file_name);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    temp_name.push(format!(".rebuild-{}-{nanos}.tmp", std::process::id()));
    Ok(index_db_path.with_file_name(temp_name))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use anyhow::Result;
    use darc_paths::SourceKind;
    use darc_store::{
        SharePolicy, ShareSessionExport, ShareState, ShareTurnExport, ShareTurnImport,
        ShareUserRecord, import_shared_turn, open_index_database_writer, set_project_share_policy,
        set_session_share_state,
    };
    use rusqlite::Connection;

    use super::rebuild_workspace_index;
    use crate::{
        config::{ProjectConfig, SharedConfig, SourcesConfig},
        constants::CONFIG_FILE_NAME,
    };

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Builds one unique temporary directory for index workflow tests.
    fn unique_test_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "darc-index-{label}-{}-{nanos}-{counter}",
            std::process::id()
        ))
    }

    /// Writes one UTF-8 file after creating its parent directory.
    fn write_file(path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
        Ok(())
    }

    /// Writes one minimal archived Codex rollout fixture.
    fn write_archived_codex_rollout(
        sessions_root: &Path,
        project_root: &Path,
        session_id: &str,
        user_message: &str,
        assistant_reply: &str,
    ) -> Result<()> {
        write_file(
            &sessions_root
                .join("codex")
                .join(format!("rollout-2026-04-01T10-00-00-{session_id}.jsonl")),
            &format!(
                concat!(
                    "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"cwd\":\"{cwd}\",\"cli_version\":\"0.118.0\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"{user_message}\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{assistant_reply}\"}}]}}}}\n"
                ),
                session_id = session_id,
                cwd = project_root.display(),
                user_message = user_message,
                assistant_reply = assistant_reply,
            ),
        )
    }

    /// Builds one synthetic shared turn fixture for rebuild preservation tests.
    fn synthetic_shared_turn(session_id: &str) -> ShareTurnExport {
        ShareTurnExport {
            session: ShareSessionExport {
                project_id: "source-repo".to_owned(),
                provider: SourceKind::Codex,
                session_id: session_id.to_owned(),
                parent_session_id: None,
                session_kind: "primary".to_owned(),
                archive_path: "shared.jsonl".to_owned(),
                cwd: "/tmp/shared-repo".to_owned(),
                cli_version: Some("0.1.0".to_owned()),
                schema_id: Some("codex:test".to_owned()),
                determinism: Some("exact".to_owned()),
                source_size: Some(1),
                source_mtime_ms: Some(1),
            },
            turn_ordinal: 0,
            turn_id: Some("turn-0".to_owned()),
            started_at: "2026-05-15T00:00:00Z".to_owned(),
            completed_at: Some("2026-05-15T00:00:01Z".to_owned()),
            status: "completed".to_owned(),
            user_message: "shared prompt".to_owned(),
            final_answer_at: Some("2026-05-15T00:00:01Z".to_owned()),
            final_answer_text: Some("shared answer".to_owned()),
            steps_json: "[]".to_owned(),
            step_count: 0,
            tool_call_count: 0,
            tool_output_count: 0,
            attachment_count: 0,
            delegation_count: 0,
            hook_summary_count: 0,
            has_final_answer: 1,
            duration_ms: Some(1),
            effective_agent_runtime_ms: Some(1),
            provider_total_token_count: Some(1),
            input_uncached_token_count: Some(1),
            cache_read_token_count: Some(0),
            cache_write_token_count: Some(0),
            output_token_count: Some(1),
            reasoning_token_count: Some(0),
            total_token_count: Some(1),
            primary_model: Some("synthetic-model".to_owned()),
            changed_file_count: 0,
            added_line_count: 0,
            removed_line_count: 0,
        }
    }

    #[test]
    fn rebuild_workspace_index_recreates_shared_index_for_all_projects() -> Result<()> {
        let root = unique_test_dir("workspace-rebuild");
        let first_project_root = root.join("repo-one");
        let second_project_root = root.join("repo-two");
        let first_sessions_root = root.join("projects/repo-one-123/sessions");
        let second_sessions_root = root.join("projects/repo-two-456/sessions");
        fs::create_dir_all(&first_project_root)?;
        fs::create_dir_all(&second_project_root)?;
        write_archived_codex_rollout(
            &first_sessions_root,
            &first_project_root,
            "22222222-2222-4222-8222-22222222223f",
            "Index first project",
            "First indexed",
        )?;
        write_archived_codex_rollout(
            &second_sessions_root,
            &second_project_root,
            "33333333-3333-4333-8333-33333333333f",
            "Index second project",
            "Second indexed",
        )?;
        let config = SharedConfig::new(
            root.clone(),
            vec![
                ProjectConfig {
                    id: "repo-one-123".into(),
                    name: "repo-one".into(),
                    local_path: first_project_root,
                    git_upstream: None,
                    sessions_root: first_sessions_root,
                    known_paths: Vec::new(),
                },
                ProjectConfig {
                    id: "repo-two-456".into(),
                    name: "repo-two".into(),
                    local_path: second_project_root,
                    git_upstream: None,
                    sessions_root: second_sessions_root,
                    known_paths: Vec::new(),
                },
            ],
            SourcesConfig::default(),
        );
        fs::write(
            root.join(CONFIG_FILE_NAME),
            toml::to_string_pretty(&config)?,
        )?;
        let index_db_path = root.join(darc_store::INDEX_DB_FILE_NAME);
        write_file(&index_db_path, "not a sqlite database")?;

        let report = rebuild_workspace_index(Some(root.clone()))?;

        assert_eq!(report.projects.len(), 2);
        assert_eq!(report.sessions_currently_indexed(), 2);
        assert_eq!(report.turns_currently_indexed(), 2);

        let connection = Connection::open(index_db_path)?;
        let rows = connection
            .prepare(
                "
                SELECT project_id, user_message, final_answer_text
                FROM turns
                ORDER BY project_id ASC
                ",
            )?
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        assert_eq!(
            rows,
            vec![
                (
                    "repo-one-123".to_owned(),
                    "Index first project".to_owned(),
                    "First indexed".to_owned()
                ),
                (
                    "repo-two-456".to_owned(),
                    "Index second project".to_owned(),
                    "Second indexed".to_owned()
                ),
            ]
        );

        Ok(())
    }

    #[test]
    fn rebuild_workspace_index_preserves_sharing_state() -> Result<()> {
        let root = unique_test_dir("workspace-rebuild-share-state");
        let project_root = root.join("repo-one");
        let sessions_root = root.join("projects/repo-one-123/sessions");
        fs::create_dir_all(&project_root)?;
        let local_session_id = "22222222-2222-4222-8222-22222222223f";
        write_archived_codex_rollout(
            &sessions_root,
            &project_root,
            local_session_id,
            "Index project",
            "Indexed",
        )?;
        let config = SharedConfig::new(
            root.clone(),
            vec![ProjectConfig {
                id: "repo-one-123".into(),
                name: "repo-one".into(),
                local_path: project_root,
                git_upstream: None,
                sessions_root,
                known_paths: Vec::new(),
            }],
            SourcesConfig::default(),
        );
        fs::write(
            root.join(CONFIG_FILE_NAME),
            toml::to_string_pretty(&config)?,
        )?;
        let index_db_path = root.join(darc_store::INDEX_DB_FILE_NAME);
        rebuild_workspace_index(Some(root.clone()))?;

        let mut connection = open_index_database_writer(&index_db_path)?;
        set_project_share_policy(
            &connection,
            "repo-one-123",
            SharePolicy::All,
            "2026-05-15T00:00:00Z",
        )?;
        set_session_share_state(
            &connection,
            "repo-one-123",
            SourceKind::Codex,
            local_session_id,
            ShareState::Included,
        )?;
        let user = ShareUserRecord {
            user_id: "usr-remote".to_owned(),
            display_name: Some("Remote User".to_owned()),
            email: Some("remote@example.invalid".to_owned()),
            public_key: Some("age1remote".to_owned()),
            source: "test".to_owned(),
            updated_at: "2026-05-15T00:00:00Z".to_owned(),
        };
        let shared_turn = synthetic_shared_turn("55555555-5555-4555-8555-55555555555f");
        import_shared_turn(
            &mut connection,
            ShareTurnImport {
                project_id: "repo-one-123",
                user: &user,
                remote_name: "remote:abcdef:darc/team",
                imported_at: "2026-05-15T00:00:00Z",
                turn: &shared_turn,
            },
        )?;
        drop(connection);

        rebuild_workspace_index(Some(root.clone()))?;

        let connection = Connection::open(&index_db_path)?;
        let policy: String = connection.query_row(
            "SELECT default_policy FROM project_share_policies WHERE project_id = 'repo-one-123'",
            [],
            |row| row.get(0),
        )?;
        let share_state: String = connection.query_row(
            "
            SELECT share_state
            FROM sessions
            WHERE project_id = 'repo-one-123'
                AND provider = 'codex'
                AND session_id = ?1
            ",
            [local_session_id],
            |row| row.get(0),
        )?;
        let shared: (i64, String) = connection.query_row(
            "
            SELECT COUNT(*), MIN(turn_search.user_message_text)
            FROM sessions
            JOIN turn_search
                ON turn_search.project_id = sessions.project_id
                AND turn_search.provider = sessions.provider
                AND turn_search.session_id = sessions.session_id
            WHERE sessions.project_id = 'repo-one-123'
                AND sessions.origin_kind = 'shared'
            ",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let user_count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM users WHERE user_id = 'usr-remote'",
            [],
            |row| row.get(0),
        )?;

        assert_eq!(policy, "all");
        assert_eq!(share_state, "included");
        assert_eq!(shared, (1, "shared prompt".to_owned()));
        assert_eq!(user_count, 1);

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rebuild_workspace_index_keeps_existing_index_when_project_fails() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_test_dir("workspace-rebuild-failure");
        let first_project_root = root.join("repo-one");
        let second_project_root = root.join("repo-two");
        let first_sessions_root = root.join("projects/repo-one-123/sessions");
        let second_sessions_root = root.join("projects/repo-two-456/sessions");
        fs::create_dir_all(&first_project_root)?;
        fs::create_dir_all(&second_project_root)?;
        write_archived_codex_rollout(
            &first_sessions_root,
            &first_project_root,
            "44444444-4444-4444-8444-44444444444f",
            "Index first project",
            "First indexed",
        )?;
        let unreadable_rollout = second_sessions_root
            .join("codex")
            .join("rollout-2026-04-01T10-00-00-55555555-5555-4555-8555-55555555555f.jsonl");
        write_file(&unreadable_rollout, "unreadable")?;
        fs::set_permissions(&unreadable_rollout, fs::Permissions::from_mode(0o000))?;

        let config = SharedConfig::new(
            root.clone(),
            vec![
                ProjectConfig {
                    id: "repo-one-123".into(),
                    name: "repo-one".into(),
                    local_path: first_project_root,
                    git_upstream: None,
                    sessions_root: first_sessions_root,
                    known_paths: Vec::new(),
                },
                ProjectConfig {
                    id: "repo-two-456".into(),
                    name: "repo-two".into(),
                    local_path: second_project_root,
                    git_upstream: None,
                    sessions_root: second_sessions_root,
                    known_paths: Vec::new(),
                },
            ],
            SourcesConfig::default(),
        );
        fs::write(
            root.join(CONFIG_FILE_NAME),
            toml::to_string_pretty(&config)?,
        )?;
        let index_db_path = root.join(darc_store::INDEX_DB_FILE_NAME);
        let connection = Connection::open(&index_db_path)?;
        connection.execute_batch(
            "
            CREATE TABLE preserved(value TEXT NOT NULL);
            INSERT INTO preserved(value) VALUES ('old index');
            ",
        )?;
        drop(connection);

        let error = rebuild_workspace_index(Some(root.clone())).expect_err("rebuild should fail");
        fs::set_permissions(&unreadable_rollout, fs::Permissions::from_mode(0o600))?;

        assert!(format!("{error:#}").contains("failed to index project `repo-two`"));
        let connection = Connection::open(&index_db_path)?;
        let value: String =
            connection.query_row("SELECT value FROM preserved", [], |row| row.get(0))?;
        assert_eq!(value, "old index");

        Ok(())
    }
}
