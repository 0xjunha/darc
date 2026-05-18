use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use darc_paths::SourceKind;
use darc_rollout::model::{NormalizedTurnStatus, NormalizedTurnStep};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{
    derived_data::{TurnDerivedContext, insert_turn_derived_records},
    index_db::schema::INSERT_TURN_SQL,
    redaction::{redact_normalized_steps, redact_text},
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

/// Stores provenance for one existing indexed session row.
struct ExistingSessionOrigin {
    origin_kind: OriginKind,
    origin_user_id: Option<String>,
    origin_remote: Option<String>,
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

/// Stores lightweight selected-session state used to detect unchanged share exports.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ShareSessionExportState {
    pub provider: SourceKind,
    pub session_id: String,
    pub source_size: Option<i64>,
    pub source_mtime_ms: Option<i64>,
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

/// Reads selected local sessions that have exportable turns for one project.
pub fn query_share_export_session_states(
    connection: &Connection,
    project_id: &str,
) -> Result<Vec<ShareSessionExportState>> {
    let mut statement = connection
        .prepare(
            "
            SELECT
                sessions.provider,
                sessions.session_id,
                sessions.source_size,
                sessions.source_mtime_ms
            FROM sessions
            LEFT JOIN project_share_policies
                ON project_share_policies.project_id = sessions.project_id
            WHERE sessions.project_id = ?1
                AND sessions.origin_kind = 'local'
                AND EXISTS (
                    SELECT 1
                    FROM turns
                    WHERE turns.project_id = sessions.project_id
                        AND turns.provider = sessions.provider
                        AND turns.session_id = sessions.session_id
                )
                AND (
                    sessions.share_state = 'included'
                    OR (
                        COALESCE(project_share_policies.default_policy, 'manual') = 'all'
                        AND sessions.share_state <> 'excluded'
                    )
                )
            ORDER BY sessions.provider ASC, sessions.session_id ASC
            ",
        )
        .with_context(|| {
            format!("failed to prepare share export session query for project `{project_id}`")
        })?;
    let rows = statement
        .query_map(params![project_id], read_share_session_export_state_row)
        .with_context(|| {
            format!("failed to query share export sessions for project `{project_id}`")
        })?;
    let mut sessions = rows
        .map(|row| row.context("failed to read share export session row"))
        .collect::<Result<Vec<_>>>()?;
    sessions.retain(|session| validate_shared_session_id(&session.session_id).is_ok());
    Ok(sessions)
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
    let mut turns = rows
        .map(|row| row.context("failed to read share export row"))
        .collect::<Result<Vec<_>>>()?;
    turns.retain(|turn| validate_shared_session_id(&turn.session.session_id).is_ok());
    for turn in &mut turns {
        redact_share_turn_export(turn)?;
    }
    Ok(turns)
}

/// Redacts one selected share export row before it crosses the share boundary.
fn redact_share_turn_export(turn: &mut ShareTurnExport) -> Result<()> {
    turn.session.archive_path = redact_text(&turn.session.archive_path);
    turn.session.cwd = redact_text(&turn.session.cwd);
    turn.user_message = redact_text(&turn.user_message);
    if let Some(final_answer_text) = &mut turn.final_answer_text {
        *final_answer_text = redact_text(final_answer_text);
    }
    let mut steps = serde_json::from_str::<Vec<NormalizedTurnStep>>(&turn.steps_json)
        .context("failed to parse share export steps_json for redaction")?;
    redact_normalized_steps(&mut steps);
    turn.steps_json =
        serde_json::to_string(&steps).context("failed to serialize redacted share export steps")?;
    Ok(())
}

/// Imports one shared turn and rebuilds derived analytics for that turn.
pub fn import_shared_turn(
    connection: &mut Connection,
    import: ShareTurnImport<'_>,
) -> Result<bool> {
    let mut outcomes = import_shared_turns(connection, &[import])?;
    outcomes
        .pop()
        .context("shared turn import produced no outcome")?
}

/// Imports shared turns in one transaction while isolating per-turn failures.
pub fn import_shared_turns(
    connection: &mut Connection,
    imports: &[ShareTurnImport<'_>],
) -> Result<Vec<Result<bool>>> {
    let mut transaction = connection
        .transaction()
        .context("failed to begin shared turn import transaction")?;
    let mut outcomes = Vec::with_capacity(imports.len());
    for import in imports {
        let savepoint = transaction
            .savepoint()
            .context("failed to begin shared turn import savepoint")?;
        match import_shared_turn_in_connection(&savepoint, *import) {
            Ok(imported) => {
                savepoint
                    .commit()
                    .context("failed to commit shared turn import savepoint")?;
                outcomes.push(Ok(imported));
            }
            Err(error) => outcomes.push(Err(error)),
        }
    }
    transaction
        .commit()
        .context("failed to commit shared turn import transaction")?;
    Ok(outcomes)
}

/// Imports one shared turn using the caller's transaction or savepoint.
fn import_shared_turn_in_connection(
    connection: &Connection,
    import: ShareTurnImport<'_>,
) -> Result<bool> {
    upsert_share_user(connection, import.user)?;
    validate_shared_session_id(&import.turn.session.session_id)?;
    validate_shared_session_kind(&import.turn.session.session_kind)?;
    validate_shared_turn_status(&import.turn.status)?;
    validate_shared_turn_numbers(import.turn)?;
    if !upsert_shared_session(connection, import)? {
        return Ok(false);
    }
    replace_shared_turn(connection, import)?;
    Ok(true)
}

/// Validates imported shared numeric fields before they enter canonical tables.
fn validate_shared_turn_numbers(turn: &ShareTurnExport) -> Result<()> {
    validate_non_negative("turn_ordinal", turn.turn_ordinal)?;
    validate_optional_non_negative("session.source_size", turn.session.source_size)?;
    validate_optional_non_negative("session.source_mtime_ms", turn.session.source_mtime_ms)?;
    validate_non_negative("step_count", turn.step_count)?;
    validate_non_negative("tool_call_count", turn.tool_call_count)?;
    validate_non_negative("tool_output_count", turn.tool_output_count)?;
    validate_non_negative("attachment_count", turn.attachment_count)?;
    validate_non_negative("delegation_count", turn.delegation_count)?;
    validate_non_negative("hook_summary_count", turn.hook_summary_count)?;
    if !matches!(turn.has_final_answer, 0 | 1) {
        bail!("shared turn has_final_answer must be 0 or 1");
    }
    validate_optional_non_negative("duration_ms", turn.duration_ms)?;
    validate_optional_non_negative(
        "effective_agent_runtime_ms",
        turn.effective_agent_runtime_ms,
    )?;
    validate_optional_non_negative(
        "provider_total_token_count",
        turn.provider_total_token_count,
    )?;
    validate_optional_non_negative(
        "input_uncached_token_count",
        turn.input_uncached_token_count,
    )?;
    validate_optional_non_negative("cache_read_token_count", turn.cache_read_token_count)?;
    validate_optional_non_negative("cache_write_token_count", turn.cache_write_token_count)?;
    validate_optional_non_negative("output_token_count", turn.output_token_count)?;
    validate_optional_non_negative("reasoning_token_count", turn.reasoning_token_count)?;
    validate_optional_non_negative("total_token_count", turn.total_token_count)?;
    validate_non_negative("changed_file_count", turn.changed_file_count)?;
    validate_non_negative("added_line_count", turn.added_line_count)?;
    validate_non_negative("removed_line_count", turn.removed_line_count)?;
    Ok(())
}

/// Validates one required non-negative shared integer.
fn validate_non_negative(field: &str, value: i64) -> Result<()> {
    if value < 0 {
        bail!("shared turn {field} must be non-negative");
    }
    Ok(())
}

/// Validates one optional non-negative shared integer.
fn validate_optional_non_negative(field: &str, value: Option<i64>) -> Result<()> {
    if let Some(value) = value {
        validate_non_negative(field, value)?;
    }
    Ok(())
}

/// Upserts one imported shared session unless a local session already owns the identity.
fn upsert_shared_session(connection: &Connection, import: ShareTurnImport<'_>) -> Result<bool> {
    let session = &import.turn.session;
    if let Some(existing) = existing_session_origin(
        connection,
        import.project_id,
        session.provider,
        &session.session_id,
    )? {
        if existing.origin_kind == OriginKind::Local {
            return Ok(false);
        }
        if existing.origin_user_id.as_deref() != Some(import.user.user_id.as_str())
            || existing.origin_remote.as_deref() != Some(import.remote_name)
        {
            bail!(
                "shared session `{}` is already owned by another exporter",
                session.session_id
            );
        }
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
) -> Result<Option<ExistingSessionOrigin>> {
    let value = connection
        .query_row(
            "
            SELECT origin_kind, origin_user_id, origin_remote
            FROM sessions
            WHERE project_id = ?1 AND provider = ?2 AND session_id = ?3
            ",
            params![project_id, provider.directory_name(), session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .with_context(|| format!("failed to read origin for session `{session_id}`"))?;
    value
        .map(|(origin_kind, origin_user_id, origin_remote)| {
            Ok(ExistingSessionOrigin {
                origin_kind: parse_origin_kind(&origin_kind)?,
                origin_user_id,
                origin_remote,
            })
        })
        .transpose()
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

/// Reads one selected export session state row from SQLite.
fn read_share_session_export_state_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<ShareSessionExportState> {
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
    Ok(ShareSessionExportState {
        provider,
        session_id: row.get(1)?,
        source_size: row.get(2)?,
        source_mtime_ms: row.get(3)?,
    })
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

/// Validates that an imported shared session id is addressable by query commands.
pub fn validate_shared_session_id(value: &str) -> Result<()> {
    if value.len() != 36 {
        bail!("shared session id must be a canonical UUID");
    }
    for (index, ch) in value.chars().enumerate() {
        match index {
            8 | 13 | 18 | 23 if ch == '-' => {}
            8 | 13 | 18 | 23 => bail!("shared session id must be a canonical UUID"),
            _ if ch.is_ascii_hexdigit() => {}
            _ => bail!("shared session id must be a canonical UUID"),
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::index_db::open_index_database_writer;
    use crate::test_support::{
        IndexedSessionFixture, IndexedTurnFixture, insert_indexed_session, insert_indexed_turn,
    };

    /// Builds one unique temporary database path for sharing tests.
    fn test_db_path(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "darc-store-{prefix}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("failed to create test directory");
        dir.join("index.sqlite")
    }

    /// Builds one valid synthetic shared turn export for validation tests.
    fn synthetic_share_turn() -> ShareTurnExport {
        ShareTurnExport {
            session: ShareSessionExport {
                project_id: "source-repo".to_owned(),
                provider: SourceKind::Codex,
                session_id: "00000000-0000-4000-8000-000000000001".to_owned(),
                parent_session_id: None,
                session_kind: "primary".to_owned(),
                archive_path: "synthetic.jsonl".to_owned(),
                cwd: "/tmp/synthetic-repo".to_owned(),
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
            user_message: "synthetic prompt".to_owned(),
            final_answer_at: Some("2026-05-15T00:00:01Z".to_owned()),
            final_answer_text: Some("synthetic answer".to_owned()),
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
    fn rejects_negative_shared_import_numbers() {
        let mut turn = synthetic_share_turn();
        turn.step_count = -1;
        assert!(validate_shared_turn_numbers(&turn).is_err());

        let mut turn = synthetic_share_turn();
        turn.turn_ordinal = -1;
        assert!(validate_shared_turn_numbers(&turn).is_err());

        let mut turn = synthetic_share_turn();
        turn.total_token_count = Some(-1);
        assert!(validate_shared_turn_numbers(&turn).is_err());
    }

    #[test]
    fn rejects_non_boolean_shared_final_answer_flag() {
        let mut turn = synthetic_share_turn();
        turn.has_final_answer = 2;

        assert!(validate_shared_turn_numbers(&turn).is_err());
    }

    #[test]
    fn shared_import_rejects_unaddressable_session_ids() -> Result<()> {
        let mut connection = open_index_database_writer(&test_db_path("invalid-session-id"))?;
        let user = ShareUserRecord {
            user_id: "usr-remote".to_owned(),
            display_name: Some("Remote User".to_owned()),
            email: Some("remote@example.invalid".to_owned()),
            public_key: Some("age1remote".to_owned()),
            source: "test".to_owned(),
            updated_at: "2026-05-15T00:00:00Z".to_owned(),
        };
        let mut turn = synthetic_share_turn();
        turn.session.session_id = "not-a-uuid".to_owned();

        let error = import_shared_turn(
            &mut connection,
            ShareTurnImport {
                project_id: "target-repo",
                user: &user,
                remote_name: "origin:darc/team",
                imported_at: "2026-05-15T00:00:00Z",
                turn: &turn,
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("canonical UUID"));

        Ok(())
    }

    #[test]
    fn share_export_redacts_stale_unredacted_rows() -> Result<()> {
        let connection = open_index_database_writer(&test_db_path("export-redacts-stale"))?;
        let session_id = "00000000-0000-4000-8000-000000000901";
        insert_indexed_session(
            &connection,
            IndexedSessionFixture::new("repo", SourceKind::Codex, session_id, "/tmp/repo"),
        )?;
        insert_indexed_turn(
            &connection,
            IndexedTurnFixture {
                has_final_answer: true,
                final_answer_text: Some("benign answer"),
                ..IndexedTurnFixture::new(
                    "repo",
                    SourceKind::Codex,
                    session_id,
                    1,
                    "2026-05-15T00:00:00Z",
                    "completed",
                    "[]",
                )
            },
        )?;
        connection.execute(
            "
            UPDATE sessions
            SET archive_path = ?1, cwd = ?2
            WHERE project_id = 'repo' AND session_id = ?3
            ",
            params![
                "/Users/alice/.codex/sessions/secret.jsonl",
                "/Users/alice/private-repo",
                session_id,
            ],
        )?;
        let unredacted_steps = r#"[{"type":"tool_call","timestamp":"2026-05-15T00:00:00Z","call_id":"call-secret","name":"Read","arguments":"{\"api_key\":\"sk-proj-abcdefghijklmnop\",\"file_path\":\"/Users/alice/.env\"}"},{"type":"tool_call_output","timestamp":"2026-05-15T00:00:01Z","call_id":"call-secret","output":"token=ghp_abcdefghijklmnopqrstuvwxyz123456"}]"#;
        connection.execute(
            "
            UPDATE turns
            SET user_message = ?1, final_answer_text = ?2, steps_json = ?3
            WHERE project_id = 'repo' AND session_id = ?4
            ",
            params![
                "Use https://user:pass@example.invalid/repo.git and TOKEN=secretvalue",
                "Final answer includes ghp_abcdefghijklmnopqrstuvwxyz123456",
                unredacted_steps,
                session_id,
            ],
        )?;
        set_project_share_policy(
            &connection,
            "repo",
            SharePolicy::All,
            "2026-05-15T00:00:00Z",
        )?;

        let turns = query_share_export_turns(&connection, "repo")?;

        assert_eq!(turns.len(), 1);
        let exported = &turns[0];
        let serialized = serde_json::to_string(exported)?;
        for secret in [
            "alice",
            "user:pass",
            "secretvalue",
            "sk-proj-abcdefghijklmnop",
            "ghp_abcdefghijklmnopqrstuvwxyz123456",
        ] {
            assert!(
                !serialized.contains(secret),
                "share export should redact {secret}: {serialized}"
            );
        }
        assert!(serialized.contains("[REDACTED"));

        Ok(())
    }

    #[test]
    fn share_export_skips_unaddressable_session_ids() -> Result<()> {
        let connection = open_index_database_writer(&test_db_path("export-skips-invalid-id"))?;
        let valid_session_id = "00000000-0000-4000-8000-000000000902";
        let invalid_session_id = "00000000-0000-4000-8000-000000000903/subagents/000000000904";
        for (session_id, message) in [
            (valid_session_id, "valid prompt"),
            (invalid_session_id, "invalid prompt"),
        ] {
            insert_indexed_session(
                &connection,
                IndexedSessionFixture::new("repo", SourceKind::Claude, session_id, "/tmp/repo"),
            )?;
            insert_indexed_turn(
                &connection,
                IndexedTurnFixture {
                    user_message: message,
                    ..IndexedTurnFixture::new(
                        "repo",
                        SourceKind::Claude,
                        session_id,
                        1,
                        "2026-05-15T00:00:00Z",
                        "completed",
                        "[]",
                    )
                },
            )?;
        }
        set_project_share_policy(
            &connection,
            "repo",
            SharePolicy::All,
            "2026-05-15T00:00:00Z",
        )?;

        let turns = query_share_export_turns(&connection, "repo")?;

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].session.session_id, valid_session_id);
        assert_eq!(turns[0].user_message, "valid prompt");
        Ok(())
    }

    #[test]
    fn shared_import_rejects_existing_session_from_other_exporter() -> Result<()> {
        let mut connection = open_index_database_writer(&test_db_path("owner-collision"))?;
        let first_user = ShareUserRecord {
            user_id: "usr-first".to_owned(),
            display_name: Some("First User".to_owned()),
            email: Some("first@example.invalid".to_owned()),
            public_key: Some("age1first".to_owned()),
            source: "test".to_owned(),
            updated_at: "2026-05-15T00:00:00Z".to_owned(),
        };
        let second_user = ShareUserRecord {
            user_id: "usr-second".to_owned(),
            display_name: Some("Second User".to_owned()),
            email: Some("second@example.invalid".to_owned()),
            public_key: Some("age1second".to_owned()),
            source: "test".to_owned(),
            updated_at: "2026-05-15T00:00:00Z".to_owned(),
        };
        let turn = synthetic_share_turn();
        assert!(import_shared_turn(
            &mut connection,
            ShareTurnImport {
                project_id: "target-repo",
                user: &first_user,
                remote_name: "origin:darc/team",
                imported_at: "2026-05-15T00:00:00Z",
                turn: &turn,
            },
        )?);

        let mut colliding_turn = synthetic_share_turn();
        colliding_turn.user_message = "malicious replacement".to_owned();
        let error = import_shared_turn(
            &mut connection,
            ShareTurnImport {
                project_id: "target-repo",
                user: &second_user,
                remote_name: "origin:darc/team",
                imported_at: "2026-05-15T00:01:00Z",
                turn: &colliding_turn,
            },
        )
        .unwrap_err();

        let existing: (String, String) = connection.query_row(
            "
            SELECT sessions.origin_user_id, turns.user_message
            FROM sessions
            JOIN turns
                ON turns.project_id = sessions.project_id
                AND turns.provider = sessions.provider
                AND turns.session_id = sessions.session_id
            WHERE sessions.project_id = 'target-repo'
                AND sessions.provider = 'codex'
                AND sessions.session_id = ?1
            ",
            [&turn.session.session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        assert!(
            error.to_string().contains("already owned"),
            "error should explain owner collision: {error:#}"
        );
        assert_eq!(existing.0, "usr-first");
        assert_eq!(existing.1, "synthetic prompt");
        Ok(())
    }

    #[test]
    fn shared_import_skips_existing_local_session() -> Result<()> {
        let mut connection = open_index_database_writer(&test_db_path("local-owner-collision"))?;
        let user = ShareUserRecord {
            user_id: "usr-remote".to_owned(),
            display_name: Some("Remote User".to_owned()),
            email: Some("remote@example.invalid".to_owned()),
            public_key: Some("age1remote".to_owned()),
            source: "test".to_owned(),
            updated_at: "2026-05-15T00:00:00Z".to_owned(),
        };
        let mut remote_turn = synthetic_share_turn();
        remote_turn.user_message = "remote prompt".to_owned();
        let local_session = &remote_turn.session;
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
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ",
            params![
                "target-repo",
                local_session.provider.directory_name(),
                local_session.session_id,
                local_session.parent_session_id,
                local_session.session_kind,
                "local.jsonl",
                "/tmp/local-repo",
                local_session.cli_version,
                local_session.schema_id,
                local_session.determinism,
                local_session.source_size,
                local_session.source_mtime_ms,
            ],
        )?;
        connection.execute(
            INSERT_TURN_SQL,
            params![
                "target-repo",
                local_session.provider.directory_name(),
                local_session.session_id,
                remote_turn.turn_ordinal,
                remote_turn.turn_id,
                remote_turn.started_at,
                remote_turn.completed_at,
                remote_turn.status,
                "local prompt",
                remote_turn.final_answer_at,
                remote_turn.final_answer_text,
                remote_turn.steps_json,
                remote_turn.step_count,
                remote_turn.tool_call_count,
                remote_turn.tool_output_count,
                remote_turn.attachment_count,
                remote_turn.delegation_count,
                remote_turn.hook_summary_count,
                remote_turn.has_final_answer,
                remote_turn.duration_ms,
                remote_turn.effective_agent_runtime_ms,
                remote_turn.provider_total_token_count,
                remote_turn.input_uncached_token_count,
                remote_turn.cache_read_token_count,
                remote_turn.cache_write_token_count,
                remote_turn.output_token_count,
                remote_turn.reasoning_token_count,
                remote_turn.total_token_count,
                remote_turn.primary_model,
                remote_turn.changed_file_count,
                remote_turn.added_line_count,
                remote_turn.removed_line_count,
            ],
        )?;

        let imported = import_shared_turn(
            &mut connection,
            ShareTurnImport {
                project_id: "target-repo",
                user: &user,
                remote_name: "origin:darc/team",
                imported_at: "2026-05-15T00:01:00Z",
                turn: &remote_turn,
            },
        )?;

        let existing: (String, Option<String>, String) = connection.query_row(
            "
            SELECT sessions.origin_kind, sessions.origin_user_id, turns.user_message
            FROM sessions
            JOIN turns
                ON turns.project_id = sessions.project_id
                AND turns.provider = sessions.provider
                AND turns.session_id = sessions.session_id
            WHERE sessions.project_id = 'target-repo'
                AND sessions.provider = 'codex'
                AND sessions.session_id = ?1
            ",
            [&local_session.session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

        assert!(!imported);
        assert_eq!(existing.0, "local");
        assert_eq!(existing.1, None);
        assert_eq!(existing.2, "local prompt");
        Ok(())
    }
}
