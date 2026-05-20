use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    error::Error as StdError,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use darc_paths::SourceKind;
#[cfg(test)]
use darc_rollout::codex::{CodexRollout, parse_rollout_file as parse_codex_rollout_file};
use darc_rollout::{
    ParseDeterminism,
    claude::{
        ClaudeArchivedContext, ClaudeError, ClaudeSessionKind,
        parse_rollout_file as parse_claude_rollout_file, resolve_claude_parse_determinism,
    },
    codex::{
        CodexError, CodexRolloutHeader, CodexRolloutSink, ParseIntoError, compare_rollout_priority,
        parse_rollout_file_into as parse_codex_rollout_file_into, parse_rollout_file_session_id,
        parse_rollout_session_meta_line, read_first_rollout_line_bytes,
        reconcile_rollout_session_id, resolve_codex_parse_determinism,
    },
    model::NormalizedTurn as CodexTurn,
};
#[cfg(test)]
use darc_store::INDEX_DB_FILE_NAME;
use darc_store::{
    StoredSessionKind, StoredSessionRecord, StoredTurnRecord, insert_session_record,
    insert_turn_record, open_index_database,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use thiserror::Error;
use walkdir::WalkDir;

const SESSION_PROGRESS_EMIT_INTERVAL: usize = 128;

/// Parses one Codex rollout file into user-visible turns.
#[cfg(test)]
pub(crate) fn parse_codex_rollout(path: &Path) -> Result<CodexRollout> {
    Ok(parse_codex_rollout_file(path)?)
}

/// Reports the results of indexing archived sessions for one project.
#[derive(Debug, Clone)]
pub struct IndexReport {
    pub project_name: String,
    pub project_root: PathBuf,
    pub sessions_root: PathBuf,
    pub index_db_path: PathBuf,
    pub providers: Vec<SourceKind>,
    pub sessions_discovered: usize,
    pub sessions_skipped_this_run: usize,
    pub sessions_currently_indexed: usize,
    pub turns_currently_indexed: usize,
    pub skipped_rollouts: Vec<SkippedRollout>,
}

/// Describes one observable indexing transition for progress UIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexProgress {
    IndexingSessions {
        indexed_sessions: usize,
        total_sessions: usize,
    },
}

/// Describes one archived rollout file that darc skipped during indexing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRollout {
    pub provider: SourceKind,
    pub source_path: PathBuf,
    pub logical_session_id: Option<String>,
    pub cli_version: Option<String>,
    pub reason: String,
}

/// Preserves the old Codex-specific skipped rollout name for compatibility.
pub type SkippedCodexRollout = SkippedRollout;

/// Stores the explicit project inputs required to index one archive tree.
#[derive(Debug, Clone)]
pub struct ProjectIndexRequest {
    pub project_id: String,
    pub project_name: String,
    pub project_root: PathBuf,
    pub sessions_root: PathBuf,
    pub index_db_path: PathBuf,
}

/// Identifies the normalized indexed session shape used by the shared indexing pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexedSessionKind {
    Primary,
    Subagent,
}

impl IndexedSessionKind {
    /// Converts one indexed session kind into the store-owned session kind.
    fn into_stored_kind(self) -> StoredSessionKind {
        match self {
            Self::Primary => StoredSessionKind::Primary,
            Self::Subagent => StoredSessionKind::Subagent,
        }
    }
}

impl From<ClaudeSessionKind> for IndexedSessionKind {
    fn from(value: ClaudeSessionKind) -> Self {
        match value {
            ClaudeSessionKind::Primary => Self::Primary,
            ClaudeSessionKind::Subagent => Self::Subagent,
        }
    }
}

/// Parses one persisted lowercase SQLite provider value back into a source kind.
fn source_kind_from_sql_text(value: &str) -> Result<SourceKind> {
    match value {
        "claude" => Ok(SourceKind::Claude),
        "codex" => Ok(SourceKind::Codex),
        other => anyhow::bail!("unsupported source kind `{other}` in SQLite index"),
    }
}

/// Stores one parsed provider rollout in the normalized indexing model.
#[derive(Debug, Clone)]
struct IndexedRollout {
    session_id: String,
    cwd: PathBuf,
    cli_version: Option<String>,
    schema_id: String,
    determinism: ParseDeterminism,
    turns: Vec<CodexTurn>,
}

/// Stores one selected archived rollout before it is parsed and indexed.
#[derive(Debug, Clone)]
struct ArchivedRolloutCandidate {
    provider: SourceKind,
    source_path: PathBuf,
    archive_path: String,
    session_id: String,
    parent_session_id: Option<String>,
    rollout_session_id: String,
    agent_id: Option<String>,
    session_kind: IndexedSessionKind,
    cli_version: Option<String>,
    size: u64,
    mtime_ms: u64,
}

/// Stores the stable logical identity for one archived rollout candidate.
#[derive(Debug, Clone)]
struct ArchivedRolloutIdentity {
    provider: SourceKind,
    session_id: String,
    parent_session_id: Option<String>,
    rollout_session_id: String,
    agent_id: Option<String>,
    session_kind: IndexedSessionKind,
}

/// Stores one archived rollout inspection result before duplicate grouping.
#[derive(Debug, Clone)]
enum ArchivedRolloutInspection {
    Candidate(ArchivedRolloutCandidate),
    Skipped(SkippedRollout),
}

/// Stores the ordered archived rollout duplicates for one logical session id.
#[derive(Debug, Clone)]
struct ArchivedRolloutGroup {
    provider: SourceKind,
    session_id: String,
    candidates: Vec<ArchivedRolloutCandidate>,
}

impl ArchivedRolloutGroup {
    /// Returns the provider/session key shared by one duplicate rollout group.
    fn session_key(&self) -> IndexedSessionKey {
        (self.provider, self.session_id.clone())
    }
}

/// Stores the indexed snapshot used to skip unchanged archived sessions.
#[derive(Debug, Clone)]
struct IndexedSessionSnapshot {
    archive_path: String,
    determinism: Option<String>,
    source_size: Option<u64>,
    source_mtime_ms: Option<u64>,
}

impl IndexedSessionSnapshot {
    /// Returns whether one archived rollout still matches the indexed session snapshot.
    fn matches_candidate(&self, candidate: &ArchivedRolloutCandidate) -> bool {
        self.archive_path == candidate.archive_path
            && self.matches_current_determinism(candidate)
            && self.source_size == Some(candidate.size)
            && self.source_mtime_ms == Some(candidate.mtime_ms)
    }

    /// Returns whether the indexed row still matches the current parser's determinism.
    fn matches_current_determinism(&self, candidate: &ArchivedRolloutCandidate) -> bool {
        let Some(stored_determinism) = self.determinism.as_deref() else {
            return false;
        };
        let Some(expected_determinism) = expected_parse_determinism(candidate) else {
            return true;
        };
        if stored_determinism == expected_determinism.as_sql_text() {
            return true;
        }
        candidate.provider == SourceKind::Claude
            && expected_determinism == ParseDeterminism::Exact
            && stored_determinism == ParseDeterminism::BestEffortForward.as_sql_text()
    }
}

/// Resolves the parser determinism currently expected for one archived candidate.
fn expected_parse_determinism(candidate: &ArchivedRolloutCandidate) -> Option<ParseDeterminism> {
    match candidate.provider {
        SourceKind::Codex => candidate
            .cli_version
            .as_deref()
            .and_then(|version| resolve_codex_parse_determinism(version).ok()),
        SourceKind::Claude => Some(resolve_claude_parse_determinism(
            candidate.cli_version.as_deref(),
        )),
    }
}

/// Preserves one SQLite sink failure while retaining the original anyhow chain.
#[derive(Debug, Error)]
#[error(transparent)]
struct SqliteSessionSinkError(#[from] anyhow::Error);

impl SqliteSessionSinkError {
    /// Returns the original sink failure without wrapping it again.
    fn into_inner(self) -> anyhow::Error {
        self.0
    }
}

/// Streams one normalized rollout into SQLite session and turn rows.
struct SqliteSessionWriter<'conn> {
    connection: &'conn Connection,
    project_id: String,
    provider: SourceKind,
    archive_path: String,
    parent_session_id: Option<String>,
    session_kind: IndexedSessionKind,
    source_size: u64,
    source_mtime_ms: u64,
    session_id: Option<String>,
    turn_ordinal: i64,
}

impl<'conn> SqliteSessionWriter<'conn> {
    /// Creates one SQLite writer for a selected archived rollout candidate.
    fn new(
        connection: &'conn Connection,
        project_id: &str,
        archived: &ArchivedRolloutCandidate,
    ) -> Self {
        Self {
            connection,
            project_id: project_id.to_owned(),
            provider: archived.provider,
            archive_path: archived.archive_path.clone(),
            parent_session_id: archived.parent_session_id.clone(),
            session_kind: archived.session_kind,
            source_size: archived.size,
            source_mtime_ms: archived.mtime_ms,
            session_id: None,
            turn_ordinal: 0,
        }
    }

    /// Inserts one normalized session row before any turns are written.
    fn begin_session(
        &mut self,
        session_id: &str,
        cwd: &Path,
        cli_version: Option<&str>,
        schema_id: &str,
        determinism: ParseDeterminism,
    ) -> Result<()> {
        insert_session_record(
            self.connection,
            &StoredSessionRecord {
                project_id: &self.project_id,
                provider: self.provider,
                session_id,
                parent_session_id: self.parent_session_id.as_deref(),
                session_kind: self.session_kind.into_stored_kind(),
                archive_path: &self.archive_path,
                cwd,
                cli_version,
                schema_id,
                determinism,
                source_size: self.source_size,
                source_mtime_ms: self.source_mtime_ms,
            },
        )?;
        self.session_id = Some(session_id.to_owned());
        self.turn_ordinal = 0;
        Ok(())
    }

    /// Inserts one normalized turn row for the active session.
    fn push_turn(&mut self, turn: CodexTurn) -> Result<()> {
        let session_id = self
            .session_id
            .as_deref()
            .context("missing active session id while inserting turn")?;
        let turn_ordinal = self.turn_ordinal;
        insert_turn_record(
            self.connection,
            StoredTurnRecord {
                project_id: &self.project_id,
                provider: self.provider,
                session_id,
                turn_ordinal,
                turn,
            },
        )?;
        self.turn_ordinal += 1;

        Ok(())
    }

    /// Writes one parsed rollout by inserting its session row and every normalized turn.
    fn write_rollout(&mut self, rollout: IndexedRollout) -> Result<()> {
        self.begin_session(
            &rollout.session_id,
            &rollout.cwd,
            rollout.cli_version.as_deref(),
            &rollout.schema_id,
            rollout.determinism,
        )?;
        for turn in rollout.turns {
            self.push_turn(turn)?;
        }
        Ok(())
    }
}

impl CodexRolloutSink for SqliteSessionWriter<'_> {
    type Error = SqliteSessionSinkError;

    fn begin_rollout(
        &mut self,
        header: &CodexRolloutHeader,
    ) -> std::result::Result<(), Self::Error> {
        self.begin_session(
            &header.session_id,
            &header.cwd,
            Some(header.cli_version.as_str()),
            header.schema_id.as_str(),
            header.determinism,
        )
        .map_err(SqliteSessionSinkError::from)
    }

    fn push_turn(&mut self, turn: CodexTurn) -> std::result::Result<(), Self::Error> {
        SqliteSessionWriter::push_turn(self, turn).map_err(SqliteSessionSinkError::from)
    }
}

/// Collects discovered archived rollout groups and any files skipped before grouping.
#[derive(Debug, Clone)]
struct DiscoveredArchivedRollouts {
    groups: Vec<ArchivedRolloutGroup>,
    discovered_session_ids: BTreeSet<IndexedSessionKey>,
    skipped_rollouts: Vec<SkippedRollout>,
}

/// Describes the SQLite index changes produced by one indexing run.
#[derive(Debug, Clone)]
struct IndexedIndexOutcome {
    sessions_succeeded: usize,
    sessions_currently_indexed: usize,
    skipped_rollouts: Vec<SkippedRollout>,
    turns_currently_indexed: usize,
}

/// Stores one provider/session key inside the normalized session index.
type IndexedSessionKey = (SourceKind, String);

/// Distinguishes rollout parse failures from infrastructure failures while indexing one candidate.
#[derive(Debug, Error)]
enum CandidateIndexError {
    #[error(transparent)]
    Parse(Box<CandidateParseError>),
    #[error(transparent)]
    Infrastructure(#[from] anyhow::Error),
}

/// Preserves one rollout parse failure that should be reported as a skipped candidate.
#[derive(Debug, Error)]
enum CandidateParseError {
    #[error("failed to parse {}", .path.display())]
    Codex {
        path: PathBuf,
        #[source]
        source: Box<darc_rollout::codex::CodexError>,
    },
    #[error("failed to parse {}", .path.display())]
    Claude {
        path: PathBuf,
        #[source]
        source: Box<darc_rollout::claude::ClaudeError>,
    },
}

impl From<CandidateParseError> for CandidateIndexError {
    fn from(value: CandidateParseError) -> Self {
        Self::Parse(Box::new(value))
    }
}

#[cfg(test)]
pub(crate) const TEST_PROJECT_ID: &str = "repo-abc123";

/// Indexes archived Codex rollouts for one explicit test project directory.
#[cfg(test)]
pub(crate) fn index_project_codex_turns_from(
    project_root: &Path,
    root: PathBuf,
) -> Result<IndexReport> {
    index_project_sessions_from(project_root, root, &[SourceKind::Codex])
}

/// Indexes archived provider rollouts for one explicit project request.
pub fn index_project_archived_sessions(
    request: &ProjectIndexRequest,
    providers: &[SourceKind],
) -> Result<IndexReport> {
    let mut progress = |_| {};
    index_project_archived_sessions_with_progress(request, providers, &mut progress)
}

/// Indexes archived provider rollouts while reporting session progress.
pub fn index_project_archived_sessions_with_progress(
    request: &ProjectIndexRequest,
    providers: &[SourceKind],
    mut progress: impl FnMut(IndexProgress),
) -> Result<IndexReport> {
    let discovered_rollouts = discover_archived_rollouts(&request.sessions_root, providers)?;
    let mut connection = open_index_database(&request.index_db_path)?;
    let total_sessions = discovered_rollouts.groups.len();
    progress(IndexProgress::IndexingSessions {
        indexed_sessions: 0,
        total_sessions,
    });

    let index_outcome = update_project_turns(
        &mut connection,
        &request.project_id,
        providers,
        &discovered_rollouts.groups,
        &discovered_rollouts.discovered_session_ids,
        &mut progress,
    )?;
    let mut skipped_rollouts = discovered_rollouts.skipped_rollouts;
    skipped_rollouts.extend(index_outcome.skipped_rollouts);
    let sessions_discovered = discovered_rollouts.discovered_session_ids.len();
    let sessions_skipped_this_run = sessions_discovered
        .checked_sub(index_outcome.sessions_succeeded)
        .context("successful indexed session count exceeds discovered sessions")?;

    Ok(IndexReport {
        project_name: request.project_name.clone(),
        project_root: request.project_root.clone(),
        sessions_root: request.sessions_root.clone(),
        index_db_path: request.index_db_path.clone(),
        providers: providers.to_vec(),
        sessions_discovered,
        sessions_skipped_this_run,
        sessions_currently_indexed: index_outcome.sessions_currently_indexed,
        turns_currently_indexed: index_outcome.turns_currently_indexed,
        skipped_rollouts,
    })
}

/// Indexes archived Codex rollouts for one explicit project request.
pub fn index_project_archived_codex_turns(request: &ProjectIndexRequest) -> Result<IndexReport> {
    index_project_archived_sessions(request, &[SourceKind::Codex])
}

/// Indexes archived provider rollouts for one explicit test project directory.
#[cfg(test)]
pub(crate) fn index_project_sessions_from(
    project_root: &Path,
    root: PathBuf,
    providers: &[SourceKind],
) -> Result<IndexReport> {
    let request = ProjectIndexRequest {
        project_id: TEST_PROJECT_ID.to_owned(),
        project_name: "repo".to_owned(),
        project_root: fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf()),
        sessions_root: root.join("projects").join(TEST_PROJECT_ID).join("sessions"),
        index_db_path: root.join(INDEX_DB_FILE_NAME),
    };
    index_project_archived_sessions(&request, providers)
}

/// Discovers and deduplicates archived rollout files below one project sessions root.
fn discover_archived_rollouts(
    sessions_root: &Path,
    providers: &[SourceKind],
) -> Result<DiscoveredArchivedRollouts> {
    let mut rollout_candidates = Vec::new();
    let mut discovered_session_ids = BTreeSet::<IndexedSessionKey>::new();
    let mut skipped_rollouts = Vec::new();

    for provider in providers {
        let root = sessions_root.join(provider.directory_name());
        let rollout_paths = match provider {
            SourceKind::Codex => discover_archived_codex_rollout_paths(&root)?,
            SourceKind::Claude => discover_archived_claude_rollout_paths(&root)?,
        };
        for path in &rollout_paths {
            let inspection = match provider {
                SourceKind::Codex => inspect_archived_codex_rollout(path, sessions_root)?,
                SourceKind::Claude => inspect_archived_claude_rollout(path, sessions_root)?,
            };
            match inspection {
                ArchivedRolloutInspection::Candidate(candidate) => {
                    discovered_session_ids
                        .insert((candidate.provider, candidate.session_id.clone()));
                    rollout_candidates.push(candidate);
                }
                ArchivedRolloutInspection::Skipped(skipped) => {
                    if let Some(session_id) = &skipped.logical_session_id {
                        discovered_session_ids.insert((skipped.provider, session_id.clone()));
                    }
                    skipped_rollouts.push(skipped);
                }
            }
        }
    }

    Ok(DiscoveredArchivedRollouts {
        groups: group_archived_rollouts(rollout_candidates),
        discovered_session_ids,
        skipped_rollouts,
    })
}

/// Discovers archived Codex rollout paths below one project archive root.
fn discover_archived_codex_rollout_paths(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut rollout_paths = Vec::new();
    for entry in WalkDir::new(root) {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        if !entry.file_type().is_file() || !is_archived_codex_rollout(entry.path()) {
            continue;
        }
        rollout_paths.push(entry.into_path());
    }
    rollout_paths.sort();
    Ok(rollout_paths)
}

/// Returns whether one path points at an archived Codex rollout file.
fn is_archived_codex_rollout(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-"))
}

/// Reads lightweight metadata for one archived rollout before deep parsing.
fn inspect_archived_codex_rollout(
    path: &Path,
    sessions_root: &Path,
) -> Result<ArchivedRolloutInspection> {
    let (size, mtime_ms) = file_snapshot(path)?;
    let archive_path = path
        .strip_prefix(sessions_root)
        .with_context(|| {
            format!(
                "failed to strip project sessions root {} from {}",
                sessions_root.display(),
                path.display()
            )
        })?
        .to_string_lossy()
        .into_owned();
    let file_name = path.file_name().and_then(|name| name.to_str());
    let filename_session_id = file_name.and_then(parse_rollout_file_session_id);
    let Some(first_line) = read_first_rollout_line_bytes(path)? else {
        return Ok(ArchivedRolloutInspection::Skipped(build_skipped_rollout(
            SourceKind::Codex,
            path,
            filename_session_id,
            None,
            format!("missing session_meta line in {}", path.display()),
        )));
    };
    let first_line = match String::from_utf8(first_line) {
        Ok(first_line) => first_line,
        Err(_error) if filename_session_id.is_some() => {
            let session_id = filename_session_id.expect("checked above");
            return Ok(ArchivedRolloutInspection::Candidate(
                build_archived_rollout_candidate(
                    path,
                    &archive_path,
                    ArchivedRolloutIdentity {
                        provider: SourceKind::Codex,
                        session_id: session_id.clone(),
                        parent_session_id: None,
                        rollout_session_id: session_id,
                        agent_id: None,
                        session_kind: IndexedSessionKind::Primary,
                    },
                    None,
                    size,
                    mtime_ms,
                ),
            ));
        }
        Err(error) => {
            return Ok(ArchivedRolloutInspection::Skipped(build_skipped_rollout(
                SourceKind::Codex,
                path,
                filename_session_id,
                None,
                error.to_string(),
            )));
        }
    };
    let meta = match parse_rollout_session_meta_line(&first_line, path) {
        Ok(Some(meta)) => meta,
        Ok(None) if filename_session_id.is_some() => {
            let session_id = filename_session_id.expect("checked above");
            return Ok(ArchivedRolloutInspection::Candidate(
                build_archived_rollout_candidate(
                    path,
                    &archive_path,
                    ArchivedRolloutIdentity {
                        provider: SourceKind::Codex,
                        session_id: session_id.clone(),
                        parent_session_id: None,
                        rollout_session_id: session_id,
                        agent_id: None,
                        session_kind: IndexedSessionKind::Primary,
                    },
                    None,
                    size,
                    mtime_ms,
                ),
            ));
        }
        Ok(None) => {
            return Ok(ArchivedRolloutInspection::Skipped(build_skipped_rollout(
                SourceKind::Codex,
                path,
                filename_session_id,
                None,
                format!("missing session_meta line in {}", path.display()),
            )));
        }
        Err(_error) if filename_session_id.is_some() => {
            let session_id = filename_session_id.expect("checked above");
            return Ok(ArchivedRolloutInspection::Candidate(
                build_archived_rollout_candidate(
                    path,
                    &archive_path,
                    ArchivedRolloutIdentity {
                        provider: SourceKind::Codex,
                        session_id: session_id.clone(),
                        parent_session_id: None,
                        rollout_session_id: session_id,
                        agent_id: None,
                        session_kind: IndexedSessionKind::Primary,
                    },
                    None,
                    size,
                    mtime_ms,
                ),
            ));
        }
        Err(error) => {
            return Ok(ArchivedRolloutInspection::Skipped(build_skipped_rollout(
                SourceKind::Codex,
                path,
                filename_session_id,
                None,
                error.to_string(),
            )));
        }
    };
    let payload_session_id = meta.session_id.clone();
    let session_id =
        match reconcile_rollout_session_id(path, file_name, Some(payload_session_id.as_str())) {
            Ok(Some(session_id)) => session_id,
            Ok(None) => {
                return Ok(ArchivedRolloutInspection::Skipped(build_skipped_rollout(
                    SourceKind::Codex,
                    path,
                    Some(payload_session_id),
                    meta.cli_version,
                    format!(
                        "failed to derive archived Codex session id from {}",
                        path.display()
                    ),
                )));
            }
            Err(error) => {
                return Ok(ArchivedRolloutInspection::Skipped(build_skipped_rollout(
                    SourceKind::Codex,
                    path,
                    filename_session_id.or(Some(payload_session_id)),
                    meta.cli_version,
                    error.to_string(),
                )));
            }
        };

    Ok(ArchivedRolloutInspection::Candidate(
        build_archived_rollout_candidate(
            path,
            &archive_path,
            ArchivedRolloutIdentity {
                provider: SourceKind::Codex,
                session_id: session_id.clone(),
                parent_session_id: None,
                rollout_session_id: session_id,
                agent_id: None,
                session_kind: IndexedSessionKind::Primary,
            },
            meta.cli_version,
            size,
            mtime_ms,
        ),
    ))
}

/// Discovers archived Claude rollout paths below one project archive root.
fn discover_archived_claude_rollout_paths(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut rollout_paths = Vec::new();
    for entry in WalkDir::new(root) {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        rollout_paths.push(entry.into_path());
    }
    rollout_paths.sort();
    Ok(rollout_paths)
}

/// Reads lightweight metadata for one archived Claude rollout before deep parsing.
fn inspect_archived_claude_rollout(
    path: &Path,
    sessions_root: &Path,
) -> Result<ArchivedRolloutInspection> {
    let (size, mtime_ms) = file_snapshot(path)?;
    let cli_version = read_claude_rollout_cli_version(path);
    let archive_path = path
        .strip_prefix(sessions_root)
        .with_context(|| {
            format!(
                "failed to strip project sessions root {} from {}",
                sessions_root.display(),
                path.display()
            )
        })?
        .to_string_lossy()
        .into_owned();
    let provider_root = sessions_root.join(SourceKind::Claude.directory_name());
    let relative = path
        .strip_prefix(&provider_root)
        .with_context(|| {
            format!(
                "failed to strip Claude archive root {} from {}",
                provider_root.display(),
                path.display()
            )
        })?
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    match relative.as_slice() {
        [parent_session_id, file_name] => {
            let file_stem = Path::new(file_name)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned);
            if file_stem.as_deref() != Some(parent_session_id.as_str()) {
                return Ok(ArchivedRolloutInspection::Skipped(build_skipped_rollout(
                    SourceKind::Claude,
                    path,
                    file_stem,
                    None,
                    format!(
                        "invalid archived Claude parent rollout path {}",
                        path.display()
                    ),
                )));
            }

            Ok(ArchivedRolloutInspection::Candidate(
                build_archived_rollout_candidate(
                    path,
                    &archive_path,
                    ArchivedRolloutIdentity {
                        provider: SourceKind::Claude,
                        session_id: parent_session_id.clone(),
                        parent_session_id: None,
                        rollout_session_id: parent_session_id.clone(),
                        agent_id: None,
                        session_kind: IndexedSessionKind::Primary,
                    },
                    cli_version.clone(),
                    size,
                    mtime_ms,
                ),
            ))
        }
        [parent_session_id, subagents_dir, file_name] if subagents_dir == "subagents" => {
            let Some(agent_id) = Path::new(file_name)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
            else {
                return Ok(ArchivedRolloutInspection::Skipped(build_skipped_rollout(
                    SourceKind::Claude,
                    path,
                    None,
                    None,
                    format!(
                        "invalid archived Claude subagent filename {}",
                        path.display()
                    ),
                )));
            };

            Ok(ArchivedRolloutInspection::Candidate(
                build_archived_rollout_candidate(
                    path,
                    &archive_path,
                    ArchivedRolloutIdentity {
                        provider: SourceKind::Claude,
                        session_id: format!("{parent_session_id}/subagents/{agent_id}"),
                        parent_session_id: Some(parent_session_id.clone()),
                        rollout_session_id: parent_session_id.clone(),
                        agent_id: Some(agent_id),
                        session_kind: IndexedSessionKind::Subagent,
                    },
                    cli_version,
                    size,
                    mtime_ms,
                ),
            ))
        }
        _ => Ok(ArchivedRolloutInspection::Skipped(build_skipped_rollout(
            SourceKind::Claude,
            path,
            None,
            None,
            format!(
                "unsupported archived Claude rollout path {}",
                path.display()
            ),
        ))),
    }
}

/// Reads a Claude rollout version cheaply enough for unchanged-session skip checks.
fn read_claude_rollout_cli_version(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file)
        .lines()
        .map_while(|line| line.ok())
        .take(128)
    {
        let Ok(object) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&line)
        else {
            continue;
        };
        if let Some(version) = object.get("version").and_then(serde_json::Value::as_str) {
            return Some(version.to_owned());
        }
    }
    None
}

/// Groups archived rollout duplicates by provider/session id and orders each group by priority.
fn group_archived_rollouts(
    rollout_candidates: Vec<ArchivedRolloutCandidate>,
) -> Vec<ArchivedRolloutGroup> {
    let mut grouped_rollouts = BTreeMap::<IndexedSessionKey, Vec<ArchivedRolloutCandidate>>::new();

    for candidate in rollout_candidates {
        grouped_rollouts
            .entry((candidate.provider, candidate.session_id.clone()))
            .or_default()
            .push(candidate);
    }

    grouped_rollouts
        .into_iter()
        .map(|((provider, session_id), mut candidates)| {
            candidates
                .sort_by(|left, right| compare_archived_rollout_candidates(left, right).reverse());
            ArchivedRolloutGroup {
                provider,
                session_id,
                candidates,
            }
        })
        .collect()
}

/// Compares two archived rollout candidates using the shared duplicate priority rules.
fn compare_archived_rollout_candidates(
    left: &ArchivedRolloutCandidate,
    right: &ArchivedRolloutCandidate,
) -> Ordering {
    compare_rollout_priority(
        left.size,
        left.mtime_ms,
        &left.source_path,
        right.size,
        right.mtime_ms,
        &right.source_path,
    )
}

/// Reads stable comparison metadata from one archived rollout file.
pub(crate) fn file_snapshot(path: &Path) -> Result<(u64, u64)> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("failed to read modified time for {}", path.display()))?;
    let mtime_ms = modified
        .duration_since(std::time::UNIX_EPOCH)
        .with_context(|| format!("modified time predates Unix epoch for {}", path.display()))?
        .as_millis()
        .try_into()
        .context("modified time exceeds u64 milliseconds range")?;

    Ok((metadata.len(), mtime_ms))
}

/// Builds one archived rollout candidate from inspected file metadata.
fn build_archived_rollout_candidate(
    source_path: &Path,
    archive_path: &str,
    identity: ArchivedRolloutIdentity,
    cli_version: Option<String>,
    size: u64,
    mtime_ms: u64,
) -> ArchivedRolloutCandidate {
    ArchivedRolloutCandidate {
        provider: identity.provider,
        source_path: source_path.to_path_buf(),
        archive_path: archive_path.to_owned(),
        session_id: identity.session_id,
        parent_session_id: identity.parent_session_id,
        rollout_session_id: identity.rollout_session_id,
        agent_id: identity.agent_id,
        session_kind: identity.session_kind,
        cli_version,
        size,
        mtime_ms,
    }
}

/// Normalizes one rollout failure into the shared index report shape.
fn build_skipped_rollout(
    provider: SourceKind,
    source_path: &Path,
    logical_session_id: Option<String>,
    cli_version: Option<String>,
    reason: String,
) -> SkippedRollout {
    SkippedRollout {
        provider,
        source_path: source_path.to_path_buf(),
        logical_session_id,
        cli_version,
        reason,
    }
}

/// Updates one project's indexed provider sessions and turns inside SQLite.
fn update_project_turns(
    connection: &mut Connection,
    project_id: &str,
    providers: &[SourceKind],
    archived_rollouts: &[ArchivedRolloutGroup],
    discovered_session_ids: &BTreeSet<IndexedSessionKey>,
    progress: &mut impl FnMut(IndexProgress),
) -> Result<IndexedIndexOutcome> {
    let provider_set = providers.iter().copied().collect::<BTreeSet<_>>();
    let mut transaction = connection
        .transaction()
        .context("failed to begin SQLite transaction")?;
    let indexed_snapshots =
        load_indexed_session_snapshots(&transaction, project_id, &provider_set)?;

    for (provider, session_id) in indexed_snapshots.keys() {
        if !discovered_session_ids.contains(&(*provider, session_id.clone())) {
            delete_indexed_session(&transaction, project_id, *provider, session_id)?;
        }
    }

    let mut sessions_succeeded = 0;
    let mut skipped_rollouts = Vec::new();
    let total_sessions = archived_rollouts.len();
    for (index, archived_group) in archived_rollouts.iter().enumerate() {
        let group_outcome = update_archived_rollout_group(
            &mut transaction,
            project_id,
            indexed_snapshots.get(&archived_group.session_key()),
            archived_group,
        )?;
        sessions_succeeded += usize::from(group_outcome.session_succeeded);
        skipped_rollouts.extend(group_outcome.skipped_rollouts);
        let indexed_sessions = index + 1;
        if should_emit_session_progress(indexed_sessions, total_sessions) {
            progress(IndexProgress::IndexingSessions {
                indexed_sessions,
                total_sessions,
            });
        }
    }

    transaction
        .commit()
        .context("failed to commit SQLite index transaction")?;

    Ok(IndexedIndexOutcome {
        sessions_succeeded,
        sessions_currently_indexed: project_session_count(connection, project_id, &provider_set)?,
        skipped_rollouts,
        turns_currently_indexed: project_turn_count(connection, project_id, &provider_set)?,
    })
}

/// Returns whether one session-count progress event should be emitted.
fn should_emit_session_progress(current: usize, total: usize) -> bool {
    current >= total || current.is_multiple_of(SESSION_PROGRESS_EMIT_INTERVAL)
}

/// Describes how one archived duplicate group affected the SQLite index.
#[derive(Debug, Clone)]
struct ArchivedRolloutGroupOutcome {
    session_succeeded: bool,
    skipped_rollouts: Vec<SkippedRollout>,
}

/// Parses the first valid archived duplicate for one logical provider session id.
fn update_archived_rollout_group(
    transaction: &mut Transaction<'_>,
    project_id: &str,
    indexed_snapshot: Option<&IndexedSessionSnapshot>,
    archived_group: &ArchivedRolloutGroup,
) -> Result<ArchivedRolloutGroupOutcome> {
    let mut skipped_rollouts = Vec::new();

    for archived in &archived_group.candidates {
        if indexed_snapshot.is_some_and(|snapshot| snapshot.matches_candidate(archived)) {
            return Ok(ArchivedRolloutGroupOutcome {
                session_succeeded: true,
                skipped_rollouts,
            });
        }

        let savepoint = transaction
            .savepoint()
            .with_context(|| format!("failed to begin savepoint for {}", archived.session_id))?;
        match index_archived_rollout_candidate(&savepoint, project_id, archived) {
            Ok(()) => {
                savepoint.commit().with_context(|| {
                    format!(
                        "failed to commit archived {} savepoint for {}",
                        archived.provider.title(),
                        archived.source_path.display()
                    )
                })?;
                return Ok(ArchivedRolloutGroupOutcome {
                    session_succeeded: true,
                    skipped_rollouts,
                });
            }
            Err(CandidateIndexError::Parse(error)) => {
                skipped_rollouts.push(skipped_archived_rollout_candidate(archived, &error));
            }
            Err(CandidateIndexError::Infrastructure(error)) => return Err(error),
        }
    }

    Ok(ArchivedRolloutGroupOutcome {
        session_succeeded: false,
        skipped_rollouts,
    })
}

/// Replaces one indexed session with one fully parsed archived rollout candidate.
fn index_archived_rollout_candidate(
    connection: &Connection,
    project_id: &str,
    archived: &ArchivedRolloutCandidate,
) -> std::result::Result<(), CandidateIndexError> {
    let share_state = local_session_share_state(
        connection,
        project_id,
        archived.provider,
        &archived.session_id,
    )?;
    delete_indexed_session(
        connection,
        project_id,
        archived.provider,
        &archived.session_id,
    )?;

    match archived.provider {
        SourceKind::Codex => {
            let mut writer = SqliteSessionWriter::new(connection, project_id, archived);
            parse_codex_rollout_file_into(&archived.source_path, &mut writer)
                .map_err(|error| classify_codex_parse_into_error(&archived.source_path, error))?;
        }
        SourceKind::Claude => {
            let rollout = parse_archived_claude_rollout(archived)?;
            let mut writer = SqliteSessionWriter::new(connection, project_id, archived);
            writer.write_rollout(rollout)?;
        }
    }
    if let Some(share_state) = share_state {
        restore_local_session_share_state(
            connection,
            project_id,
            archived.provider,
            &archived.session_id,
            &share_state,
        )?;
    }
    Ok(())
}

/// Reads an explicit local share state before replacing one indexed session.
fn local_session_share_state(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
) -> Result<Option<String>> {
    connection
        .query_row(
            "
            SELECT share_state
            FROM sessions
            WHERE project_id = ?1
                AND provider = ?2
                AND session_id = ?3
                AND origin_kind = 'local'
                AND share_state <> 'unset'
            ",
            params![project_id, provider.directory_name(), session_id],
            |row| row.get(0),
        )
        .optional()
        .context("failed to read existing session share state")
}

/// Restores an explicit local share state after replacing one indexed session.
fn restore_local_session_share_state(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
    share_state: &str,
) -> Result<()> {
    connection
        .execute(
            "
            UPDATE sessions
            SET share_state = ?1
            WHERE project_id = ?2
                AND provider = ?3
                AND session_id = ?4
                AND origin_kind = 'local'
            ",
            params![
                share_state,
                project_id,
                provider.directory_name(),
                session_id
            ],
        )
        .context("failed to restore session share state")?;
    Ok(())
}

/// Parses one archived Claude rollout candidate into the normalized indexing model.
fn parse_archived_claude_rollout(
    archived: &ArchivedRolloutCandidate,
) -> std::result::Result<IndexedRollout, CandidateIndexError> {
    let rollout = parse_claude_rollout_file(
        &archived.source_path,
        &ClaudeArchivedContext {
            session_id: archived.session_id.clone(),
            parent_session_id: archived.parent_session_id.clone(),
            session_kind: match archived.session_kind {
                IndexedSessionKind::Primary => ClaudeSessionKind::Primary,
                IndexedSessionKind::Subagent => ClaudeSessionKind::Subagent,
            },
            expected_rollout_session_id: archived.rollout_session_id.clone(),
            expected_agent_id: archived.agent_id.clone(),
        },
    )
    .map_err(|error| classify_claude_parse_error(&archived.source_path, error))?;
    Ok(IndexedRollout {
        session_id: rollout.session_id,
        cwd: rollout.cwd,
        cli_version: rollout.cli_version,
        schema_id: rollout.schema_id,
        determinism: rollout.determinism,
        turns: rollout.turns,
    })
}

/// Classifies one streaming Codex parse failure as skippable or hard.
fn classify_codex_parse_into_error(
    source_path: &Path,
    error: ParseIntoError<SqliteSessionSinkError>,
) -> CandidateIndexError {
    match error {
        ParseIntoError::Parse(error) => classify_codex_parse_error(source_path, error),
        ParseIntoError::Sink(error) => CandidateIndexError::Infrastructure(error.into_inner()),
    }
}

/// Classifies one Codex parse failure as skippable or hard.
fn classify_codex_parse_error(source_path: &Path, error: CodexError) -> CandidateIndexError {
    if codex_error_is_skippable(&error) {
        CandidateParseError::Codex {
            path: source_path.to_path_buf(),
            source: Box::new(error),
        }
        .into()
    } else {
        CandidateIndexError::Infrastructure(error.into())
    }
}

/// Returns whether one Codex parse failure should be reported as a skipped rollout.
fn codex_error_is_skippable(error: &CodexError) -> bool {
    matches!(
        error,
        CodexError::MissingSessionMetaLine { .. }
            | CodexError::DecodeFirstLine { .. }
            | CodexError::DeserializeHeaderJson { .. }
            | CodexError::DeserializeJsonLine { .. }
            | CodexError::MismatchedSessionIds { .. }
            | CodexError::MissingCliVersion { .. }
            | CodexError::ResolveSchema { .. }
            | CodexError::ParseCliVersion { .. }
            | CodexError::UnsupportedFeature { .. }
            | CodexError::UnsupportedResponseItem { .. }
            | CodexError::UnsupportedRolloutItem { .. }
            | CodexError::UnsupportedMessageContentShape { .. }
            | CodexError::UnsupportedFieldShape { .. }
            | CodexError::UnsupportedToolOutputShape { .. }
    ) || matches!(
        error,
        CodexError::ReadLine { source, .. } if source.kind() == std::io::ErrorKind::InvalidData
    )
}

/// Classifies one Claude parse failure as skippable or hard.
fn classify_claude_parse_error(source_path: &Path, error: ClaudeError) -> CandidateIndexError {
    if claude_error_is_skippable(&error) {
        CandidateParseError::Claude {
            path: source_path.to_path_buf(),
            source: Box::new(error),
        }
        .into()
    } else {
        CandidateIndexError::Infrastructure(error.into())
    }
}

/// Returns whether one Claude parse failure should be reported as a skipped rollout.
fn claude_error_is_skippable(error: &ClaudeError) -> bool {
    matches!(
        error,
        ClaudeError::ParseJsonLine { .. }
            | ClaudeError::JsonLineNotObject { .. }
            | ClaudeError::MissingCwdMetadata { .. }
            | ClaudeError::MismatchedSessionId { .. }
            | ClaudeError::MismatchedAgentId { .. }
            | ClaudeError::MissingUserMessageObject { .. }
            | ClaudeError::MissingAssistantMessageObject { .. }
    ) || matches!(
        error,
        ClaudeError::ReadLine { source, .. } if source.kind() == std::io::ErrorKind::InvalidData
    )
}

/// Builds the skip report for one archived rollout candidate that failed to parse.
fn skipped_archived_rollout_candidate(
    archived: &ArchivedRolloutCandidate,
    error: &CandidateParseError,
) -> SkippedRollout {
    build_skipped_rollout(
        archived.provider,
        &archived.source_path,
        Some(archived.session_id.clone()),
        archived.cli_version.clone(),
        format_error_chain(error),
    )
}

/// Formats one error chain as a concise skip reason.
fn format_error_chain(error: &(dyn StdError + 'static)) -> String {
    let mut messages = Vec::new();
    let mut current = Some(error);
    while let Some(cause) = current {
        messages.push(cause.to_string());
        current = cause.source();
    }
    messages.join(": ")
}

/// Loads the indexed session snapshots used to detect unchanged archived rollouts.
fn load_indexed_session_snapshots(
    connection: &Connection,
    project_id: &str,
    providers: &BTreeSet<SourceKind>,
) -> Result<BTreeMap<IndexedSessionKey, IndexedSessionSnapshot>> {
    let mut statement = connection
        .prepare(
            "
            SELECT provider, session_id, archive_path, determinism, source_size, source_mtime_ms
            FROM sessions
            WHERE project_id = ?1
                AND origin_kind = 'local'
            ",
        )
        .context("failed to prepare indexed session snapshot query")?;
    let mut rows = statement
        .query(params![project_id])
        .context("failed to query indexed session snapshots")?;
    let mut snapshots = BTreeMap::new();

    while let Some(row) = rows
        .next()
        .context("failed to read indexed session snapshot row")?
    {
        let provider = source_kind_from_sql_text(
            &row.get::<_, String>(0)
                .context("failed to read indexed provider")?,
        )?;
        if !providers.contains(&provider) {
            continue;
        }

        let session_id: String = row.get(1).context("failed to read indexed session id")?;
        let archive_path: String = row.get(2).context("failed to read indexed archive path")?;
        let determinism: Option<String> =
            row.get(3).context("failed to read indexed determinism")?;
        let source_size = optional_sql_i64_to_u64(
            row.get(4).context("failed to read indexed source_size")?,
            "source_size",
        )?;
        let source_mtime_ms = optional_sql_i64_to_u64(
            row.get(5)
                .context("failed to read indexed source_mtime_ms")?,
            "source_mtime_ms",
        )?;

        snapshots.insert(
            (provider, session_id),
            IndexedSessionSnapshot {
                archive_path,
                determinism,
                source_size,
                source_mtime_ms,
            },
        );
    }

    Ok(snapshots)
}

/// Deletes one indexed session and cascades any stored turns.
fn delete_indexed_session(
    connection: &Connection,
    project_id: &str,
    provider: SourceKind,
    session_id: &str,
) -> Result<()> {
    connection
        .execute(
            "
            DELETE FROM sessions
            WHERE project_id = ?1
                AND provider = ?2
                AND session_id = ?3
            ",
            params![project_id, provider.directory_name(), session_id],
        )
        .with_context(|| {
            format!(
                "failed to delete indexed {} session {}",
                provider.title(),
                session_id
            )
        })?;
    Ok(())
}

/// Returns the total number of indexed sessions for one project after parsing completes.
fn project_session_count(
    connection: &Connection,
    project_id: &str,
    providers: &BTreeSet<SourceKind>,
) -> Result<usize> {
    let mut total = 0usize;
    for provider in providers {
        let sessions_indexed: i64 = connection
            .query_row(
                "
                SELECT COUNT(*)
                FROM sessions
                WHERE project_id = ?1
                    AND provider = ?2
                    AND origin_kind = 'local'
                ",
                params![project_id, provider.directory_name()],
                |row| row.get(0),
            )
            .with_context(|| format!("failed to count indexed {} sessions", provider.title()))?;
        total += usize::try_from(sessions_indexed)
            .context("indexed session count exceeds usize range")?;
    }
    Ok(total)
}

/// Returns the total number of indexed turns for one project after parsing completes.
fn project_turn_count(
    connection: &Connection,
    project_id: &str,
    providers: &BTreeSet<SourceKind>,
) -> Result<usize> {
    let mut total = 0usize;
    for provider in providers {
        let turns_indexed: i64 = connection
            .query_row(
                "
                SELECT COUNT(*)
                FROM turns
                JOIN sessions
                    ON sessions.project_id = turns.project_id
                    AND sessions.provider = turns.provider
                    AND sessions.session_id = turns.session_id
                WHERE turns.project_id = ?1
                    AND turns.provider = ?2
                    AND sessions.origin_kind = 'local'
                ",
                params![project_id, provider.directory_name()],
                |row| row.get(0),
            )
            .with_context(|| format!("failed to count indexed {} turns", provider.title()))?;
        total +=
            usize::try_from(turns_indexed).context("indexed turn count exceeds usize range")?;
    }
    Ok(total)
}

/// Converts one nullable SQLite integer into an unsigned snapshot value.
fn optional_sql_i64_to_u64(value: Option<i64>, column_name: &str) -> Result<Option<u64>> {
    value
        .map(|value| {
            u64::try_from(value)
                .with_context(|| format!("indexed `{column_name}` value {value} is negative"))
        })
        .transpose()
}

#[cfg(test)]
mod classification_tests {
    use std::{
        env, fs, io,
        time::{SystemTime, UNIX_EPOCH},
    };

    use darc_store::open_index_database_writer;
    use rusqlite::params;
    use serde_json::Value;

    use super::*;

    /// Builds one reusable JSON error for classification tests.
    fn json_error() -> serde_json::Error {
        serde_json::from_str::<Value>("{").expect_err("invalid JSON fixture")
    }

    /// Builds one unique temporary database path for engine tests.
    fn test_db_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("darc-{prefix}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("failed to create test directory");
        dir.join("index.sqlite")
    }

    /// Builds one archived candidate for snapshot matching tests.
    fn archived_candidate(
        provider: SourceKind,
        cli_version: Option<&str>,
    ) -> ArchivedRolloutCandidate {
        ArchivedRolloutCandidate {
            provider,
            source_path: PathBuf::from("/tmp/repo/archive.jsonl"),
            archive_path: "codex/archive.jsonl".to_owned(),
            session_id: "session-1".to_owned(),
            parent_session_id: None,
            rollout_session_id: "session-1".to_owned(),
            agent_id: None,
            session_kind: IndexedSessionKind::Primary,
            cli_version: cli_version.map(str::to_owned),
            size: 100,
            mtime_ms: 200,
        }
    }

    #[test]
    fn delete_indexed_session_removes_shared_rows_for_local_promotion() {
        let connection =
            open_index_database_writer(test_db_path("promote-shared").as_path()).unwrap();
        connection
            .execute(
                "
                INSERT INTO sessions (
                    project_id,
                    provider,
                    session_id,
                    session_kind,
                    archive_path,
                    cwd,
                    origin_kind,
                    origin_user_id,
                    origin_remote,
                    imported_at
                ) VALUES (?1, ?2, ?3, 'primary', ?4, ?5, 'shared', 'usr-remote', 'origin:darc/team', '2026-05-15T00:00:00Z')
                ",
                params![
                    "repo",
                    SourceKind::Codex.directory_name(),
                    "session-promote",
                    "shared://origin/usr-remote/codex/session-promote",
                    "/tmp/repo",
                ],
            )
            .unwrap();

        delete_indexed_session(&connection, "repo", SourceKind::Codex, "session-promote").unwrap();

        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    /// Builds one indexed snapshot for snapshot matching tests.
    fn indexed_snapshot(determinism: ParseDeterminism) -> IndexedSessionSnapshot {
        IndexedSessionSnapshot {
            archive_path: "codex/archive.jsonl".to_owned(),
            determinism: Some(determinism.as_sql_text().to_owned()),
            source_size: Some(100),
            source_mtime_ms: Some(200),
        }
    }

    #[test]
    fn snapshot_matching_reindexes_codex_when_determinism_becomes_exact() {
        let candidate = archived_candidate(SourceKind::Codex, Some("0.128.0"));

        assert!(
            !indexed_snapshot(ParseDeterminism::BestEffortForward).matches_candidate(&candidate)
        );
        assert!(indexed_snapshot(ParseDeterminism::Exact).matches_candidate(&candidate));
    }

    #[test]
    fn snapshot_matching_keeps_claude_best_effort_versions_current() {
        let candidate = archived_candidate(SourceKind::Claude, Some("2.1.125"));

        assert!(
            indexed_snapshot(ParseDeterminism::BestEffortForward).matches_candidate(&candidate)
        );
        assert!(!indexed_snapshot(ParseDeterminism::Exact).matches_candidate(&candidate));
    }

    #[test]
    fn snapshot_matching_keeps_claude_content_best_effort_current() {
        let candidate = archived_candidate(SourceKind::Claude, Some("2.1.126"));

        assert!(
            indexed_snapshot(ParseDeterminism::BestEffortForward).matches_candidate(&candidate)
        );
        assert!(indexed_snapshot(ParseDeterminism::Exact).matches_candidate(&candidate));
    }

    #[test]
    fn snapshot_matching_reindexes_missing_determinism() {
        let candidate = archived_candidate(SourceKind::Codex, Some("0.128.0"));
        let snapshot = IndexedSessionSnapshot {
            archive_path: "codex/archive.jsonl".to_owned(),
            determinism: None,
            source_size: Some(100),
            source_mtime_ms: Some(200),
        };

        assert!(!snapshot.matches_candidate(&candidate));
    }

    #[test]
    fn codex_skip_classification_is_an_explicit_allowlist() {
        let path = Path::new("/tmp/codex-rollout.jsonl");

        assert!(codex_error_is_skippable(&CodexError::DeserializeJsonLine {
            path: path.to_path_buf(),
            line_no: 7,
            context: "JSONL line",
            source: json_error(),
        }));
        assert!(!codex_error_is_skippable(&CodexError::SerializeJsonLine {
            path: path.to_path_buf(),
            line_no: 7,
            context: "normalized response item",
            source: json_error(),
        }));
        assert!(codex_error_is_skippable(&CodexError::ReadLine {
            path: path.to_path_buf(),
            line_no: 7,
            source: io::Error::new(io::ErrorKind::InvalidData, "invalid utf-8"),
        }));
        assert!(!codex_error_is_skippable(&CodexError::OpenFile {
            path: path.to_path_buf(),
            source: io::Error::other("permission denied"),
        }));
        assert!(!codex_error_is_skippable(&CodexError::ReadLine {
            path: path.to_path_buf(),
            line_no: 7,
            source: io::Error::other("permission denied"),
        }));
    }

    #[test]
    fn claude_skip_classification_is_an_explicit_allowlist() {
        let path = Path::new("/tmp/claude-rollout.jsonl");

        assert!(claude_error_is_skippable(&ClaudeError::ParseJsonLine {
            path: path.to_path_buf(),
            line_no: 4,
            source: json_error(),
        }));
        assert!(claude_error_is_skippable(&ClaudeError::ReadLine {
            path: path.to_path_buf(),
            line_no: 4,
            source: io::Error::new(io::ErrorKind::InvalidData, "invalid utf-8"),
        }));
        assert!(!claude_error_is_skippable(&ClaudeError::SerializeJson {
            context: "Claude assistant payload",
            source: json_error(),
        }));
        assert!(!claude_error_is_skippable(&ClaudeError::ReadLine {
            path: path.to_path_buf(),
            line_no: 4,
            source: io::Error::other("permission denied"),
        }));
    }

    #[test]
    fn codex_sink_errors_preserve_the_anyhow_chain() {
        let classified = classify_codex_parse_into_error(
            Path::new("/tmp/codex-rollout.jsonl"),
            ParseIntoError::Sink(SqliteSessionSinkError::from(
                anyhow::Error::new(io::Error::other("disk full"))
                    .context("failed to insert Codex turn"),
            )),
        );

        let CandidateIndexError::Infrastructure(error) = classified else {
            panic!("expected infrastructure error");
        };
        let messages: Vec<_> = error.chain().map(ToString::to_string).collect();

        assert_eq!(messages[0], "failed to insert Codex turn");
        assert!(messages.iter().any(|message| message.contains("disk full")));
        assert!(
            error
                .chain()
                .any(|cause| cause.downcast_ref::<io::Error>().is_some())
        );
    }
}
