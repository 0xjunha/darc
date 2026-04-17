use std::path::Path;

use anyhow::{Context, Result};
use darc_paths::SourceKind;
use rusqlite::{Connection, TransactionBehavior};

use super::{
    SessionBundleQueryData, SessionBundleView, TurnDetailOptions, open_existing_index_database,
};
use crate::query::{
    files::build_session_files_query, projects::query_session_summary,
    turns::build_session_turn_details,
};

/// Queries one composite session bundle from indexed session, turn, and file summaries.
pub fn query_project_session_bundle(
    index_db_path: &Path,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    project_root: Option<&Path>,
    view: SessionBundleView,
) -> Result<SessionBundleQueryData> {
    let mut connection = open_existing_index_database(index_db_path)?;
    // Keep session summary, turn detail, and file summaries on one SQLite snapshot.
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .context("failed to open read transaction for session bundle query")?;
    let bundle = build_session_bundle_query(
        &transaction,
        project_id,
        provider,
        session_id,
        project_root,
        view,
    )?;
    transaction
        .commit()
        .context("failed to commit read transaction for session bundle query")?;
    Ok(bundle)
}

/// Builds one composite session bundle on top of the existing read-only query primitives.
fn build_session_bundle_query(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    project_root: Option<&Path>,
    view: SessionBundleView,
) -> Result<SessionBundleQueryData> {
    let session = query_session_summary(connection, project_id, provider, session_id)?;
    let turns = build_session_turn_details(
        connection,
        project_id,
        provider,
        session_id,
        TurnDetailOptions {
            include_raw: false,
            include_insights: false,
            narrative: matches!(view, SessionBundleView::Narrative),
        },
    )?;
    let session_files =
        build_session_files_query(connection, project_id, provider, session_id, project_root)?;
    Ok(SessionBundleQueryData {
        project_id: project_id.to_owned(),
        provider,
        session_id: session_id.to_owned(),
        view,
        session,
        turns,
        session_files,
    })
}
