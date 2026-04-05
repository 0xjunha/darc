use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;

use crate::{
    active_project::load_active_project, constants::INDEX_DB_FILE_NAME, default_root_path,
    index_db::open_index_database,
};

/// Reports aggregate Claude rollout analytics from the normalized SQLite index.
#[derive(Debug, Clone, Serialize)]
pub struct ClaudeRolloutAnalyticsReport {
    pub project_name: String,
    pub project_root: PathBuf,
    pub index_db_path: PathBuf,
    pub sessions_total: u64,
    pub primary_sessions: u64,
    pub subagent_sessions: u64,
    pub exact_sessions: u64,
    pub best_effort_sessions: u64,
    pub turns_total: u64,
    pub completed_turns: u64,
    pub incomplete_turns: u64,
    pub aborted_turns: u64,
    pub turns_with_final_answer: u64,
    pub turns_with_attachments: u64,
    pub turns_with_delegation: u64,
    pub total_step_count: u64,
    pub total_tool_calls: u64,
    pub total_tool_outputs: u64,
    pub total_attachments: u64,
    pub total_delegation_events: u64,
    pub total_hook_summaries: u64,
    pub total_duration_ms: u64,
    pub average_duration_ms: Option<f64>,
    pub schemas: Vec<ClaudeSchemaAnalytics>,
}

/// Stores one per-schema Claude analytics row.
#[derive(Debug, Clone, Serialize)]
pub struct ClaudeSchemaAnalytics {
    pub schema_id: String,
    pub session_count: u64,
    pub turn_count: u64,
    pub exact_session_count: u64,
    pub best_effort_session_count: u64,
}

/// Reports Claude rollout analytics for the active darc project.
pub fn report_claude_rollout_analytics(
    root: Option<PathBuf>,
) -> Result<ClaudeRolloutAnalyticsReport> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    report_claude_rollout_analytics_from(&current_dir, root.unwrap_or_else(default_root_path))
}

/// Reports Claude rollout analytics for one explicit current directory and darc root.
pub(crate) fn report_claude_rollout_analytics_from(
    current_dir: &Path,
    root: PathBuf,
) -> Result<ClaudeRolloutAnalyticsReport> {
    let active_project = load_active_project(current_dir, &root)?;
    let index_db_path = root.join(INDEX_DB_FILE_NAME);
    let connection = open_index_database(&index_db_path)?;

    let (sessions_total, primary_sessions, subagent_sessions, exact_sessions, best_effort_sessions) =
        query_claude_session_counts(&connection, &active_project.project.id)?;
    let turn_counts = query_claude_turn_counts(&connection, &active_project.project.id)?;
    let schemas = query_claude_schema_rows(&connection, &active_project.project.id)?;

    Ok(ClaudeRolloutAnalyticsReport {
        project_name: active_project.project.name,
        project_root: active_project.current_root,
        index_db_path,
        sessions_total,
        primary_sessions,
        subagent_sessions,
        exact_sessions,
        best_effort_sessions,
        turns_total: turn_counts.turns_total,
        completed_turns: turn_counts.completed_turns,
        incomplete_turns: turn_counts.incomplete_turns,
        aborted_turns: turn_counts.aborted_turns,
        turns_with_final_answer: turn_counts.turns_with_final_answer,
        turns_with_attachments: turn_counts.turns_with_attachments,
        turns_with_delegation: turn_counts.turns_with_delegation,
        total_step_count: turn_counts.total_step_count,
        total_tool_calls: turn_counts.total_tool_calls,
        total_tool_outputs: turn_counts.total_tool_outputs,
        total_attachments: turn_counts.total_attachments,
        total_delegation_events: turn_counts.total_delegation_events,
        total_hook_summaries: turn_counts.total_hook_summaries,
        total_duration_ms: turn_counts.total_duration_ms,
        average_duration_ms: turn_counts.average_duration_ms,
        schemas,
    })
}

#[derive(Debug, Clone, Copy, Default)]
struct ClaudeTurnCounts {
    turns_total: u64,
    completed_turns: u64,
    incomplete_turns: u64,
    aborted_turns: u64,
    turns_with_final_answer: u64,
    turns_with_attachments: u64,
    turns_with_delegation: u64,
    total_step_count: u64,
    total_tool_calls: u64,
    total_tool_outputs: u64,
    total_attachments: u64,
    total_delegation_events: u64,
    total_hook_summaries: u64,
    total_duration_ms: u64,
    average_duration_ms: Option<f64>,
}

/// Stores one raw SQLite aggregate row for indexed Claude turn analytics.
type ClaudeTurnCountRow = (
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    Option<f64>,
);

/// Queries the indexed Claude session counts for one project.
fn query_claude_session_counts(
    connection: &Connection,
    project_id: &str,
) -> Result<(u64, u64, u64, u64, u64)> {
    let counts: (i64, i64, i64, i64, i64) = connection
        .query_row(
            "
            SELECT
                COUNT(*),
                COALESCE(SUM(CASE WHEN session_kind = 'primary' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN session_kind = 'subagent' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN determinism = 'exact' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN determinism = 'best_effort_forward' THEN 1 ELSE 0 END), 0)
            FROM sessions
            WHERE project_id = ?1 AND provider = 'claude'
            ",
            [project_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .context("failed to query indexed Claude session counts")?;
    Ok((
        sql_count_to_u64(counts.0, "sessions_total")?,
        sql_count_to_u64(counts.1, "primary_sessions")?,
        sql_count_to_u64(counts.2, "subagent_sessions")?,
        sql_count_to_u64(counts.3, "exact_sessions")?,
        sql_count_to_u64(counts.4, "best_effort_sessions")?,
    ))
}

/// Queries the indexed Claude turn counters for one project.
fn query_claude_turn_counts(connection: &Connection, project_id: &str) -> Result<ClaudeTurnCounts> {
    let counts: ClaudeTurnCountRow = connection
        .query_row(
            "
            SELECT
                COUNT(*),
                COALESCE(SUM(CASE WHEN status = 'completed' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'incomplete' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'aborted' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(has_final_answer), 0),
                COALESCE(SUM(CASE WHEN attachment_count > 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN delegation_count > 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(step_count), 0),
                COALESCE(SUM(tool_call_count), 0),
                COALESCE(SUM(tool_output_count), 0),
                COALESCE(SUM(attachment_count), 0),
                COALESCE(SUM(delegation_count), 0),
                COALESCE(SUM(hook_summary_count), 0),
                AVG(duration_ms)
            FROM turns
            WHERE project_id = ?1 AND provider = 'claude'
            ",
            [project_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                ))
            },
        )
        .context("failed to query indexed Claude turn counts")?;

    let total_duration_ms: i64 = connection
        .query_row(
            "
            SELECT COALESCE(SUM(duration_ms), 0)
            FROM turns
            WHERE project_id = ?1 AND provider = 'claude'
            ",
            [project_id],
            |row| row.get(0),
        )
        .context("failed to query indexed Claude duration totals")?;

    Ok(ClaudeTurnCounts {
        turns_total: sql_count_to_u64(counts.0, "turns_total")?,
        completed_turns: sql_count_to_u64(counts.1, "completed_turns")?,
        incomplete_turns: sql_count_to_u64(counts.2, "incomplete_turns")?,
        aborted_turns: sql_count_to_u64(counts.3, "aborted_turns")?,
        turns_with_final_answer: sql_count_to_u64(counts.4, "turns_with_final_answer")?,
        turns_with_attachments: sql_count_to_u64(counts.5, "turns_with_attachments")?,
        turns_with_delegation: sql_count_to_u64(counts.6, "turns_with_delegation")?,
        total_step_count: sql_count_to_u64(counts.7, "total_step_count")?,
        total_tool_calls: sql_count_to_u64(counts.8, "total_tool_calls")?,
        total_tool_outputs: sql_count_to_u64(counts.9, "total_tool_outputs")?,
        total_attachments: sql_count_to_u64(counts.10, "total_attachments")?,
        total_delegation_events: sql_count_to_u64(counts.11, "total_delegation_events")?,
        total_hook_summaries: sql_count_to_u64(counts.12, "total_hook_summaries")?,
        total_duration_ms: sql_count_to_u64(total_duration_ms, "total_duration_ms")?,
        average_duration_ms: counts.13,
    })
}

/// Queries per-schema indexed Claude session and turn counts for one project.
fn query_claude_schema_rows(
    connection: &Connection,
    project_id: &str,
) -> Result<Vec<ClaudeSchemaAnalytics>> {
    let mut statement = connection
        .prepare(
            "
            SELECT
                s.schema_id,
                COUNT(*),
                COALESCE(SUM(CASE WHEN s.determinism = 'exact' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN s.determinism = 'best_effort_forward' THEN 1 ELSE 0 END), 0)
            FROM sessions s
            WHERE s.project_id = ?1 AND s.provider = 'claude'
            GROUP BY s.schema_id
            ORDER BY s.schema_id
            ",
        )
        .context("failed to prepare indexed Claude schema query")?;
    let mut rows = statement
        .query([project_id])
        .context("failed to query indexed Claude schema rows")?;
    let mut schemas = Vec::new();

    while let Some(row) = rows
        .next()
        .context("failed to read indexed Claude schema row")?
    {
        let schema_id: Option<String> = row.get(0).context("failed to read Claude schema id")?;
        let schema_id = schema_id.unwrap_or_else(|| "<unknown>".to_owned());
        let session_count = sql_count_to_u64(
            row.get::<_, i64>(1)
                .context("failed to read Claude schema session count")?,
            "schema session_count",
        )?;
        let exact_session_count = sql_count_to_u64(
            row.get::<_, i64>(2)
                .context("failed to read Claude schema exact session count")?,
            "schema exact_session_count",
        )?;
        let best_effort_session_count = sql_count_to_u64(
            row.get::<_, i64>(3)
                .context("failed to read Claude schema best-effort session count")?,
            "schema best_effort_session_count",
        )?;
        let turn_count = query_schema_turn_count(connection, project_id, &schema_id)?;
        schemas.push(ClaudeSchemaAnalytics {
            schema_id,
            session_count,
            turn_count,
            exact_session_count,
            best_effort_session_count,
        });
    }

    Ok(schemas)
}

/// Queries the indexed turn count for one Claude schema id.
fn query_schema_turn_count(
    connection: &Connection,
    project_id: &str,
    schema_id: &str,
) -> Result<u64> {
    let count: i64 = connection
        .query_row(
            "
            SELECT COUNT(*)
            FROM turns t
            JOIN sessions s
                ON s.project_id = t.project_id
                AND s.provider = t.provider
                AND s.session_id = t.session_id
            WHERE t.project_id = ?1 AND t.provider = 'claude' AND COALESCE(s.schema_id, '<unknown>') = ?2
            ",
            (project_id, schema_id),
            |row| row.get(0),
        )
        .with_context(|| format!("failed to count indexed Claude turns for schema `{schema_id}`"))?;
    sql_count_to_u64(count, "schema turn_count")
}

/// Converts one SQLite aggregate count into an unsigned Rust count.
fn sql_count_to_u64(value: i64, label: &str) -> Result<u64> {
    u64::try_from(value).with_context(|| format!("{label} exceeded u64 range"))
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use anyhow::Result;

    use super::report_claude_rollout_analytics_from;
    use crate::{
        config::{ProjectConfig, SharedConfig, SourcesConfig},
        constants::{CONFIG_FILE_NAME, INDEX_DB_FILE_NAME},
        index::index_project_sessions_from,
    };

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("test-{prefix}-{}-{nanos}", std::process::id()))
    }

    fn write_file(path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
        Ok(())
    }

    fn write_index_config(root: &Path, project_root: &Path, sessions_root: &Path) -> Result<()> {
        let config = SharedConfig::new(
            root.to_path_buf(),
            vec![ProjectConfig {
                id: "repo-abc123".into(),
                name: "repo".into(),
                local_path: project_root.to_path_buf(),
                git_upstream: None,
                sessions_root: sessions_root.to_path_buf(),
                known_paths: Vec::new(),
            }],
            SourcesConfig::default(),
        );
        write_file(
            &root.join(CONFIG_FILE_NAME),
            &toml::to_string_pretty(&config)?,
        )
    }

    #[test]
    fn reports_indexed_claude_rollout_analytics() -> Result<()> {
        let darc_root = unique_test_dir("claude-analytics");
        let project_root = darc_root.join("repo");
        let sessions_root = darc_root.join("projects/repo-abc123/sessions");
        let claude_root = sessions_root.join("claude");
        let session_id = "session-analytics";
        let session_path = claude_root
            .join(session_id)
            .join(format!("{session_id}.jsonl"));
        fs::create_dir_all(&project_root)?;
        write_index_config(&darc_root, &project_root, &sessions_root)?;

        write_file(
            &session_path,
            concat!(
                "{\"parentUuid\":null,\"isSidechain\":false,\"promptId\":\"prompt-1\",\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Delegate README.md\"},\"uuid\":\"user-1\",\"timestamp\":\"2026-04-01T00:00:01Z\",\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",\"sessionId\":\"session-analytics\",\"version\":\"2.1.90\",\"gitBranch\":\"HEAD\"}\n",
                "{\"parentUuid\":\"user-1\",\"isSidechain\":false,\"attachment\":{\"type\":\"deferred_tools_delta\",\"addedNames\":[\"Agent\"],\"addedLines\":[\"Agent\"],\"removedNames\":[]},\"type\":\"attachment\",\"uuid\":\"attachment-1\",\"timestamp\":\"2026-04-01T00:00:01Z\",\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",\"sessionId\":\"session-analytics\",\"version\":\"2.1.90\",\"gitBranch\":\"HEAD\"}\n",
                "{\"parentUuid\":\"attachment-1\",\"isSidechain\":false,\"message\":{\"model\":\"claude-sonnet-4-6\",\"id\":\"assistant-1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"tool-1\",\"name\":\"Agent\",\"input\":{\"description\":\"Read README heading\",\"prompt\":\"Read README.md and return the first heading.\",\"subagent_type\":\"general-purpose\"}}],\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"requestId\":\"req-1\",\"type\":\"assistant\",\"uuid\":\"assistant-1\",\"timestamp\":\"2026-04-01T00:00:02Z\",\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",\"sessionId\":\"session-analytics\",\"version\":\"2.1.90\",\"gitBranch\":\"HEAD\"}\n",
                "{\"parentUuid\":\"assistant-1\",\"isSidechain\":false,\"type\":\"system\",\"subtype\":\"task_started\",\"task_id\":\"task-1\",\"tool_use_id\":\"tool-1\",\"description\":\"Read README heading\",\"task_type\":\"local_agent\",\"prompt\":\"Read README.md and return the first heading.\",\"uuid\":\"system-1\",\"timestamp\":\"2026-04-01T00:00:03Z\",\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",\"sessionId\":\"session-analytics\",\"version\":\"2.1.90\",\"gitBranch\":\"HEAD\"}\n",
                "{\"parentUuid\":\"assistant-1\",\"isSidechain\":false,\"promptId\":\"prompt-1\",\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"tool_use_id\":\"tool-1\",\"type\":\"tool_result\",\"content\":[{\"type\":\"text\",\"text\":\"# Audit Fixture\"},{\"type\":\"text\",\"text\":\"agentId: agent-1\"}]}]},\"uuid\":\"user-2\",\"timestamp\":\"2026-04-01T00:00:04Z\",\"toolUseResult\":{\"status\":\"completed\",\"prompt\":\"Read README.md and return the first heading.\",\"agentId\":\"agent-1\",\"agentType\":\"general-purpose\",\"content\":[{\"type\":\"text\",\"text\":\"# Audit Fixture\"}],\"totalDurationMs\":12,\"totalTokens\":34,\"totalToolUseCount\":1},\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",\"sessionId\":\"session-analytics\",\"version\":\"2.1.90\",\"gitBranch\":\"HEAD\"}\n",
                "{\"parentUuid\":\"user-2\",\"isSidechain\":false,\"type\":\"system\",\"subtype\":\"stop_hook_summary\",\"hookCount\":2,\"hookInfos\":[{\"command\":\"callback\",\"durationMs\":12}],\"hookErrors\":[],\"preventedContinuation\":false,\"stopReason\":\"\",\"hasOutput\":true,\"level\":\"suggestion\",\"timestamp\":\"2026-04-01T00:00:05Z\",\"uuid\":\"system-2\",\"toolUseID\":\"tool-1\",\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",\"sessionId\":\"session-analytics\",\"version\":\"2.1.90\",\"gitBranch\":\"HEAD\"}\n",
                "{\"parentUuid\":\"user-2\",\"isSidechain\":false,\"message\":{\"model\":\"claude-sonnet-4-6\",\"id\":\"assistant-2\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"# Audit Fixture\"}],\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"requestId\":\"req-2\",\"type\":\"assistant\",\"uuid\":\"assistant-2\",\"timestamp\":\"2026-04-01T00:00:06Z\",\"userType\":\"external\",\"entrypoint\":\"sdk-cli\",\"cwd\":\"/tmp/repo\",\"sessionId\":\"session-analytics\",\"version\":\"2.1.90\",\"gitBranch\":\"HEAD\"}\n"
            ),
        )?;

        let _report = index_project_sessions_from(
            &project_root,
            darc_root.clone(),
            &[crate::SourceKind::Claude],
        )?;
        let analytics = report_claude_rollout_analytics_from(&project_root, darc_root.clone())?;

        assert_eq!(analytics.project_name, "repo");
        assert_eq!(analytics.project_root, fs::canonicalize(&project_root)?);
        assert_eq!(analytics.index_db_path, darc_root.join(INDEX_DB_FILE_NAME));
        assert_eq!(analytics.sessions_total, 1);
        assert_eq!(analytics.primary_sessions, 1);
        assert_eq!(analytics.subagent_sessions, 0);
        assert_eq!(analytics.exact_sessions, 0);
        assert_eq!(analytics.best_effort_sessions, 1);
        assert_eq!(analytics.turns_total, 1);
        assert_eq!(analytics.completed_turns, 1);
        assert_eq!(analytics.incomplete_turns, 0);
        assert_eq!(analytics.aborted_turns, 0);
        assert_eq!(analytics.turns_with_final_answer, 1);
        assert_eq!(analytics.turns_with_attachments, 1);
        assert_eq!(analytics.turns_with_delegation, 1);
        assert_eq!(analytics.total_step_count, 6);
        assert_eq!(analytics.total_tool_calls, 1);
        assert_eq!(analytics.total_tool_outputs, 1);
        assert_eq!(analytics.total_attachments, 1);
        assert_eq!(analytics.total_delegation_events, 2);
        assert_eq!(analytics.total_hook_summaries, 1);
        assert_eq!(analytics.total_duration_ms, 5_000);
        assert_eq!(analytics.average_duration_ms, Some(5_000.0));
        assert_eq!(analytics.schemas.len(), 1);
        assert_eq!(
            analytics.schemas[0].schema_id,
            "claude.primary_transcript.2_1_90_to_latest"
        );
        assert_eq!(analytics.schemas[0].session_count, 1);
        assert_eq!(analytics.schemas[0].turn_count, 1);
        assert_eq!(analytics.schemas[0].exact_session_count, 0);
        assert_eq!(analytics.schemas[0].best_effort_session_count, 1);

        Ok(())
    }
}
