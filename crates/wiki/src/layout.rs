use std::{
    fs,
    path::{Path, PathBuf},
};

use darc_paths::is_valid_project_id;

use crate::{
    Result, WikiError,
    ids::{DigestId, EntryId, RunId},
};

/// Names the fixed wiki directory stored under the Darc root.
pub const CONTEXT_WIKI_DIR_NAME: &str = "context-wiki";

/// Stores the current on-disk storage layout version text.
pub const STORAGE_VERSION: &str = "1";

const RUN_REQUEST_FILE_NAME: &str = "request.json";
const RUN_CONTEXT_FILE_NAME: &str = "context.json";
const RUN_PROPOSAL_FILE_NAME: &str = "proposal.json";
const RUN_RESULT_FILE_NAME: &str = "result.json";
const RUN_EVENTS_FILE_NAME: &str = "events.jsonl";
const RUN_STDOUT_LOG_FILE_NAME: &str = "agent.stdout.log";
const RUN_STDERR_LOG_FILE_NAME: &str = "agent.stderr.log";
const RUN_CANCEL_FLAG_FILE_NAME: &str = "cancel.flag";

/// Resolves the top-level Context Wiki layout rooted under one Darc root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextWikiLayout {
    pub darc_root: PathBuf,
    pub root: PathBuf,
    pub version_path: PathBuf,
    pub projects_root: PathBuf,
}

impl ContextWikiLayout {
    /// Builds one Context Wiki layout rooted under the provided Darc directory.
    pub fn new(darc_root: impl Into<PathBuf>) -> Self {
        let darc_root = darc_root.into();
        let root = darc_root.join(CONTEXT_WIKI_DIR_NAME);
        Self {
            darc_root,
            version_path: root.join("VERSION"),
            projects_root: root.join("projects"),
            root,
        }
    }

    /// Creates the top-level Context Wiki directories and version file when missing.
    pub fn ensure_root(&self) -> Result<()> {
        if self.root.exists() {
            self.validate_root()?;
        } else {
            ensure_directory(&self.root)?;
        }
        ensure_directory(&self.projects_root)?;
        if !self.version_path.exists() {
            fs::write(&self.version_path, format!("{STORAGE_VERSION}\n")).map_err(|source| {
                WikiError::WriteFile {
                    path: self.version_path.clone(),
                    source,
                }
            })?;
        }
        Ok(())
    }

    /// Validates the top-level Context Wiki storage version when the root exists.
    pub fn validate_root(&self) -> Result<()> {
        if !self.root.exists() {
            return Ok(());
        }
        if !self.version_path.exists() {
            return Err(WikiError::MissingStorageVersion {
                path: self.version_path.clone(),
            });
        }

        let actual =
            fs::read_to_string(&self.version_path).map_err(|source| WikiError::ReadFile {
                path: self.version_path.clone(),
                source,
            })?;
        let actual = actual.trim().to_owned();
        if actual == STORAGE_VERSION {
            Ok(())
        } else {
            Err(WikiError::UnsupportedStorageVersion {
                path: self.version_path.clone(),
                expected: STORAGE_VERSION.to_owned(),
                actual,
            })
        }
    }

    /// Resolves one per-project Context Wiki layout from the top-level wiki root.
    pub fn project_layout(&self, project_id: impl Into<String>) -> Result<ProjectLayout> {
        let project_id = project_id.into();
        if !is_valid_project_id(&project_id) {
            return Err(WikiError::InvalidProjectId { value: project_id });
        }
        let root = self.projects_root.join(&project_id);
        Ok(ProjectLayout {
            context: self.clone(),
            project_id,
            root: root.clone(),
            registry_dir: root.join("registry"),
            categories_path: root.join("registry/categories.toml"),
            domains_path: root.join("registry/domains.toml"),
            entries_dir: root.join("entries"),
            digests_dir: root.join("digests"),
            runs_dir: root.join("runs"),
        })
    }
}

/// Resolves the full on-disk directory layout for one project wiki.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectLayout {
    context: ContextWikiLayout,
    pub project_id: String,
    pub root: PathBuf,
    pub registry_dir: PathBuf,
    pub categories_path: PathBuf,
    pub domains_path: PathBuf,
    pub entries_dir: PathBuf,
    pub digests_dir: PathBuf,
    pub runs_dir: PathBuf,
}

impl ProjectLayout {
    /// Returns the enclosing top-level Context Wiki layout for this project.
    pub fn context(&self) -> &ContextWikiLayout {
        &self.context
    }

    /// Creates the per-project directory tree when it does not exist yet.
    pub fn ensure(&self) -> Result<()> {
        self.context.ensure_root()?;
        ensure_directory(&self.root)?;
        ensure_directory(&self.registry_dir)?;
        ensure_directory(&self.entries_dir)?;
        ensure_directory(&self.digests_dir)?;
        ensure_directory(&self.runs_dir)?;
        Ok(())
    }

    /// Validates the enclosing Context Wiki storage version for read-side access.
    pub fn validate_storage(&self) -> Result<()> {
        self.context.validate_root()
    }

    /// Resolves one category directory path under the canonical entries root.
    pub fn entry_category_dir(&self, category: &str) -> PathBuf {
        self.entries_dir.join(category)
    }

    /// Resolves one canonical entry Markdown path under the category-scoped entries layout.
    pub fn entry_path(&self, category: &str, entry_id: &EntryId) -> PathBuf {
        self.entry_category_dir(category)
            .join(format!("{entry_id}.md"))
    }

    /// Resolves one canonical digest Markdown path under the project layout.
    pub fn digest_path(&self, digest_id: &DigestId) -> PathBuf {
        self.digests_dir.join(format!("{digest_id}.md"))
    }

    /// Resolves one run directory path under the project layout.
    pub fn run_dir(&self, run_id: &RunId) -> PathBuf {
        self.runs_dir.join(run_id.as_str())
    }

    /// Resolves one run state TOML path under the project layout.
    pub fn run_state_path(&self, run_id: &RunId) -> PathBuf {
        self.run_dir(run_id).join("run.toml")
    }

    /// Resolves one run request artifact path under the project layout.
    pub fn run_request_path(&self, run_id: &RunId) -> PathBuf {
        self.run_dir(run_id).join(RUN_REQUEST_FILE_NAME)
    }

    /// Resolves one run context artifact path under the project layout.
    pub fn run_context_path(&self, run_id: &RunId) -> PathBuf {
        self.run_dir(run_id).join(RUN_CONTEXT_FILE_NAME)
    }

    /// Resolves one run proposal artifact path under the project layout.
    pub fn run_proposal_path(&self, run_id: &RunId) -> PathBuf {
        self.run_dir(run_id).join(RUN_PROPOSAL_FILE_NAME)
    }

    /// Resolves one run result artifact path under the project layout.
    pub fn run_result_path(&self, run_id: &RunId) -> PathBuf {
        self.run_dir(run_id).join(RUN_RESULT_FILE_NAME)
    }

    /// Resolves one run events log path under the project layout.
    pub fn run_events_path(&self, run_id: &RunId) -> PathBuf {
        self.run_dir(run_id).join(RUN_EVENTS_FILE_NAME)
    }

    /// Resolves one run stdout log path under the project layout.
    pub fn run_stdout_log_path(&self, run_id: &RunId) -> PathBuf {
        self.run_dir(run_id).join(RUN_STDOUT_LOG_FILE_NAME)
    }

    /// Resolves one run stderr log path under the project layout.
    pub fn run_stderr_log_path(&self, run_id: &RunId) -> PathBuf {
        self.run_dir(run_id).join(RUN_STDERR_LOG_FILE_NAME)
    }

    /// Resolves one run cancel flag path under the project layout.
    pub fn run_cancel_flag_path(&self, run_id: &RunId) -> PathBuf {
        self.run_dir(run_id).join(RUN_CANCEL_FLAG_FILE_NAME)
    }
}

/// Creates one directory path and all missing parents.
fn ensure_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|source| WikiError::CreateDir {
        path: path.to_path_buf(),
        source,
    })
}
