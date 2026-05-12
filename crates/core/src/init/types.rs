use std::{
    fmt::{self, Display},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use darc_store::INDEX_DB_FILE_NAME;

use crate::{
    config::{ProjectConfig, SharedConfig, SourceKind},
    constants::CONFIG_FILE_NAME,
};

/// Describes the shared config and project directories that `init` will create.
#[derive(Debug, Clone)]
pub struct InitDraft {
    pub global_config_exists: bool,
    pub project_exists: bool,
    pub sources: Vec<DetectedRolloutSource>,
    pub project: ProjectConfig,
    pub(super) config: SharedConfig,
}

impl InitDraft {
    /// Returns the shared root path stored in the config.
    pub fn root(&self) -> &Path {
        &self.config.root
    }

    /// Returns the shared config path derived from the root path.
    pub(super) fn config_path(&self) -> PathBuf {
        self.root().join(CONFIG_FILE_NAME)
    }

    /// Returns the shared index database path derived from the root path.
    pub(super) fn index_db_path(&self) -> PathBuf {
        self.root().join(INDEX_DB_FILE_NAME)
    }

    /// Serializes the shared config derived during init preparation.
    pub fn config_toml(&self) -> Result<String> {
        toml::to_string_pretty(&self.config).context("failed to serialize config TOML")
    }
}

impl Display for InitDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.global_config_exists {
            writeln!(f, "Global Darc: existing config detected")?;
        } else {
            writeln!(f, "Global Darc: no config detected")?;
            writeln!(f, "Detected sources:")?;
            for source in &self.sources {
                writeln!(f, "- {}", format_source(source))?;
            }
        }
        writeln!(f, "Root Path: {}", self.root().display())?;
        writeln!(f, "Config Path: {}", self.config_path().display())?;
        writeln!(f, "Index DB Path: {}", self.index_db_path().display())?;

        writeln!(f, "\nProject:")?;
        writeln!(f, "Name: {}", self.project.name)?;
        writeln!(f, "Local Path: {}", self.project.local_path.display())?;
        if let Some(upstream) = &self.project.git_upstream {
            writeln!(f, "Upstream: {upstream}")?;
        }
        Ok(())
    }
}

/// Summarizes one detected upstream rollout source.
#[derive(Debug, Clone)]
pub struct DetectedRolloutSource {
    pub home: PathBuf,
    pub kind: SourceKind,
    pub root: PathBuf,
    pub rollout_files: usize,
    pub subagent_rollout_files: usize,
}

fn format_source(source: &DetectedRolloutSource) -> String {
    match source.kind {
        SourceKind::Codex => format!(
            "{}: {} ({} rollouts)",
            source.kind.title(),
            source.root.display(),
            source.rollout_files,
        ),
        SourceKind::Claude if source.subagent_rollout_files > 0 => format!(
            "{}: {} ({} sessions, including {} subagents)",
            source.kind.title(),
            source.root.display(),
            source.rollout_files,
            source.subagent_rollout_files,
        ),
        SourceKind::Claude => format!(
            "{}: {} ({} sessions)",
            source.kind.title(),
            source.root.display(),
            source.rollout_files,
        ),
    }
}
