use anyhow::{Context, Result};
use darc_paths::SourceKind;
use darc_rollout::model::NormalizedTurnStep;
use rusqlite::Connection;

use crate::{
    derived_data::{TurnDerivedContext, insert_turn_derived_records},
    index_db::schema::{INSERT_SESSION_SQL, INSERT_TURN_SQL},
};

const CREATE_PRE_ANALYTICS_INDEX_SCHEMA_SQL: &str = "
    CREATE TABLE sessions (
        project_id TEXT NOT NULL,
        provider TEXT NOT NULL,
        session_id TEXT NOT NULL,
        parent_session_id TEXT,
        session_kind TEXT NOT NULL,
        archive_path TEXT NOT NULL,
        cwd TEXT NOT NULL,
        cli_version TEXT,
        schema_id TEXT,
        determinism TEXT,
        source_size INTEGER,
        source_mtime_ms INTEGER,
        PRIMARY KEY (project_id, provider, session_id),
        UNIQUE (project_id, archive_path)
    );

    CREATE TABLE turns (
        project_id TEXT NOT NULL,
        provider TEXT NOT NULL,
        session_id TEXT NOT NULL,
        turn_ordinal INTEGER NOT NULL,
        turn_id TEXT,
        started_at TEXT NOT NULL,
        completed_at TEXT,
        status TEXT NOT NULL,
        user_message TEXT NOT NULL,
        final_answer_at TEXT,
        final_answer_text TEXT,
        steps_json TEXT NOT NULL,
        PRIMARY KEY (project_id, provider, session_id, turn_ordinal),
        FOREIGN KEY (project_id, provider, session_id)
            REFERENCES sessions(project_id, provider, session_id)
            ON DELETE CASCADE
    );
";

const INSERT_PRE_ANALYTICS_TURN_SQL: &str = "
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
        steps_json
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
";

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

/// Creates one normalized index schema snapshot from before derived analytics existed.
pub fn create_pre_analytics_index_schema(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(CREATE_PRE_ANALYTICS_INDEX_SCHEMA_SQL)
        .context("failed to create pre-analytics index schema")?;
    Ok(())
}

/// Inserts one indexed session row for SQLite-backed tests.
pub fn insert_indexed_session(
    connection: &Connection,
    fixture: IndexedSessionFixture<'_>,
) -> Result<()> {
    connection.execute(
        INSERT_SESSION_SQL,
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
            "0.118.0",
            "fixture",
            "exact",
            1_i64,
            1_i64,
        ),
    )?;
    Ok(())
}

/// Inserts one turn row into a pre-analytics normalized schema snapshot.
pub fn insert_pre_analytics_turn(
    connection: &Connection,
    fixture: IndexedTurnFixture<'_>,
) -> Result<()> {
    let final_answer_at = fixture
        .final_answer_at
        .or_else(|| fixture.has_final_answer.then_some(fixture.started_at));
    let final_answer_text = fixture
        .final_answer_text
        .or_else(|| fixture.has_final_answer.then_some("done"));
    connection.execute(
        INSERT_PRE_ANALYTICS_TURN_SQL,
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
        ],
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
        INSERT_TURN_SQL,
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

    let steps = serde_json::from_str::<Vec<NormalizedTurnStep>>(fixture.steps_json)
        .context("fixture steps_json should parse")?;
    insert_turn_derived_records(
        connection,
        &TurnDerivedContext {
            project_id: fixture.project_id,
            provider: fixture.provider,
            session_id: fixture.session_id,
            turn_ordinal: fixture.turn_ordinal,
            user_message: fixture.user_message,
            final_answer_text,
        },
        &steps,
    )?;

    Ok(())
}

/// Seeds one representative legacy Codex-only index fixture for migration tests.
pub fn seed_legacy_codex_index(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "
        CREATE TABLE codex_sessions (
            project_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            archive_path TEXT NOT NULL,
            cwd TEXT NOT NULL,
            PRIMARY KEY (project_id, session_id),
            UNIQUE (project_id, archive_path)
        );

        CREATE TABLE codex_turns (
            project_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            turn_ordinal INTEGER NOT NULL,
            turn_id TEXT,
            started_at TEXT NOT NULL,
            completed_at TEXT,
            status TEXT NOT NULL,
            user_message TEXT NOT NULL,
            final_answer_at TEXT,
            final_answer_text TEXT,
            steps_json TEXT NOT NULL,
            PRIMARY KEY (project_id, session_id, turn_ordinal)
        );

        INSERT INTO codex_sessions (project_id, session_id, archive_path, cwd)
        VALUES ('project', 'session', 'codex/rollout.jsonl', '/tmp/repo');

        INSERT INTO codex_turns (
            project_id,
            session_id,
            turn_ordinal,
            turn_id,
            started_at,
            completed_at,
            status,
            user_message,
            final_answer_at,
            final_answer_text,
            steps_json
        )
        VALUES (
            'project',
            'session',
            0,
            'turn-1',
            '2026-04-01T00:00:00Z',
            '2026-04-01T00:00:01Z',
            'completed',
            'Task',
            '2026-04-01T00:00:01Z',
            'Reply',
            '[]'
        );
        ",
    )?;
    Ok(())
}
