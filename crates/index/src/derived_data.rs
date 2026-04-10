use anyhow::{Context, Result};
use darc_paths::SourceKind;
use darc_rollout::model::NormalizedTurnStep;
use rusqlite::{Connection, params};

use crate::{
    index_db::schema::{
        DELETE_DERIVED_ANALYTICS_SQL, INSERT_FILE_ACCESS_SQL, INSERT_TOOL_CALL_SQL,
        INSERT_TURN_SEARCH_SQL, SELECT_DERIVED_ANALYTICS_REBUILD_ROWS_SQL,
    },
    policy::{build_turn_search_text, derive_file_access_records, extract_tool_call_records},
};

/// Stores the canonical turn identity and text needed to derive search analytics rows.
pub(crate) struct TurnDerivedContext<'a> {
    pub(crate) project_id: &'a str,
    pub(crate) provider: SourceKind,
    pub(crate) session_id: &'a str,
    pub(crate) turn_ordinal: i64,
    pub(crate) user_message: &'a str,
    pub(crate) final_answer_text: Option<&'a str>,
}

/// Inserts one turn's derived tool-call and file-access records into SQLite.
pub(crate) fn insert_turn_derived_records(
    connection: &Connection,
    context: &TurnDerivedContext<'_>,
    steps: &[NormalizedTurnStep],
) -> Result<()> {
    let project_id = context.project_id;
    let provider = context.provider;
    let session_id = context.session_id;
    let user_message = context.user_message;
    let final_answer_text = context.final_answer_text;
    let turn_ordinal = context.turn_ordinal;
    let turn_ordinal = u64::try_from(turn_ordinal)
        .context("turn ordinal is negative while inserting analytics")?;
    let tool_calls =
        extract_tool_call_records(project_id, provider, session_id, turn_ordinal, steps);
    let file_accesses = derive_file_access_records(&tool_calls);

    let mut tool_call_statement = connection
        .prepare(INSERT_TOOL_CALL_SQL)
        .context("failed to prepare tool_call insert statement")?;
    for record in &tool_calls {
        tool_call_statement
            .execute(params![
                record.project_id.as_str(),
                record.provider.directory_name(),
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
            ])
            .with_context(|| {
                format!(
                    "failed to insert derived tool call {} for {}/{session_id}#{turn_ordinal}",
                    record.call_id,
                    provider.directory_name(),
                )
            })?;
    }
    drop(tool_call_statement);

    let mut file_access_statement = connection
        .prepare(INSERT_FILE_ACCESS_SQL)
        .context("failed to prepare file_access insert statement")?;
    for record in &file_accesses {
        file_access_statement
            .execute(params![
                record.project_id.as_str(),
                record.provider.directory_name(),
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
                record.file_name.as_deref(),
            ])
            .with_context(|| {
                format!(
                    "failed to insert derived file access {} for {}/{session_id}#{turn_ordinal}",
                    record.path,
                    provider.directory_name(),
                )
            })?;
    }

    let tool_text = build_turn_search_text(steps);
    connection
        .execute(
            INSERT_TURN_SEARCH_SQL,
            params![
                project_id,
                provider.directory_name(),
                session_id,
                i64::try_from(turn_ordinal).context("turn ordinal exceeds SQLite INTEGER range")?,
                user_message,
                final_answer_text.unwrap_or_default(),
                tool_text,
            ],
        )
        .with_context(|| {
            format!(
                "failed to insert derived turn search row for {}/{session_id}#{turn_ordinal}",
                provider.directory_name(),
            )
        })?;

    Ok(())
}

/// Rebuilds every derived tool-call and file-access row from stored turn steps.
pub(crate) fn rebuild_derived_analytics_tables(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(DELETE_DERIVED_ANALYTICS_SQL)
        .context("failed to clear derived analytics tables")?;

    let mut statement = connection
        .prepare(SELECT_DERIVED_ANALYTICS_REBUILD_ROWS_SQL)
        .context("failed to prepare derived analytics rebuild query")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })
        .context("failed to query turns for derived analytics rebuild")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read derived analytics rebuild rows")?;
    drop(statement);

    for (project_id, provider, session_id, turn_ordinal, steps_json, user_message, final_answer) in
        rows
    {
        let provider = parse_provider(&provider)?;
        let steps = serde_json::from_str::<Vec<NormalizedTurnStep>>(&steps_json).with_context(|| {
            format!(
                "failed to parse stored steps_json while rebuilding analytics for {provider:?}/{session_id}#{turn_ordinal}"
            )
        })?;
        insert_turn_derived_records(
            connection,
            &TurnDerivedContext {
                project_id: &project_id,
                provider,
                session_id: &session_id,
                turn_ordinal,
                user_message: &user_message,
                final_answer_text: final_answer.as_deref(),
            },
            &steps,
        )?;
    }

    Ok(())
}
/// Parses one persisted lowercase SQLite provider value back into a source kind.
fn parse_provider(value: &str) -> Result<SourceKind> {
    match value {
        "claude" => Ok(SourceKind::Claude),
        "codex" => Ok(SourceKind::Codex),
        other => anyhow::bail!("unsupported provider `{other}` in index"),
    }
}
