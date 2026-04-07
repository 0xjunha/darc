use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use darc_index::policy::{derive_file_access_records, extract_tool_call_records};
use darc_paths::SourceKind;
use darc_rollout::model::NormalizedTurnStep;
use rusqlite::Connection;

static UNIQUE_TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Stores one indexed session fixture used by SQLite-backed tests.
pub struct IndexedSessionFixture<'a> {
    pub project_id: &'a str,
    pub provider: SourceKind,
    pub session_id: &'a str,
    pub parent_session_id: Option<&'a str>,
    pub session_kind: &'a str,
    pub cwd: &'a str,
}

impl<'a> IndexedSessionFixture<'a> {
    /// Builds one primary-session fixture with the common defaults.
    pub fn new(
        project_id: &'a str,
        provider: SourceKind,
        session_id: &'a str,
        cwd: &'a str,
    ) -> Self {
        Self {
            project_id,
            provider,
            session_id,
            parent_session_id: None,
            session_kind: "primary",
            cwd,
        }
    }
}

/// Stores one indexed turn fixture used by SQLite-backed tests.
pub struct IndexedTurnFixture<'a> {
    pub project_id: &'a str,
    pub provider: SourceKind,
    pub session_id: &'a str,
    pub turn_ordinal: i64,
    pub turn_id: Option<&'a str>,
    pub started_at: &'a str,
    pub completed_at: Option<&'a str>,
    pub status: &'a str,
    pub user_message: &'a str,
    pub final_answer_at: Option<&'a str>,
    pub final_answer_text: Option<&'a str>,
    pub steps_json: &'a str,
    pub step_count: i64,
    pub tool_call_count: i64,
    pub tool_output_count: i64,
    pub attachment_count: i64,
    pub delegation_count: i64,
    pub hook_summary_count: i64,
    pub has_final_answer: bool,
    pub duration_ms: i64,
}

impl<'a> IndexedTurnFixture<'a> {
    /// Builds one indexed turn fixture with default optional fields and zeroed metrics.
    pub fn new(
        project_id: &'a str,
        provider: SourceKind,
        session_id: &'a str,
        turn_ordinal: i64,
        started_at: &'a str,
        status: &'a str,
        steps_json: &'a str,
    ) -> Self {
        Self {
            project_id,
            provider,
            session_id,
            turn_ordinal,
            turn_id: None,
            started_at,
            completed_at: Some(started_at),
            status,
            user_message: "Inspect the repo",
            final_answer_at: None,
            final_answer_text: None,
            steps_json,
            step_count: 0,
            tool_call_count: 0,
            tool_output_count: 0,
            attachment_count: 0,
            delegation_count: 0,
            hook_summary_count: 0,
            has_final_answer: false,
            duration_ms: 0,
        }
    }
}

/// Builds one unique temporary directory path for filesystem-based tests.
pub fn unique_test_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    let counter = UNIQUE_TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!(
        "test-{prefix}-{}-{nanos}-{counter}",
        std::process::id()
    ))
}

/// Writes one UTF-8 test file after creating any missing parent directories.
pub fn write_file(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().context("missing parent directory")?;
    fs::create_dir_all(parent)?;
    fs::write(path, content)?;
    Ok(())
}

/// Inserts one indexed session row for SQLite-backed tests.
pub fn insert_indexed_session(
    connection: &Connection,
    fixture: IndexedSessionFixture<'_>,
) -> Result<()> {
    connection.execute(
        "
        INSERT INTO sessions (
            project_id,
            provider,
            session_id,
            parent_session_id,
            session_kind,
            archive_path,
            cwd,
            cli_version,
            schema_id,
            determinism,
            source_size,
            source_mtime_ms
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '0.118.0', 'fixture', 'exact', 1, 1)
        ",
        (
            fixture.project_id,
            fixture.provider.directory_name(),
            fixture.session_id,
            fixture.parent_session_id,
            fixture.session_kind,
            format!(
                "{}/{}.jsonl",
                fixture.provider.directory_name(),
                fixture.session_id
            ),
            fixture.cwd,
        ),
    )?;
    Ok(())
}

/// Inserts one indexed turn row plus derived analytics rows for SQLite-backed tests.
pub fn insert_indexed_turn(connection: &Connection, fixture: IndexedTurnFixture<'_>) -> Result<()> {
    let final_answer_at = fixture
        .final_answer_at
        .or_else(|| fixture.has_final_answer.then_some(fixture.started_at));
    let final_answer_text = fixture
        .final_answer_text
        .or_else(|| fixture.has_final_answer.then_some("done"));
    connection.execute(
        "
        INSERT INTO turns (
            project_id,
            provider,
            session_id,
            turn_ordinal,
            turn_id,
            started_at,
            completed_at,
            status,
            user_message,
            final_answer_at,
            final_answer_text,
            steps_json,
            step_count,
            tool_call_count,
            tool_output_count,
            attachment_count,
            delegation_count,
            hook_summary_count,
            has_final_answer,
            duration_ms
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
        ",
        rusqlite::params![
            fixture.project_id,
            fixture.provider.directory_name(),
            fixture.session_id,
            fixture.turn_ordinal,
            fixture.turn_id,
            fixture.started_at,
            fixture.completed_at,
            fixture.status,
            fixture.user_message,
            final_answer_at,
            final_answer_text,
            fixture.steps_json,
            fixture.step_count,
            fixture.tool_call_count,
            fixture.tool_output_count,
            fixture.attachment_count,
            fixture.delegation_count,
            fixture.hook_summary_count,
            i64::from(fixture.has_final_answer),
            fixture.duration_ms,
        ],
    )?;

    let turn_ordinal =
        u64::try_from(fixture.turn_ordinal).context("fixture turn ordinal must be non-negative")?;
    let steps = serde_json::from_str::<Vec<NormalizedTurnStep>>(fixture.steps_json)
        .context("fixture steps_json should parse")?;
    let tool_calls = extract_tool_call_records(
        fixture.project_id,
        fixture.provider,
        fixture.session_id,
        turn_ordinal,
        &steps,
    );

    for record in &tool_calls {
        connection.execute(
            "
            INSERT INTO tool_calls (
                project_id,
                provider,
                session_id,
                turn_ordinal,
                call_ordinal,
                call_id,
                timestamp,
                tool_name,
                arguments_text,
                output_text,
                status,
                is_error
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ",
            rusqlite::params![
                record.project_id.as_str(),
                fixture.provider.directory_name(),
                record.session_id.as_str(),
                i64::try_from(record.turn_ordinal)
                    .context("turn ordinal exceeds SQLite INTEGER range")?,
                i64::try_from(record.call_ordinal)
                    .context("call ordinal exceeds SQLite INTEGER range")?,
                record.call_id.as_str(),
                record.timestamp.as_str(),
                record.tool_name.as_deref(),
                record.arguments_text.as_deref(),
                record.output_text.as_deref(),
                record.status.as_deref(),
                i64::from(record.is_error),
            ],
        )?;
    }

    for record in derive_file_access_records(&tool_calls) {
        connection.execute(
            "
            INSERT INTO file_accesses (
                project_id,
                provider,
                session_id,
                turn_ordinal,
                call_ordinal,
                call_id,
                timestamp,
                tool_name,
                access_type,
                path,
                repo_relative_path
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ",
            rusqlite::params![
                record.project_id.as_str(),
                fixture.provider.directory_name(),
                record.session_id.as_str(),
                i64::try_from(record.turn_ordinal)
                    .context("turn ordinal exceeds SQLite INTEGER range")?,
                i64::try_from(record.call_ordinal)
                    .context("call ordinal exceeds SQLite INTEGER range")?,
                record.call_id.as_str(),
                record.timestamp.as_str(),
                record.tool_name.as_str(),
                record.access_type.as_sql_text(),
                record.path.as_str(),
                record.repo_relative_path.as_deref(),
            ],
        )?;
    }

    Ok(())
}

/// Runs one Git command in a test repository and returns an error on failure.
pub fn run_git(cwd: &Path, args: &[&str]) -> Result<()> {
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

/// Initializes one Git repository fixture with a fixed identity and remote.
pub fn init_git_repo(path: &Path, remote: &str) -> Result<()> {
    fs::create_dir_all(path)?;
    run_git(path, &["init"])?;
    run_git(path, &["config", "user.name", "Darc Test"])?;
    run_git(path, &["config", "user.email", "darc-tests@example.com"])?;
    run_git(path, &["config", "commit.gpgsign", "false"])?;
    run_git(path, &["remote", "add", "origin", remote])
}
