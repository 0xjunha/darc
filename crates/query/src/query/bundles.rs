use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, TransactionBehavior};

use super::{
    SessionBundleQueryData, SessionBundleQueryRequest, SessionBundleView, SessionsView,
    TurnDetailOptions, open_existing_index_database,
};
use crate::query::{
    files::build_session_files_query, projects::compact_session_summary,
    projects::query_session_summary, turns::build_session_turn_details_page,
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
    )?;
    let session = match request.session_view {
        SessionsView::Compact => compact_session_summary(session),
        SessionsView::Full => session,
    };
    let (turns, turns_has_more) = build_session_turn_details_page(
        connection,
        request.project_id,
        request.provider,
        request.session_id,
        TurnDetailOptions {
            include_raw: false,
            include_insights: false,
            narrative: matches!(request.view, SessionBundleView::Narrative),
        },
        request.turn_limit,
        request.turn_offset,
    )?;
    let session_files = build_session_files_query(
        connection,
        request.project_id,
        request.provider,
        request.session_id,
        request.project_root,
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
        session,
        turns,
        session_files,
    })
}
