use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use darc_paths::SourceKind;
use darc_rollout::model::{NormalizedTurnStatus, NormalizedTurnStep};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{
    derived_data::{TurnDerivedContext, insert_turn_derived_records},
    index_db::schema::INSERT_TURN_SQL,
};

const DEFAULT_SHARE_POLICY: SharePolicy = SharePolicy::Manual;

/// Identifies whether one indexed session is local-only or imported from sharing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginKind {
    Local,
    Shared,
}

impl OriginKind {
    /// Returns the stable SQLite representation for one origin kind.
    pub fn as_sql_text(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Shared => "shared",
        }
    }
}

/// Identifies the default project-level sharing policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharePolicy {
    Manual,
    All,
}

impl SharePolicy {
    /// Returns the stable SQLite representation for one share policy.
    pub fn as_sql_text(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::All => "all",
        }
    }
}

/// Identifies one session-level sharing override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareState {
    Unset,
    Included,
    Excluded,
}

impl ShareState {
    /// Returns the stable SQLite representation for one share state.
    pub fn as_sql_text(self) -> &'static str {
        match self {
            Self::Unset => "unset",
            Self::Included => "included",
            Self::Excluded => "excluded",
        }
    }
}

/// Stores one share user row in the SQLite index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareUserRecord {
    pub user_id: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub public_key: Option<String>,
    pub source: String,
    pub updated_at: String,
}

/// Stores one session provenance block exposed in query payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProvenance {
    pub origin_kind: OriginKind,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub user_email: Option<String>,
    pub origin_remote: Option<String>,
    pub imported_at: Option<String>,
}

/// Stores project-level sharing status for CLI reporting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareStatus {
    pub project_id: String,
    pub default_policy: SharePolicy,
    pub local_session_count: u64,
    pub shared_session_count: u64,
    pub selected_session_count: u64,
    pub included_session_count: u64,
    pub excluded_session_count: u64,
    pub unset_session_count: u64,
}

/// Stores one canonical session row selected for export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareSessionExport {
    pub project_id: String,
    pub provider: SourceKind,
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub session_kind: String,
    pub archive_path: String,
    pub cwd: String,
    pub cli_version: Option<String>,
    pub schema_id: Option<String>,
    pub determinism: Option<String>,
    pub source_size: Option<i64>,
    pub source_mtime_ms: Option<i64>,
}

/// Stores one canonical turn row selected for export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareTurnExport {
    pub session: ShareSessionExport,
    pub turn_ordinal: i64,
    pub turn_id: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub status: String,
    pub user_message: String,
    pub final_answer_at: Option<String>,
    pub final_answer_text: Option<String>,
    pub steps_json: String,
    pub step_count: i64,
    pub tool_call_count: i64,
    pub tool_output_count: i64,
    pub attachment_count: i64,
    pub delegation_count: i64,
    pub hook_summary_count: i64,
    pub has_final_answer: i64,
    pub duration_ms: Option<i64>,
    pub effective_agent_runtime_ms: Option<i64>,
    pub provider_total_token_count: Option<i64>,
    pub input_uncached_token_count: Option<i64>,
    pub cache_read_token_count: Option<i64>,
    pub cache_write_token_count: Option<i64>,
    pub output_token_count: Option<i64>,
    pub reasoning_token_count: Option<i64>,
    pub total_token_count: Option<i64>,
    pub primary_model: Option<String>,
    pub changed_file_count: i64,
    pub added_line_count: i64,
    pub removed_line_count: i64,
}

/// Stores one imported shared turn plus the provenance needed for its parent session.
#[derive(Debug, Clone, Copy)]
pub struct ShareTurnImport<'a> {
    pub project_id: &'a str,
    pub user: &'a ShareUserRecord,
    pub remote_name: &'a str,
    pub imported_at: &'a str,
    pub turn: &'a ShareTurnExport,
}

/// Parses one SQLite origin-kind value.
pub fn parse_origin_kind(value: &str) -> Result<OriginKind> {
    match value {
        "local" => Ok(OriginKind::Local),
        "shared" => Ok(OriginKind::Shared),
        other => bail!("unsupported session origin kind `{other}` in index"),
    }
}

/// Parses one SQLite share-policy value.
pub fn parse_share_policy(value: &str) -> Result<SharePolicy> {
    match value {
        "manual" => Ok(SharePolicy::Manual),
        "all" => Ok(SharePolicy::All),
        other => bail!("unsupported project share policy `{other}` in index"),
    }
}

/// Parses one SQLite share-state value.
pub fn parse_share_state(value: &str) -> Result<ShareState> {
    match value {
        "unset" => Ok(ShareState::Unset),
        "included" => Ok(ShareState::Included),
        "excluded" => Ok(ShareState::Excluded),
        other => bail!("unsupported session share state `{other}` in index"),
    }
}

/// Upserts one share user row.
pub fn upsert_share_user(connection: &Connection, user: &ShareUserRecord) -> Result<()> {
    connection
        .execute(
            "
            INSERT INTO users (user_id, display_name, email, public_key, source, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(user_id) DO UPDATE SET
                display_name = excluded.display_name,
                email = excluded.email,
                public_key = excluded.public_key,
                source = excluded.source,
                updated_at = excluded.updated_at
            ",
            params![
                user.user_id,
                user.display_name,
                user.email,
                user.public_key,
                user.source,
                user.updated_at
            ],
        )
        .context("failed to upsert shared Darc user")?;
    Ok(())
}

/// Reads one project share policy or returns the V1 default.
pub fn project_share_policy(connection: &Connection, project_id: &str) -> Result<SharePolicy> {
    let value = connection
        .query_row(
            "SELECT default_policy FROM project_share_policies WHERE project_id = ?1",
            params![project_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .with_context(|| format!("failed to read share policy for project `{project_id}`"))?;
    value
        .as_deref()
        .map(parse_share_policy)
        .transpose()
        .map(|policy| policy.unwrap_or(DEFAULT_SHARE_POLICY))
}

/// Upserts one project share policy.
pub fn set_project_share_policy(
    connection: &Connection,
    project_id: &str,
    policy: SharePolicy,
    updated_at: &str,
) -> Result<()> {
    connection
        .execute(
            "
            INSERT INTO project_share_policies (project_id, default_policy, updated_at)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(project_id) DO UPDATE SET
                default_policy = excluded.default_policy,
                updated_at = excluded.updated_at
            ",
            params![project_id, policy.as_sql_text(), updated_at],
        )
        .with_context(|| format!("failed to set share policy for project `{project_id}`"))?;
    Ok(())
}

/// Sets one local session sharing override and returns the affected row count.
pub fn set_session_share_state(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    state: ShareState,
) -> Result<usize> {
    connection
        .execute(
            "
            UPDATE sessions
            SET share_state = ?1
            WHERE project_id = ?2
                AND provider = ?3
                AND session_id = ?4
                AND origin_kind = 'local'
            ",
            params![
                state.as_sql_text(),
                project_id,
                provider.directory_name(),
                session_id
            ],
        )
        .with_context(|| format!("failed to update sharing state for session `{session_id}`"))
}

/// Clears all local session sharing overrides for one project.
pub fn clear_project_share_states(connection: &Connection, project_id: &str) -> Result<usize> {
    connection
        .execute(
            "
            UPDATE sessions
            SET share_state = 'unset'
            WHERE project_id = ?1 AND origin_kind = 'local'
            ",
            params![project_id],
        )
        .with_context(|| format!("failed to clear sharing state for project `{project_id}`"))
}

/// Reads one project sharing status summary.
pub fn query_share_status(connection: &Connection, project_id: &str) -> Result<ShareStatus> {
    let default_policy = project_share_policy(connection, project_id)?;
    let (
        local_session_count,
        shared_session_count,
        included_session_count,
        excluded_session_count,
        unset_session_count,
    ): (i64, i64, i64, i64, i64) = connection
        .query_row(
            "
            SELECT
                COALESCE(SUM(CASE WHEN origin_kind = 'local' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN origin_kind = 'shared' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN origin_kind = 'local' AND share_state = 'included' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN origin_kind = 'local' AND share_state = 'excluded' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN origin_kind = 'local' AND share_state = 'unset' THEN 1 ELSE 0 END), 0)
            FROM sessions
            WHERE project_id = ?1
            ",
            params![project_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .with_context(|| format!("failed to read sharing status for project `{project_id}`"))?;
    let selected_session_count = match default_policy {
        SharePolicy::Manual => included_session_count,
        SharePolicy::All => local_session_count - excluded_session_count,
    };
    Ok(ShareStatus {
        project_id: project_id.to_owned(),
        default_policy,
        local_session_count: sql_count_to_u64(local_session_count)?,
        shared_session_count: sql_count_to_u64(shared_session_count)?,
        selected_session_count: sql_count_to_u64(selected_session_count)?,
        included_session_count: sql_count_to_u64(included_session_count)?,
        excluded_session_count: sql_count_to_u64(excluded_session_count)?,
        unset_session_count: sql_count_to_u64(unset_session_count)?,
    })
}

/// Reads every selected local turn that should be exported for one project.
pub fn query_share_export_turns(
    connection: &Connection,
    project_id: &str,
) -> Result<Vec<ShareTurnExport>> {
    let mut statement = connection
        .prepare(
            "
            SELECT
                sessions.project_id,
                sessions.provider,
                sessions.session_id,
                sessions.parent_session_id,
                sessions.session_kind,
                sessions.archive_path,
                sessions.cwd,
                sessions.cli_version,
                sessions.schema_id,
                sessions.determinism,
                sessions.source_size,
                sessions.source_mtime_ms,
                turns.turn_ordinal,
                turns.turn_id,
                turns.started_at,
                turns.completed_at,
                turns.status,
                turns.user_message,
                turns.final_answer_at,
                turns.final_answer_text,
                turns.steps_json,
                turns.step_count,
                turns.tool_call_count,
                turns.tool_output_count,
                turns.attachment_count,
                turns.delegation_count,
                turns.hook_summary_count,
                turns.has_final_answer,
                turns.duration_ms,
                turns.effective_agent_runtime_ms,
                turns.provider_total_token_count,
                turns.input_uncached_token_count,
                turns.cache_read_token_count,
                turns.cache_write_token_count,
                turns.output_token_count,
                turns.reasoning_token_count,
                turns.total_token_count,
                turns.primary_model,
                turns.changed_file_count,
                turns.added_line_count,
                turns.removed_line_count
            FROM sessions
            JOIN turns
                ON turns.project_id = sessions.project_id
                AND turns.provider = sessions.provider
                AND turns.session_id = sessions.session_id
            LEFT JOIN project_share_policies
                ON project_share_policies.project_id = sessions.project_id
            WHERE sessions.project_id = ?1
                AND sessions.origin_kind = 'local'
                AND (
                    sessions.share_state = 'included'
                    OR (
                        COALESCE(project_share_policies.default_policy, 'manual') = 'all'
                        AND sessions.share_state <> 'excluded'
                    )
                )
            ORDER BY sessions.provider ASC, sessions.session_id ASC, turns.turn_ordinal ASC
            ",
        )
        .with_context(|| {
            format!("failed to prepare share export query for project `{project_id}`")
        })?;
    let rows = statement
        .query_map(params![project_id], read_share_turn_export_row)
        .with_context(|| format!("failed to query share export rows for project `{project_id}`"))?;
    rows.map(|row| row.context("failed to read share export row"))
        .collect()
}

/// Imports one shared turn and rebuilds derived analytics for that turn.
pub fn import_shared_turn(
    connection: &mut Connection,
    import: ShareTurnImport<'_>,
) -> Result<bool> {
    let transaction = connection
        .transaction()
        .context("failed to begin shared turn import transaction")?;
    upsert_share_user(&transaction, import.user)?;
    validate_shared_session_kind(&import.turn.session.session_kind)?;
    validate_shared_turn_status(&import.turn.status)?;
    if !upsert_shared_session(&transaction, import)? {
        transaction
            .commit()
            .context("failed to commit skipped shared turn import")?;
        return Ok(false);
    }
    replace_shared_turn(&transaction, import)?;
    transaction
        .commit()
        .context("failed to commit shared turn import")?;
    Ok(true)
}

/// Upserts one imported shared session unless a local session already owns the identity.
fn upsert_shared_session(connection: &Connection, import: ShareTurnImport<'_>) -> Result<bool> {
    let session = &import.turn.session;
    if let Some(origin_kind) = existing_session_origin(
        connection,
        import.project_id,
        session.provider,
        &session.session_id,
    )? && origin_kind == OriginKind::Local
    {
        return Ok(false);
    }
    let archive_path = shared_archive_path(import);
    connection
        .execute(
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
                source_mtime_ms,
                origin_kind,
                origin_user_id,
                origin_remote,
                imported_at,
                share_state
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'shared', ?13, ?14, ?15, 'unset')
            ON CONFLICT(project_id, provider, session_id) DO UPDATE SET
                parent_session_id = excluded.parent_session_id,
                session_kind = excluded.session_kind,
                archive_path = excluded.archive_path,
                cwd = excluded.cwd,
                cli_version = excluded.cli_version,
                schema_id = excluded.schema_id,
                determinism = excluded.determinism,
                source_size = excluded.source_size,
                source_mtime_ms = excluded.source_mtime_ms,
                origin_kind = excluded.origin_kind,
                origin_user_id = excluded.origin_user_id,
                origin_remote = excluded.origin_remote,
                imported_at = excluded.imported_at
            WHERE sessions.origin_kind = 'shared'
            ",
            params![
                import.project_id,
                session.provider.directory_name(),
                session.session_id,
                session.parent_session_id,
                session.session_kind,
                archive_path,
                session.cwd,
                session.cli_version,
                session.schema_id,
                session.determinism,
                session.source_size,
                session.source_mtime_ms,
                import.user.user_id,
                import.remote_name,
                import.imported_at,
            ],
        )
        .with_context(|| format!("failed to upsert shared session `{}`", session.session_id))?;
    Ok(true)
}

/// Builds a synthetic archive path for imported sessions to avoid local path uniqueness collisions.
fn shared_archive_path(import: ShareTurnImport<'_>) -> String {
    format!(
        "shared://{}/{}/{}/{}",
        import.remote_name,
        import.user.user_id,
        import.turn.session.provider.directory_name(),
        import.turn.session.session_id
    )
}

/// Replaces one imported shared turn and derived rows.
fn replace_shared_turn(connection: &Connection, import: ShareTurnImport<'_>) -> Result<()> {
    let turn = import.turn;
    let session = &turn.session;
    connection
        .execute(
            "
            DELETE FROM turns
            WHERE project_id = ?1
                AND provider = ?2
                AND session_id = ?3
                AND turn_ordinal = ?4
            ",
            params![
                import.project_id,
                session.provider.directory_name(),
                session.session_id,
                turn.turn_ordinal
            ],
        )
        .with_context(|| {
            format!(
                "failed to delete previous shared turn {} for session `{}`",
                turn.turn_ordinal, session.session_id
            )
        })?;
    connection
        .execute(
            INSERT_TURN_SQL,
            params![
                import.project_id,
                session.provider.directory_name(),
                session.session_id,
                turn.turn_ordinal,
                turn.turn_id,
                turn.started_at,
                turn.completed_at,
                turn.status,
                turn.user_message,
                turn.final_answer_at,
                turn.final_answer_text,
                turn.steps_json,
                turn.step_count,
                turn.tool_call_count,
                turn.tool_output_count,
                turn.attachment_count,
                turn.delegation_count,
                turn.hook_summary_count,
                turn.has_final_answer,
                turn.duration_ms,
                turn.effective_agent_runtime_ms,
                turn.provider_total_token_count,
                turn.input_uncached_token_count,
                turn.cache_read_token_count,
                turn.cache_write_token_count,
                turn.output_token_count,
                turn.reasoning_token_count,
                turn.total_token_count,
                turn.primary_model,
                turn.changed_file_count,
                turn.added_line_count,
                turn.removed_line_count,
            ],
        )
        .with_context(|| {
            format!(
                "failed to insert shared turn {} for session `{}`",
                turn.turn_ordinal, session.session_id
            )
        })?;
    let steps = serde_json::from_str::<Vec<NormalizedTurnStep>>(&turn.steps_json)
        .context("failed to parse shared turn steps_json")?;
    insert_turn_derived_records(
        connection,
        &TurnDerivedContext {
            project_id: import.project_id,
            provider: session.provider,
            session_id: &session.session_id,
            turn_ordinal: turn.turn_ordinal,
            session_cwd: Some(&session.cwd),
            user_message: &turn.user_message,
            final_answer_text: turn.final_answer_text.as_deref(),
        },
        &steps,
    )
    .context("failed to rebuild derived analytics for shared turn")?;
    Ok(())
}

/// Reads the origin kind for one existing session identity.
fn existing_session_origin(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
) -> Result<Option<OriginKind>> {
    let value = connection
        .query_row(
            "
            SELECT origin_kind
            FROM sessions
            WHERE project_id = ?1 AND provider = ?2 AND session_id = ?3
            ",
            params![project_id, provider.directory_name(), session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .with_context(|| format!("failed to read origin for session `{session_id}`"))?;
    value.as_deref().map(parse_origin_kind).transpose()
}

/// Deletes imported shared turns missing from the latest authenticated sync payload.
pub fn prune_shared_turns(
    connection: &Connection,
    project_id: &str,
    origin_remote: &str,
    origin_user_id: &str,
    keep_turns: &BTreeSet<(SourceKind, String, i64)>,
) -> Result<usize> {
    let mut statement = connection
        .prepare(
            "
            SELECT turns.provider, turns.session_id, turns.turn_ordinal
            FROM turns
            JOIN sessions
                ON sessions.project_id = turns.project_id
                AND sessions.provider = turns.provider
                AND sessions.session_id = turns.session_id
            WHERE sessions.project_id = ?1
                AND sessions.origin_kind = 'shared'
                AND sessions.origin_remote = ?2
                AND sessions.origin_user_id = ?3
            ",
        )
        .with_context(|| {
            format!("failed to prepare shared turn prune query for project `{project_id}`")
        })?;
    let rows = statement
        .query_map(params![project_id, origin_remote, origin_user_id], |row| {
            let provider_text: String = row.get(0)?;
            let provider = match provider_text.as_str() {
                "claude" => SourceKind::Claude,
                "codex" => SourceKind::Codex,
                _ => {
                    return Err(rusqlite::Error::InvalidColumnType(
                        0,
                        "provider".to_owned(),
                        rusqlite::types::Type::Text,
                    ));
                }
            };
            Ok((provider, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
        })
        .with_context(|| {
            format!("failed to query shared turns to prune for project `{project_id}`")
        })?;
    let mut prune_targets = Vec::new();
    for row in rows {
        let turn_key = row.context("failed to read shared turn prune row")?;
        if !keep_turns.contains(&turn_key) {
            prune_targets.push(turn_key);
        }
    }

    let mut pruned = 0usize;
    for (provider, session_id, turn_ordinal) in prune_targets {
        pruned += connection
            .execute(
                "
                DELETE FROM turns
                WHERE project_id = ?1
                    AND provider = ?2
                    AND session_id = ?3
                    AND turn_ordinal = ?4
                ",
                params![
                    project_id,
                    provider.directory_name(),
                    session_id,
                    turn_ordinal,
                ],
            )
            .with_context(|| {
                format!("failed to prune shared turn {turn_ordinal} for session `{session_id}`")
            })?;
    }
    connection
        .execute(
            "
            DELETE FROM sessions
            WHERE project_id = ?1
                AND origin_kind = 'shared'
                AND origin_remote = ?2
                AND origin_user_id = ?3
                AND NOT EXISTS (
                    SELECT 1
                    FROM turns
                    WHERE turns.project_id = sessions.project_id
                        AND turns.provider = sessions.provider
                        AND turns.session_id = sessions.session_id
                )
            ",
            params![project_id, origin_remote, origin_user_id],
        )
        .with_context(|| {
            format!("failed to prune empty shared sessions for project `{project_id}`")
        })?;
    Ok(pruned)
}

/// Reads one selected export row from SQLite.
fn read_share_turn_export_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ShareTurnExport> {
    let provider_text: String = row.get(1)?;
    let provider = match provider_text.as_str() {
        "claude" => SourceKind::Claude,
        "codex" => SourceKind::Codex,
        _ => {
            return Err(rusqlite::Error::InvalidColumnType(
                1,
                "provider".to_owned(),
                rusqlite::types::Type::Text,
            ));
        }
    };
    Ok(ShareTurnExport {
        session: ShareSessionExport {
            project_id: row.get(0)?,
            provider,
            session_id: row.get(2)?,
            parent_session_id: row.get(3)?,
            session_kind: row.get(4)?,
            archive_path: row.get(5)?,
            cwd: row.get(6)?,
            cli_version: row.get(7)?,
            schema_id: row.get(8)?,
            determinism: row.get(9)?,
            source_size: row.get(10)?,
            source_mtime_ms: row.get(11)?,
        },
        turn_ordinal: row.get(12)?,
        turn_id: row.get(13)?,
        started_at: row.get(14)?,
        completed_at: row.get(15)?,
        status: row.get(16)?,
        user_message: row.get(17)?,
        final_answer_at: row.get(18)?,
        final_answer_text: row.get(19)?,
        steps_json: row.get(20)?,
        step_count: row.get(21)?,
        tool_call_count: row.get(22)?,
        tool_output_count: row.get(23)?,
        attachment_count: row.get(24)?,
        delegation_count: row.get(25)?,
        hook_summary_count: row.get(26)?,
        has_final_answer: row.get(27)?,
        duration_ms: row.get(28)?,
        effective_agent_runtime_ms: row.get(29)?,
        provider_total_token_count: row.get(30)?,
        input_uncached_token_count: row.get(31)?,
        cache_read_token_count: row.get(32)?,
        cache_write_token_count: row.get(33)?,
        output_token_count: row.get(34)?,
        reasoning_token_count: row.get(35)?,
        total_token_count: row.get(36)?,
        primary_model: row.get(37)?,
        changed_file_count: row.get(38)?,
        added_line_count: row.get(39)?,
        removed_line_count: row.get(40)?,
    })
}

/// Converts one SQL count into an unsigned count.
fn sql_count_to_u64(value: i64) -> Result<u64> {
    u64::try_from(value).context("negative count encountered in SQLite query")
}

/// Returns whether one session kind string is supported by stored normalized sessions.
pub fn validate_shared_session_kind(value: &str) -> Result<()> {
    match value {
        "primary" | "subagent" => Ok(()),
        other => bail!("unsupported shared session kind `{other}`"),
    }
}

/// Returns whether one status string is supported by stored normalized turns.
pub fn validate_shared_turn_status(value: &str) -> Result<NormalizedTurnStatus> {
    match value {
        "completed" => Ok(NormalizedTurnStatus::Completed),
        "aborted" => Ok(NormalizedTurnStatus::Aborted),
        "incomplete" => Ok(NormalizedTurnStatus::Incomplete),
        other => bail!("unsupported shared turn status `{other}`"),
    }
}
