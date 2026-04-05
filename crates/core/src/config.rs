use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    constants::{CLAUDE_DIR_NAME, CODEX_DIR_NAME},
    versions::CONFIG_VERSION,
};

/// Represents the full shared config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedConfig {
    #[serde(default = "default_config_version")]
    pub version: u32,
    #[serde(default)]
    pub root: PathBuf,
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
    #[serde(default)]
    pub sources: SourcesConfig,
}

fn default_config_version() -> u32 {
    CONFIG_VERSION
}

impl SharedConfig {
    /// Creates a new config with the given root and projects.
    pub fn new(root: PathBuf, projects: Vec<ProjectConfig>, sources: SourcesConfig) -> Self {
        Self {
            version: CONFIG_VERSION,
            root,
            projects,
            sources,
        }
    }
}

/// Stores one project entry inside the shared config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub local_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_upstream: Option<String>,
    pub sessions_root: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_paths: Vec<PathBuf>,
}

/// Groups the detected upstream source settings in the config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourcesConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeSourceConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexSourceConfig>,
}

/// Stores Claude-specific source settings in the shared config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeSourceConfig {
    pub enabled: bool,
    pub home: PathBuf,
    pub include_subagents: bool,
    pub projects_root: PathBuf,
}

/// Stores Codex-specific source settings in the shared config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexSourceConfig {
    pub enabled: bool,
    pub home: PathBuf,
    pub sessions_root: PathBuf,
}

/// Identifies which upstream tool produced a rollout tree.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Claude,
    Codex,
}

impl SourceKind {
    /// Returns the stable lowercase value used in persisted SQLite rows.
    pub(crate) const fn as_sql_text(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// Returns the stable directory name used for archived sessions.
    pub fn directory_name(self) -> &'static str {
        match self {
            Self::Claude => CLAUDE_DIR_NAME,
            Self::Codex => CODEX_DIR_NAME,
        }
    }

    /// Returns a human-readable name for the source kind.
    pub fn title(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }

    /// Parses one persisted lowercase SQLite value back into a source kind.
    pub(crate) fn from_sql_text(value: &str) -> Result<Self> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            other => anyhow::bail!("unsupported source kind `{other}` in SQLite index"),
        }
    }
}

/// Loads and deserializes the full shared config from disk.
pub fn load_config(path: &Path) -> Result<SharedConfig> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&content).context("failed to parse shared config")
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.title())
    }
}
