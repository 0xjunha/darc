use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, TransactionBehavior};

use super::{
    DEFAULT_SESSION_BUNDLE_FILE_LIMIT, SessionBundleQueryData, SessionBundleQueryRequest,
    SessionBundleView, SessionFilesQueryRequest, SessionsView, TurnDetailOptions,
    open_existing_index_database,
};
use crate::query::{
    files::build_session_files_query,
    projects::compact_session_summary,
    projects::query_session_summary,
    turns::{TurnDetailQueryScope, build_session_turn_details_page},
};

/// Queries one composite session bundle from indexed session, turn, and file summaries.
pub fn query_project_session_bundle(
    index_db_path: &Path,
    request: SessionBundleQueryRequest<'_>,
) -> Result<SessionBundleQueryData> {
    let mut connection = open_existing_index_database(index_db_path)?;
    // Keep session summary, turn detail, and file summaries on one SQLite snapshot.
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .context("failed to open read transaction for session bundle query")?;
    let bundle = build_session_bundle_query(&transaction, request)?;
    transaction
        .commit()
        .context("failed to commit read transaction for session bundle query")?;
    Ok(bundle)
}

/// Builds one composite session bundle on top of the existing read-only query primitives.
fn build_session_bundle_query(
    connection: &Connection,
    request: SessionBundleQueryRequest<'_>,
) -> Result<SessionBundleQueryData> {
    let session = query_session_summary(
        connection,
        request.project_id,
        request.provider,
        request.session_id,
        request.project_root,
    )?;
    let session = match request.session_view {
        SessionsView::Compact => compact_session_summary(session),
        SessionsView::Full => session,
    };
    let (turns, turns_has_more) = build_session_turn_details_page(
        connection,
        TurnDetailQueryScope {
            project_id: request.project_id,
            project_root: request.project_root,
            provider: request.provider,
            session_id: request.session_id,
        },
        TurnDetailOptions {
            include_raw: false,
            include_insights: false,
            narrative: matches!(request.view, SessionBundleView::Narrative),
            step_limit: request.step_limit,
            step_offset: request.step_offset,
        },
        request.turn_limit,
        request.turn_offset,
    )?;
    let session_files = build_session_files_query(
        connection,
        SessionFilesQueryRequest {
            project_id: request.project_id,
            project_root: request.project_root,
            provider: request.provider,
            session_id: request.session_id,
            limit: DEFAULT_SESSION_BUNDLE_FILE_LIMIT,
            offset: 0,
        },
    )?;
    Ok(SessionBundleQueryData {
        project_id: request.project_id.to_owned(),
        provider: request.provider,
        session_id: request.session_id.to_owned(),
        session_view: request.session_view,
        view: request.view,
        turn_limit: u64::try_from(request.turn_limit).context("query limit exceeds u64 range")?,
        turn_offset: u64::try_from(request.turn_offset)
            .context("query offset exceeds u64 range")?,
        turns_has_more,
        session_file_limit: u64::try_from(DEFAULT_SESSION_BUNDLE_FILE_LIMIT)
            .context("query limit exceeds u64 range")?,
        session_file_count: session_files.file_count,
        session_files_has_more: session_files.has_more,
        step_limit: u64::try_from(request.step_limit).context("query limit exceeds u64 range")?,
        step_offset: u64::try_from(request.step_offset)
            .context("query offset exceeds u64 range")?,
        session,
        turns,
        session_files,
    })
}
