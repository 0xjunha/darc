use std::{collections::BTreeSet, path::PathBuf};

use serde::{Deserialize, Serialize};

/// Identifies which upstream tool produced one archived session tree.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum SourceKind {
    Claude,
    Codex,
}

impl SourceKind {
    /// Returns the stable directory name used for archived sessions.
    pub fn directory_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// Stores the Claude-specific source settings needed for sync discovery.
#[derive(Debug, Clone)]
pub struct ClaudeSource {
    pub include_subagents: bool,
    pub projects_root: PathBuf,
}

/// Stores the Codex-specific source settings needed for sync discovery.
#[derive(Debug, Clone)]
pub struct CodexSource {
    pub home: PathBuf,
    pub sessions_root: PathBuf,
}

/// Collects the explicit project and source inputs required to plan a sync.
#[derive(Debug, Clone)]
pub struct SyncRequest {
    pub project_name: String,
    pub project_root: PathBuf,
    pub sessions_root: PathBuf,
    pub primary_project_path: PathBuf,
    pub stored_known_paths: BTreeSet<PathBuf>,
    pub project_paths: BTreeSet<PathBuf>,
    pub other_project_paths: BTreeSet<PathBuf>,
    pub project_upstream: Option<String>,
    pub sources: Vec<SourceKind>,
    pub claude: Option<ClaudeSource>,
    pub codex: Option<CodexSource>,
}
