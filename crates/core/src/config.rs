use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
pub use darc_paths::SourceKind;
use serde::{Deserialize, Serialize};

const CONFIG_VERSION: u32 = 1;

/// Represents the full shared config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedConfig {
    #[serde(default = "default_config_version")]
    pub version: u32,
    #[serde(default)]
    pub root: PathBuf,
    #[serde(default = "default_check_for_update_on_startup")]
    pub check_for_update_on_startup: bool,
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
    #[serde(default)]
    pub sources: SourcesConfig,
    #[serde(default, skip_serializing_if = "WatchConfig::is_default")]
    pub watch: WatchConfig,
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
            check_for_update_on_startup: false,
            projects,
            sources,
            watch: WatchConfig::default(),
        }
    }
}

/// Returns the default setting for interactive startup update checks.
fn default_check_for_update_on_startup() -> bool {
    false
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

/// Stores workspace-level defaults for continuous refresh mode.
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WatchConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debounce: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_interval: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconcile_interval: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<SourceKind>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub poll: bool,
}

impl WatchConfig {
    /// Returns whether the watch config has no explicit settings.
    pub fn is_default(config: &Self) -> bool {
        config == &Self::default()
    }
}

/// Returns whether a boolean is false for default-skipping serialization.
fn is_false(value: &bool) -> bool {
    !*value
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

/// Loads and deserializes the full shared config from disk.
pub fn load_config(path: &Path) -> Result<SharedConfig> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&content).context("failed to parse shared config")
}
