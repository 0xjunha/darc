use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use darc_index::INDEX_DB_FILE_NAME;
pub use darc_query::{ClaudeRolloutAnalyticsReport, ClaudeSchemaAnalytics};
use darc_query::{ProjectAnalyticsRequest, report_project_claude_rollout_analytics};

use crate::{active_project::load_active_project, default_root_path};

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
    report_project_claude_rollout_analytics(&ProjectAnalyticsRequest {
        project_id: active_project.project.id,
        project_name: active_project.project.name,
        project_root: active_project.current_root,
        index_db_path: root.join(INDEX_DB_FILE_NAME),
    })
}
