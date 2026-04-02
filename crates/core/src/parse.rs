use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, Transaction, params};
use serde::Serialize;
use walkdir::WalkDir;

use crate::{
    SourceKind,
    active_project::load_active_project,
    constants::INDEX_DB_FILE_NAME,
    default_root_path,
    index_db::open_index_database,
    rollout::ParseDeterminism,
    rollout::codex::{
        CodexRolloutHeader, CodexRolloutSink, compare_rollout_priority, parse_rollout_file,
        parse_rollout_file_into, read_rollout_session_meta, reconcile_rollout_session_id,
    },
};

/// Parses one Codex rollout file into user-visible turns.
pub fn parse_codex_rollout(path: &Path) -> Result<CodexRollout> {
    parse_rollout_file(path)
}

/// Reports the results of indexing archived Codex turns for one project.
#[derive(Debug, Clone)]
pub struct ParseReport {
    pub project_name: String,
    pub project_root: PathBuf,
    pub codex_archive_root: PathBuf,
    pub index_db_path: PathBuf,
    pub sessions_indexed: usize,
    pub turns_indexed: usize,
}

/// Parses archived Codex rollouts for the active project into SQLite.
pub fn parse_project_codex_turns(root: Option<PathBuf>) -> Result<ParseReport> {
    let current_dir =
        env::current_dir().context("unable to resolve the current working directory")?;
    parse_project_codex_turns_from(&current_dir, root.unwrap_or_else(default_root_path))
}

/// Stores the parsed Codex dialogue for one rollout file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexRollout {
    pub session_id: String,
    pub cwd: PathBuf,
    pub cli_version: String,
    pub schema_id: String,
    pub determinism: ParseDeterminism,
    pub turns: Vec<CodexTurn>,
}

/// Stores one user turn and the assistant activity that followed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexTurn {
    pub turn_id: Option<String>,
    pub user_message: String,
    pub final_answer: Option<CodexTurnMessage>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub status: CodexTurnStatus,
    pub steps: Vec<CodexTurnStep>,
}

/// Stores one top-level assistant message attached to a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexTurnMessage {
    pub timestamp: String,
    pub text: String,
}

/// Tracks whether a parsed Codex turn finished normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexTurnStatus {
    Completed,
    Aborted,
    Incomplete,
}

impl CodexTurnStatus {
    /// Returns the stable SQLite string value for one turn status.
    fn as_sql_text(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Aborted => "aborted",
            Self::Incomplete => "incomplete",
        }
    }
}

/// Stores one ordered assistant-visible step inside a Codex turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CodexTurnStep {
    Reasoning {
        timestamp: String,
        summary: Vec<String>,
        encrypted: bool,
    },
    Commentary {
        timestamp: String,
        text: String,
    },
    ToolCall {
        timestamp: String,
        call_id: String,
        name: String,
        arguments: String,
    },
    ToolCallOutput {
        timestamp: String,
        call_id: String,
        output: String,
    },
    ProviderResponseItem {
        timestamp: String,
        item_type: String,
        payload_json: String,
    },
}

/// Stores one selected archived rollout before it is parsed and indexed.
#[derive(Debug, Clone)]
struct ArchivedCodexRolloutCandidate {
    source_path: PathBuf,
    archive_path: String,
    session_id: String,
    size: u64,
    mtime_ms: u64,
}

/// Stores the ordered archived rollout duplicates for one logical session id.
#[derive(Debug, Clone)]
struct ArchivedCodexRolloutGroup {
    candidates: Vec<ArchivedCodexRolloutCandidate>,
}

impl ArchivedCodexRolloutGroup {
    /// Returns the logical session id shared by one duplicate rollout group.
    fn session_id(&self) -> &str {
        self.candidates
            .first()
            .map(|candidate| candidate.session_id.as_str())
            .expect("archived rollout group should never be empty")
    }
}

/// Stores the indexed snapshot used to skip unchanged archived sessions.
#[derive(Debug, Clone)]
struct IndexedCodexSessionSnapshot {
    archive_path: String,
    source_size: Option<u64>,
    source_mtime_ms: Option<u64>,
}

impl IndexedCodexSessionSnapshot {
    /// Returns whether one archived rollout still matches the indexed session snapshot.
    fn matches_candidate(&self, candidate: &ArchivedCodexRolloutCandidate) -> bool {
        self.archive_path == candidate.archive_path
            && self.source_size == Some(candidate.size)
            && self.source_mtime_ms == Some(candidate.mtime_ms)
    }
}

/// Streams one parsed rollout directly into SQLite session and turn rows.
struct SqliteRolloutSink<'conn> {
    connection: &'conn Connection,
    project_id: String,
    archive_path: String,
    source_size: u64,
    source_mtime_ms: u64,
    session_id: Option<String>,
    turn_ordinal: i64,
}

impl<'conn> SqliteRolloutSink<'conn> {
    /// Creates one SQLite sink for a selected archived rollout candidate.
    fn new(
        connection: &'conn Connection,
        project_id: &str,
        archived: &ArchivedCodexRolloutCandidate,
    ) -> Self {
        Self {
            connection,
            project_id: project_id.to_owned(),
            archive_path: archived.archive_path.clone(),
            source_size: archived.size,
            source_mtime_ms: archived.mtime_ms,
            session_id: None,
            turn_ordinal: 0,
        }
    }
}

impl CodexRolloutSink for SqliteRolloutSink<'_> {
    fn begin_rollout(&mut self, header: &CodexRolloutHeader) -> Result<()> {
        let source_size = i64::try_from(self.source_size)
            .context("Codex source_size exceeds SQLite INTEGER range")?;
        let source_mtime_ms = i64::try_from(self.source_mtime_ms)
            .context("Codex source_mtime_ms exceeds SQLite INTEGER range")?;

        self.connection
            .execute(
                "
                INSERT INTO codex_sessions (
                    project_id,
                    session_id,
                    archive_path,
                    cwd,
                    cli_version,
                    schema_id,
                    determinism,
                    source_size,
                    source_mtime_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ",
                params![
                    self.project_id.as_str(),
                    header.session_id.as_str(),
                    self.archive_path.as_str(),
                    header.cwd.to_string_lossy(),
                    header.cli_version.as_str(),
                    header.schema_id.as_str(),
                    header.determinism.as_sql_text(),
                    source_size,
                    source_mtime_ms,
                ],
            )
            .with_context(|| format!("failed to insert Codex session {}", header.session_id))?;
        self.session_id = Some(header.session_id.clone());
        self.turn_ordinal = 0;
        Ok(())
    }

    fn push_turn(&mut self, turn: CodexTurn) -> Result<()> {
        let session_id = self
            .session_id
            .as_deref()
            .context("missing active Codex session id while inserting turn")?;
        let steps_json =
            serde_json::to_string(&turn.steps).context("failed to serialize Codex turn steps")?;
        let final_answer_at = turn.final_answer.as_ref().map(|message| &message.timestamp);
        let final_answer_text = turn.final_answer.as_ref().map(|message| &message.text);

        self.connection
            .execute(
                "
                INSERT INTO codex_turns (
                    project_id,
                    session_id,
                    turn_ordinal,
                    turn_id,
                    started_at,
                    completed_at,
                    status,
                    user_message,
                    final_answer_at,
                    final_answer_text,
                    steps_json
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                ",
                params![
                    self.project_id.as_str(),
                    session_id,
                    self.turn_ordinal,
                    turn.turn_id,
                    turn.started_at,
                    turn.completed_at,
                    turn.status.as_sql_text(),
                    turn.user_message,
                    final_answer_at,
                    final_answer_text,
                    steps_json,
                ],
            )
            .with_context(|| {
                format!(
                    "failed to insert Codex turn {} for session {}",
                    self.turn_ordinal, session_id
                )
            })?;
        self.turn_ordinal += 1;

        Ok(())
    }
}

/// Parses archived Codex rollouts for one explicit current directory and memstack root.
fn parse_project_codex_turns_from(current_dir: &Path, root: PathBuf) -> Result<ParseReport> {
    let active_project = load_active_project(current_dir, &root)?;
    let codex_archive_root = active_project
        .project
        .sessions_root
        .join(SourceKind::Codex.directory_name());
    let archived_rollouts = discover_archived_codex_rollouts(
        &codex_archive_root,
        &active_project.project.sessions_root,
    )?;
    let index_db_path = root.join(INDEX_DB_FILE_NAME);
    let mut connection = open_index_database(&index_db_path)?;

    let turns_indexed = update_project_codex_turns(
        &mut connection,
        &active_project.project.id,
        &archived_rollouts,
    )?;

    Ok(ParseReport {
        project_name: active_project.project.name,
        project_root: active_project.current_root,
        codex_archive_root,
        index_db_path,
        sessions_indexed: archived_rollouts.len(),
        turns_indexed,
    })
}

/// Discovers and deduplicates archived Codex rollout files below one project archive root.
fn discover_archived_codex_rollouts(
    root: &Path,
    sessions_root: &Path,
) -> Result<Vec<ArchivedCodexRolloutGroup>> {
    let rollout_paths = discover_archived_codex_rollout_paths(root)?;
    let rollout_candidates = rollout_paths
        .iter()
        .map(|path| inspect_archived_codex_rollout(path, sessions_root))
        .collect::<Result<Vec<_>>>()?;
    Ok(group_archived_codex_rollouts(rollout_candidates))
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
) -> Result<ArchivedCodexRolloutCandidate> {
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
    let session_id = archived_codex_rollout_session_id(path)?;

    Ok(ArchivedCodexRolloutCandidate {
        source_path: path.to_path_buf(),
        archive_path,
        session_id,
        size,
        mtime_ms,
    })
}

/// Reads the logical session id used to group archived rollout duplicates.
fn archived_codex_rollout_session_id(path: &Path) -> Result<String> {
    let file_name = path.file_name().and_then(|name| name.to_str());
    let payload_session_id = match read_rollout_session_meta(path) {
        Ok(Some(meta)) => Some(meta.session_id),
        Ok(None) => None,
        Err(_)
            if file_name
                .and_then(crate::rollout::codex::parse_rollout_file_session_id)
                .is_some() =>
        {
            None
        }
        Err(error) => return Err(error),
    };

    reconcile_rollout_session_id(path, file_name, payload_session_id.as_deref())?.with_context(
        || {
            format!(
                "failed to derive archived Codex session id from {}",
                path.display()
            )
        },
    )
}

/// Groups archived rollout duplicates by session id and orders each group by priority.
fn group_archived_codex_rollouts(
    rollout_candidates: Vec<ArchivedCodexRolloutCandidate>,
) -> Vec<ArchivedCodexRolloutGroup> {
    let mut grouped_rollouts = BTreeMap::<String, Vec<ArchivedCodexRolloutCandidate>>::new();

    for candidate in rollout_candidates {
        grouped_rollouts
            .entry(candidate.session_id.clone())
            .or_default()
            .push(candidate);
    }

    grouped_rollouts
        .into_values()
        .map(|mut candidates| {
            candidates.sort_by(|left, right| {
                compare_archived_codex_rollout_candidates(left, right).reverse()
            });
            ArchivedCodexRolloutGroup { candidates }
        })
        .collect()
}

/// Compares two archived rollout candidates using the shared duplicate priority rules.
fn compare_archived_codex_rollout_candidates(
    left: &ArchivedCodexRolloutCandidate,
    right: &ArchivedCodexRolloutCandidate,
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
fn file_snapshot(path: &Path) -> Result<(u64, u64)> {
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

/// Updates one project's indexed Codex sessions and turns inside SQLite.
fn update_project_codex_turns(
    connection: &mut Connection,
    project_id: &str,
    archived_rollouts: &[ArchivedCodexRolloutGroup],
) -> Result<usize> {
    let mut transaction = connection
        .transaction()
        .context("failed to begin SQLite transaction")?;
    let indexed_snapshots = load_indexed_codex_session_snapshots(&transaction, project_id)?;
    let archived_session_ids = archived_rollouts
        .iter()
        .map(|archived| archived.session_id().to_owned())
        .collect::<BTreeSet<_>>();

    for session_id in indexed_snapshots.keys() {
        if !archived_session_ids.contains(session_id) {
            delete_indexed_codex_session(&transaction, project_id, session_id)?;
        }
    }

    for archived_group in archived_rollouts {
        update_archived_codex_rollout_group(
            &mut transaction,
            project_id,
            indexed_snapshots.get(archived_group.session_id()),
            archived_group,
        )?;
    }

    transaction
        .commit()
        .context("failed to commit SQLite parse transaction")?;

    project_codex_turn_count(connection, project_id)
}

/// Parses the first valid archived duplicate for one logical Codex session id.
fn update_archived_codex_rollout_group(
    transaction: &mut Transaction<'_>,
    project_id: &str,
    indexed_snapshot: Option<&IndexedCodexSessionSnapshot>,
    archived_group: &ArchivedCodexRolloutGroup,
) -> Result<()> {
    let mut last_error = None;

    for archived in &archived_group.candidates {
        if indexed_snapshot.is_some_and(|snapshot| snapshot.matches_candidate(archived)) {
            return Ok(());
        }

        let savepoint = transaction
            .savepoint()
            .with_context(|| format!("failed to begin savepoint for {}", archived.session_id))?;
        match index_archived_codex_rollout_candidate(&savepoint, project_id, archived) {
            Ok(()) => {
                savepoint.commit().with_context(|| {
                    format!(
                        "failed to commit archived Codex savepoint for {}",
                        archived.source_path.display()
                    )
                })?;
                return Ok(());
            }
            Err(error) => {
                last_error = Some(error);
            }
        }
    }

    if let Some(error) = last_error {
        return Err(error);
    }

    anyhow::bail!(
        "missing archived Codex rollout candidates for session {}",
        archived_group.session_id()
    );
}

/// Deletes and reparses one archived rollout candidate into the SQLite index.
fn index_archived_codex_rollout_candidate(
    connection: &Connection,
    project_id: &str,
    archived: &ArchivedCodexRolloutCandidate,
) -> Result<()> {
    delete_indexed_codex_session(connection, project_id, &archived.session_id)?;
    let mut sink = SqliteRolloutSink::new(connection, project_id, archived);
    parse_rollout_file_into(&archived.source_path, &mut sink)
        .with_context(|| format!("failed to parse {}", archived.source_path.display()))
}

/// Loads the indexed session snapshots used to detect unchanged archived rollouts.
fn load_indexed_codex_session_snapshots(
    connection: &Connection,
    project_id: &str,
) -> Result<BTreeMap<String, IndexedCodexSessionSnapshot>> {
    let mut statement = connection
        .prepare(
            "
            SELECT session_id, archive_path, source_size, source_mtime_ms
            FROM codex_sessions
            WHERE project_id = ?1
            ",
        )
        .context("failed to prepare indexed Codex session snapshot query")?;
    let mut rows = statement
        .query(params![project_id])
        .context("failed to query indexed Codex session snapshots")?;
    let mut snapshots = BTreeMap::new();

    while let Some(row) = rows
        .next()
        .context("failed to read indexed Codex session snapshot row")?
    {
        let session_id: String = row
            .get(0)
            .context("failed to read indexed Codex session id")?;
        let archive_path: String = row
            .get(1)
            .context("failed to read indexed Codex archive path")?;
        let source_size = optional_sql_i64_to_u64(
            row.get(2)
                .context("failed to read indexed Codex source_size")?,
            "source_size",
        )?;
        let source_mtime_ms = optional_sql_i64_to_u64(
            row.get(3)
                .context("failed to read indexed Codex source_mtime_ms")?,
            "source_mtime_ms",
        )?;

        snapshots.insert(
            session_id,
            IndexedCodexSessionSnapshot {
                archive_path,
                source_size,
                source_mtime_ms,
            },
        );
    }

    Ok(snapshots)
}

/// Deletes one indexed Codex session and cascades any stored turns.
fn delete_indexed_codex_session(
    connection: &Connection,
    project_id: &str,
    session_id: &str,
) -> Result<()> {
    connection
        .execute(
            "
            DELETE FROM codex_sessions
            WHERE project_id = ?1 AND session_id = ?2
            ",
            params![project_id, session_id],
        )
        .with_context(|| format!("failed to delete indexed Codex session {session_id}"))?;
    Ok(())
}

/// Returns the total number of indexed Codex turns for one project after parsing completes.
fn project_codex_turn_count(connection: &Connection, project_id: &str) -> Result<usize> {
    let turns_indexed: i64 = connection
        .query_row(
            "
            SELECT COUNT(*)
            FROM codex_turns
            WHERE project_id = ?1
            ",
            params![project_id],
            |row| row.get(0),
        )
        .context("failed to count indexed Codex turns")?;
    usize::try_from(turns_indexed).context("indexed Codex turn count exceeds usize range")
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
mod tests {
    use std::{
        env, fs,
        io::Cursor,
        path::{Path, PathBuf},
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };

    use anyhow::{Context, Result};
    use rusqlite::Connection;
    use serde_json::Value;

    use super::{
        CodexRollout, CodexTurnMessage, CodexTurnStatus, CodexTurnStep,
        parse_project_codex_turns_from,
    };
    use crate::config::{ProjectConfig, SharedConfig, SourcesConfig};
    use crate::constants::{CONFIG_FILE_NAME, INDEX_DB_FILE_NAME};
    use crate::rollout::{ParseDeterminism, codex::parse_rollout_reader};

    #[test]
    fn parses_two_turns_with_event_boundaries() -> Result<()> {
        let rollout = parse_fixture(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-two-turns","cwd":"/tmp/repo","cli_version":"0.118.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}
{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"duplicate"}]}}
{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"user_message","message":"First task"}}
{"timestamp":"2026-01-01T00:00:04Z","type":"event_msg","payload":{"type":"agent_message","phase":"commentary","message":"duplicate commentary"}}
{"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Checking files."}]}}
{"timestamp":"2026-01-01T00:00:06Z","type":"response_item","payload":{"type":"reasoning","summary":["scan"],"encrypted_content":"secret"}}
{"timestamp":"2026-01-01T00:00:07Z","type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"exec_command","arguments":"{\"cmd\":\"ls\"}"}}
{"timestamp":"2026-01-01T00:00:08Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"ok"}}
{"timestamp":"2026-01-01T00:00:09Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"First reply"}]}}
{"timestamp":"2026-01-01T00:00:10Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}
{"timestamp":"2026-01-01T00:00:11Z","type":"event_msg","payload":{"type":"user_message","message":"Second task"}}
{"timestamp":"2026-01-01T00:00:12Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Second reply"}]}}
"#,
        )?;

        assert_eq!(
            rollout,
            CodexRollout {
                session_id: "fixture-two-turns".to_owned(),
                cwd: Path::new("/tmp/repo").to_path_buf(),
                cli_version: "0.118.0".to_owned(),
                schema_id: "codex.turn_lifecycle".to_owned(),
                determinism: ParseDeterminism::Exact,
                turns: vec![
                    super::CodexTurn {
                        turn_id: Some("turn-1".to_owned()),
                        user_message: "First task".to_owned(),
                        final_answer: Some(CodexTurnMessage {
                            timestamp: "2026-01-01T00:00:09Z".to_owned(),
                            text: "First reply".to_owned(),
                        }),
                        started_at: "2026-01-01T00:00:03Z".to_owned(),
                        completed_at: Some("2026-01-01T00:00:09Z".to_owned()),
                        status: CodexTurnStatus::Completed,
                        steps: vec![
                            CodexTurnStep::Commentary {
                                timestamp: "2026-01-01T00:00:05Z".to_owned(),
                                text: "Checking files.".to_owned(),
                            },
                            CodexTurnStep::Reasoning {
                                timestamp: "2026-01-01T00:00:06Z".to_owned(),
                                summary: vec!["scan".to_owned()],
                                encrypted: true,
                            },
                            CodexTurnStep::ToolCall {
                                timestamp: "2026-01-01T00:00:07Z".to_owned(),
                                call_id: "call-1".to_owned(),
                                name: "exec_command".to_owned(),
                                arguments: "{\"cmd\":\"ls\"}".to_owned(),
                            },
                            CodexTurnStep::ToolCallOutput {
                                timestamp: "2026-01-01T00:00:08Z".to_owned(),
                                call_id: "call-1".to_owned(),
                                output: "ok".to_owned(),
                            },
                        ],
                    },
                    super::CodexTurn {
                        turn_id: Some("turn-2".to_owned()),
                        user_message: "Second task".to_owned(),
                        final_answer: Some(CodexTurnMessage {
                            timestamp: "2026-01-01T00:00:12Z".to_owned(),
                            text: "Second reply".to_owned(),
                        }),
                        started_at: "2026-01-01T00:00:11Z".to_owned(),
                        completed_at: Some("2026-01-01T00:00:12Z".to_owned()),
                        status: CodexTurnStatus::Completed,
                        steps: vec![],
                    },
                ],
            }
        );

        Ok(())
    }

    #[test]
    fn falls_back_to_non_boilerplate_response_item_user_messages() -> Result<()> {
        let rollout = parse_fixture(
            r##"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-fallback","cwd":"/tmp/repo","cli_version":"0.118.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /tmp/repo"}]}}
{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>/tmp/repo</cwd>\n</environment_context>"}]}}
{"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Summarize the build output"}]}}
{"timestamp":"2026-01-01T00:00:04Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Build passed."}]}}
"##,
        )?;

        assert_eq!(rollout.turns.len(), 1);
        assert_eq!(rollout.turns[0].user_message, "Summarize the build output");
        assert_eq!(rollout.turns[0].status, CodexTurnStatus::Completed);
        assert_eq!(
            rollout.turns[0].final_answer,
            Some(CodexTurnMessage {
                timestamp: "2026-01-01T00:00:04Z".to_owned(),
                text: "Build passed.".to_owned(),
            })
        );

        Ok(())
    }

    #[test]
    fn uses_task_complete_when_no_final_answer_message_exists() -> Result<()> {
        let rollout = parse_fixture(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-complete","cwd":"/tmp/repo","cli_version":"0.118.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Run the checks"}}
{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Running checks."}]}}
{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Checks passed."}}
"#,
        )?;

        assert_eq!(rollout.turns.len(), 1);
        assert_eq!(rollout.turns[0].status, CodexTurnStatus::Completed);
        assert!(matches!(
            rollout.turns[0].final_answer.as_ref(),
            Some(CodexTurnMessage { text, .. }) if text == "Checks passed."
        ));

        Ok(())
    }

    #[test]
    fn marks_aborted_turns() -> Result<()> {
        let rollout = parse_fixture(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-aborted","cwd":"/tmp/repo","cli_version":"0.118.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Inspect the repo"}}
{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Reading files."}]}}
{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"turn_aborted","turn_id":"turn-1","reason":"interrupted"}}
"#,
        )?;

        assert_eq!(rollout.turns.len(), 1);
        assert_eq!(rollout.turns[0].status, CodexTurnStatus::Aborted);
        assert_eq!(
            rollout.turns[0].completed_at.as_deref(),
            Some("2026-01-01T00:00:03Z")
        );
        assert!(rollout.turns[0].final_answer.is_none());

        Ok(())
    }

    #[test]
    fn treats_legacy_unphased_assistant_messages_as_final_answers() -> Result<()> {
        let rollout = parse_fixture(
            r##"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-legacy-final","cwd":"/tmp/repo","cli_version":"0.118.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /tmp/repo"}]}}
{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"Legacy prompt"}}
{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"agent_message","message":"Legacy final reply"}}
{"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Legacy final reply"}]}}
"##,
        )?;

        assert_eq!(rollout.turns.len(), 1);
        assert_eq!(rollout.turns[0].status, CodexTurnStatus::Completed);
        assert_eq!(
            rollout.turns[0].final_answer,
            Some(CodexTurnMessage {
                timestamp: "2026-01-01T00:00:03Z".to_owned(),
                text: "Legacy final reply".to_owned(),
            })
        );

        Ok(())
    }

    #[test]
    fn parses_structured_tool_payloads_and_custom_tool_items() -> Result<()> {
        let rollout = parse_fixture(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"fixture-structured-tools","cwd":"/tmp/repo","cli_version":"0.118.0"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Inspect the rollout"}}
{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"screenshot","arguments":"{\"pageno\":0,\"mode\":\"page\"}"}}
{"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":[{"type":"input_image","image_url":"data:image/png;base64,abc"}]}}
{"timestamp":"2026-01-01T00:00:04Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"call-2","name":"apply_patch","input":"*** Begin Patch\n*** End Patch\n"}}
{"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-2","output":"{\"output\":\"Success\",\"metadata\":{\"exit_code\":0}}"}}
{"timestamp":"2026-01-01T00:00:06Z","type":"response_item","payload":{"type":"web_search_call","status":"completed","action":{"type":"open_page","url":"https://example.com"}}}
{"timestamp":"2026-01-01T00:00:07Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Parsed."}]}}
"#,
        )?;

        assert_eq!(rollout.turns.len(), 1);
        assert_eq!(rollout.turns[0].steps.len(), 5);

        let CodexTurnStep::ToolCall { arguments, .. } = &rollout.turns[0].steps[0] else {
            panic!("expected structured function_call step");
        };
        let arguments: Value = serde_json::from_str(arguments)?;
        assert_eq!(arguments["mode"], "page");
        assert_eq!(arguments["pageno"], 0);

        let CodexTurnStep::ToolCallOutput { output, .. } = &rollout.turns[0].steps[1] else {
            panic!("expected structured function_call_output step");
        };
        let output: Value = serde_json::from_str(output)?;
        assert_eq!(output[0]["type"], "input_image");

        let CodexTurnStep::ToolCall {
            name, arguments, ..
        } = &rollout.turns[0].steps[2]
        else {
            panic!("expected custom tool call step");
        };
        assert_eq!(name, "apply_patch");
        assert_eq!(arguments, "*** Begin Patch\n*** End Patch");

        let CodexTurnStep::ToolCallOutput { output, .. } = &rollout.turns[0].steps[3] else {
            panic!("expected custom tool output step");
        };
        let output: Value = serde_json::from_str(output)?;
        assert_eq!(output["output"], "Success");
        assert_eq!(output["metadata"]["exit_code"], 0);

        let CodexTurnStep::ProviderResponseItem {
            item_type,
            payload_json,
            ..
        } = &rollout.turns[0].steps[4]
        else {
            panic!("expected preserved provider response item");
        };
        assert_eq!(item_type, "web_search_call");
        let payload: Value = serde_json::from_str(payload_json)?;
        assert_eq!(payload["action"]["url"], "https://example.com");

        Ok(())
    }

    fn parse_fixture(input: &str) -> Result<CodexRollout> {
        parse_rollout_reader(Cursor::new(input), Path::new("fixture.jsonl"))
    }

    /// Builds a unique temporary directory for one parse test fixture.
    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("test-{prefix}-{}-{nanos}", std::process::id()))
    }

    /// Writes one text file while creating any missing parent directories.
    fn write_file(path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
        Ok(())
    }

    /// Sets one file's modified timestamp to a fixed value for snapshot-based parse tests.
    fn touch_file_timestamp(path: &Path, timestamp: &str) -> Result<()> {
        let status = Command::new("touch")
            .arg("-t")
            .arg(timestamp)
            .arg(path)
            .status()
            .with_context(|| format!("failed to run touch for {}", path.display()))?;
        if !status.success() {
            anyhow::bail!("touch -t {timestamp} failed for {}", path.display());
        }
        Ok(())
    }

    /// Writes a minimal shared config for one parse indexing test.
    fn write_parse_config(
        root: &Path,
        project_root: &Path,
        sessions_root: &Path,
    ) -> Result<String> {
        let project_id = "repo-abc123".to_owned();
        let config = SharedConfig::new(
            root.to_path_buf(),
            vec![ProjectConfig {
                id: project_id.clone(),
                name: "repo".into(),
                local_path: project_root.to_path_buf(),
                git_upstream: None,
                sessions_root: sessions_root.to_path_buf(),
                known_paths: Vec::new(),
            }],
            SourcesConfig::default(),
        );
        write_file(
            &root.join(CONFIG_FILE_NAME),
            &toml::to_string_pretty(&config)?,
        )?;
        Ok(project_id)
    }

    #[test]
    fn parse_project_indexes_codex_turns_into_sqlite() -> Result<()> {
        let memstack_root = unique_test_dir("parse-index");
        let project_root = memstack_root.join("repo");
        let sessions_root = memstack_root.join("projects/repo-abc123/sessions");
        let codex_root = sessions_root.join("codex");
        let index_db_path = memstack_root.join(INDEX_DB_FILE_NAME);
        let rollout_path = codex_root
            .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
        fs::create_dir_all(&project_root)?;
        write_parse_config(&memstack_root, &project_root, &sessions_root)?;

        write_file(
            &rollout_path,
            &format!(
                concat!(
                    "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"First task\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"First reply\"}}]}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:04Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-2\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:05Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Second task\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:06Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"commentary\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Checking\"}}]}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:07Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Second reply\"}}]}}}}\n"
                ),
                project_root.display()
            ),
        )?;
        let (source_size, source_mtime_ms) = super::file_snapshot(&rollout_path)?;

        let report = parse_project_codex_turns_from(&project_root, memstack_root.clone())?;

        assert_eq!(report.project_name, "repo");
        assert_eq!(report.project_root, fs::canonicalize(&project_root)?);
        assert_eq!(report.sessions_indexed, 1);
        assert_eq!(report.turns_indexed, 2);
        assert_eq!(report.index_db_path, index_db_path);

        let connection = Connection::open(&report.index_db_path)?;
        let indexed_sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM codex_sessions WHERE project_id = ?1",
            ["repo-abc123"],
            |row| row.get(0),
        )?;
        let indexed_turns: i64 = connection.query_row(
            "SELECT COUNT(*) FROM codex_turns WHERE project_id = ?1",
            ["repo-abc123"],
            |row| row.get(0),
        )?;
        let second_turn: (String, String) = connection.query_row(
            "
            SELECT user_message, final_answer_text
            FROM codex_turns
            WHERE project_id = ?1 AND session_id = ?2 AND turn_ordinal = 1
            ",
            ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let session_metadata: (String, String, String, i64, i64) = connection.query_row(
            "
            SELECT cli_version, schema_id, determinism, source_size, source_mtime_ms
            FROM codex_sessions
            WHERE project_id = ?1 AND session_id = ?2
            ",
            ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        assert_eq!(indexed_sessions, 1);
        assert_eq!(indexed_turns, 2);
        assert_eq!(second_turn.0, "Second task");
        assert_eq!(second_turn.1, "Second reply");
        assert_eq!(session_metadata.0, "0.118.0");
        assert_eq!(session_metadata.1, "codex.turn_lifecycle");
        assert_eq!(session_metadata.2, "exact");
        assert_eq!(u64::try_from(session_metadata.3)?, source_size);
        assert_eq!(u64::try_from(session_metadata.4)?, source_mtime_ms);

        Ok(())
    }

    #[test]
    fn parse_project_rewrites_existing_indexed_turns() -> Result<()> {
        let memstack_root = unique_test_dir("parse-rewrite");
        let project_root = memstack_root.join("repo");
        let sessions_root = memstack_root.join("projects/repo-abc123/sessions");
        let codex_root = sessions_root.join("codex");
        let rollout_path = codex_root
            .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
        fs::create_dir_all(&project_root)?;
        write_parse_config(&memstack_root, &project_root, &sessions_root)?;

        write_file(
            &rollout_path,
            &format!(
                concat!(
                    "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Original task\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Original reply\"}}]}}}}\n"
                ),
                project_root.display()
            ),
        )?;
        parse_project_codex_turns_from(&project_root, memstack_root.clone())?;

        write_file(
            &rollout_path,
            &format!(
                concat!(
                    "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Updated task\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Updated reply\"}}]}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:04Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-2\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:05Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Second task\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:06Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Second reply\"}}]}}}}\n"
                ),
                project_root.display()
            ),
        )?;

        let report = parse_project_codex_turns_from(&project_root, memstack_root.clone())?;
        let connection = Connection::open(memstack_root.join(INDEX_DB_FILE_NAME))?;
        let indexed_turns: i64 = connection.query_row(
            "SELECT COUNT(*) FROM codex_turns WHERE project_id = ?1",
            ["repo-abc123"],
            |row| row.get(0),
        )?;
        let first_turn: (String, String) = connection.query_row(
            "
            SELECT user_message, final_answer_text
            FROM codex_turns
            WHERE project_id = ?1 AND session_id = ?2 AND turn_ordinal = 0
            ",
            ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        assert_eq!(report.turns_indexed, 2);
        assert_eq!(indexed_turns, 2);
        assert_eq!(first_turn.0, "Updated task");
        assert_eq!(first_turn.1, "Updated reply");

        Ok(())
    }

    #[test]
    fn parse_project_skips_unchanged_sessions_when_snapshot_matches() -> Result<()> {
        let memstack_root = unique_test_dir("parse-skip-unchanged");
        let project_root = memstack_root.join("repo");
        let sessions_root = memstack_root.join("projects/repo-abc123/sessions");
        let codex_root = sessions_root.join("codex");
        let rollout_path = codex_root
            .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
        fs::create_dir_all(&project_root)?;
        write_parse_config(&memstack_root, &project_root, &sessions_root)?;

        let original = format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Original task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Original reply\"}}]}}}}\n"
            ),
            project_root.display()
        );
        write_file(&rollout_path, &original)?;
        touch_file_timestamp(&rollout_path, "202604011000.00")?;
        parse_project_codex_turns_from(&project_root, memstack_root.clone())?;

        write_file(&rollout_path, &"{".repeat(original.len()))?;
        touch_file_timestamp(&rollout_path, "202604011000.00")?;

        let report = parse_project_codex_turns_from(&project_root, memstack_root.clone())?;
        let connection = Connection::open(memstack_root.join(INDEX_DB_FILE_NAME))?;
        let indexed_turn: (String, String) = connection.query_row(
            "
            SELECT user_message, final_answer_text
            FROM codex_turns
            WHERE project_id = ?1 AND session_id = ?2 AND turn_ordinal = 0
            ",
            ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        assert_eq!(report.sessions_indexed, 1);
        assert_eq!(report.turns_indexed, 1);
        assert_eq!(indexed_turn.0, "Original task");
        assert_eq!(indexed_turn.1, "Original reply");

        Ok(())
    }

    #[test]
    fn parse_project_deduplicates_archived_rollouts_with_same_session_id() -> Result<()> {
        let memstack_root = unique_test_dir("parse-deduplicate");
        let project_root = memstack_root.join("repo");
        let sessions_root = memstack_root.join("projects/repo-abc123/sessions");
        let codex_root = sessions_root.join("codex");
        fs::create_dir_all(&project_root)?;
        write_parse_config(&memstack_root, &project_root, &sessions_root)?;

        write_file(
            &codex_root
                .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
            &format!(
                concat!(
                    "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Stale task\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Stale reply\"}}]}}}}\n"
                ),
                project_root.display()
            ),
        )?;
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_file(
            &codex_root
                .join("rollout-2026-04-01T11-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
            &format!(
                concat!(
                    "{{\"timestamp\":\"2026-04-01T11:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T11:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T11:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Fresh task\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T11:00:03Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"commentary\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Checking\"}}]}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T11:00:04Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Fresh reply\"}}]}}}}\n"
                ),
                project_root.display()
            ),
        )?;

        let report = parse_project_codex_turns_from(&project_root, memstack_root.clone())?;
        let connection = Connection::open(memstack_root.join(INDEX_DB_FILE_NAME))?;
        let indexed_sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM codex_sessions WHERE project_id = ?1",
            ["repo-abc123"],
            |row| row.get(0),
        )?;
        let indexed_turns: i64 = connection.query_row(
            "SELECT COUNT(*) FROM codex_turns WHERE project_id = ?1",
            ["repo-abc123"],
            |row| row.get(0),
        )?;
        let indexed_row: (String, String) = connection.query_row(
            "
            SELECT archive_path, user_message
            FROM codex_sessions s
            JOIN codex_turns t
              ON t.project_id = s.project_id
             AND t.session_id = s.session_id
             AND t.turn_ordinal = 0
            WHERE s.project_id = ?1
            ",
            ["repo-abc123"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        assert_eq!(report.sessions_indexed, 1);
        assert_eq!(report.turns_indexed, 1);
        assert_eq!(indexed_sessions, 1);
        assert_eq!(indexed_turns, 1);
        assert_eq!(
            indexed_row.0,
            "codex/rollout-2026-04-01T11-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"
        );
        assert_eq!(indexed_row.1, "Fresh task");

        Ok(())
    }

    #[test]
    fn parse_project_rejects_mismatched_filename_and_payload_session_ids() -> Result<()> {
        let memstack_root = unique_test_dir("parse-id-mismatch");
        let project_root = memstack_root.join("repo");
        let sessions_root = memstack_root.join("projects/repo-abc123/sessions");
        let codex_root = sessions_root.join("codex");
        fs::create_dir_all(&project_root)?;
        write_parse_config(&memstack_root, &project_root, &sessions_root)?;

        write_file(
            &codex_root
                .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
            &format!(
                concat!(
                    "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e40\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Task\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Reply\"}}]}}}}\n"
                ),
                project_root.display()
            ),
        )?;

        let error =
            parse_project_codex_turns_from(&project_root, memstack_root).expect_err("mismatch");

        assert!(error.to_string().contains("mismatched Codex session ids"));

        Ok(())
    }

    #[test]
    fn parse_project_ignores_corrupt_losing_duplicate() -> Result<()> {
        let memstack_root = unique_test_dir("parse-corrupt-loser");
        let project_root = memstack_root.join("repo");
        let sessions_root = memstack_root.join("projects/repo-abc123/sessions");
        let codex_root = sessions_root.join("codex");
        fs::create_dir_all(&project_root)?;
        write_parse_config(&memstack_root, &project_root, &sessions_root)?;

        write_file(
            &codex_root
                .join("rollout-2026-04-01T09-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
            "{not-json\n",
        )?;
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_file(
            &codex_root
                .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
            &format!(
                concat!(
                    "{{\"timestamp\":\"2026-04-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Fresh task\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T10:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Fresh reply\"}}]}}}}\n"
                ),
                project_root.display()
            ),
        )?;

        let report = parse_project_codex_turns_from(&project_root, memstack_root.clone())?;
        let connection = Connection::open(memstack_root.join(INDEX_DB_FILE_NAME))?;
        let indexed_turn: (String, String) = connection.query_row(
            "
            SELECT user_message, final_answer_text
            FROM codex_turns
            WHERE project_id = ?1 AND session_id = ?2 AND turn_ordinal = 0
            ",
            ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        assert_eq!(report.sessions_indexed, 1);
        assert_eq!(report.turns_indexed, 1);
        assert_eq!(indexed_turn.0, "Fresh task");
        assert_eq!(indexed_turn.1, "Fresh reply");

        Ok(())
    }

    #[test]
    fn parse_project_falls_back_when_selected_duplicate_is_corrupt() -> Result<()> {
        let memstack_root = unique_test_dir("parse-corrupt-winner");
        let project_root = memstack_root.join("repo");
        let sessions_root = memstack_root.join("projects/repo-abc123/sessions");
        let codex_root = sessions_root.join("codex");
        fs::create_dir_all(&project_root)?;
        write_parse_config(&memstack_root, &project_root, &sessions_root)?;

        write_file(
            &codex_root
                .join("rollout-2026-04-01T09-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
            &format!(
                concat!(
                    "{{\"timestamp\":\"2026-04-01T09:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T09:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Stale task\"}}}}\n",
                    "{{\"timestamp\":\"2026-04-01T09:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Stale reply\"}}]}}}}\n"
                ),
                project_root.display()
            ),
        )?;
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_file(
            &codex_root
                .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
            &format!("{{not-json\n{}\n", "x".repeat(4096)),
        )?;

        let report = parse_project_codex_turns_from(&project_root, memstack_root.clone())?;
        let connection = Connection::open(memstack_root.join(INDEX_DB_FILE_NAME))?;
        let indexed_row: (String, String, String) = connection.query_row(
            "
            SELECT s.archive_path, t.user_message, t.final_answer_text
            FROM codex_sessions s
            JOIN codex_turns t
              ON t.project_id = s.project_id
             AND t.session_id = s.session_id
             AND t.turn_ordinal = 0
            WHERE s.project_id = ?1 AND s.session_id = ?2
            ",
            ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

        assert_eq!(report.sessions_indexed, 1);
        assert_eq!(report.turns_indexed, 1);
        assert_eq!(
            indexed_row.0,
            "codex/rollout-2026-04-01T09-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"
        );
        assert_eq!(indexed_row.1, "Stale task");
        assert_eq!(indexed_row.2, "Stale reply");

        Ok(())
    }

    #[test]
    fn parse_project_fails_when_all_duplicate_candidates_are_corrupt() -> Result<()> {
        let memstack_root = unique_test_dir("parse-all-corrupt-duplicates");
        let project_root = memstack_root.join("repo");
        let sessions_root = memstack_root.join("projects/repo-abc123/sessions");
        let codex_root = sessions_root.join("codex");
        fs::create_dir_all(&project_root)?;
        write_parse_config(&memstack_root, &project_root, &sessions_root)?;

        write_file(
            &codex_root
                .join("rollout-2026-04-01T09-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
            "{not-json\n",
        )?;
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_file(
            &codex_root
                .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
            &format!("{{not-json\n{}\n", "x".repeat(4096)),
        )?;

        let error =
            parse_project_codex_turns_from(&project_root, memstack_root).expect_err("parse fails");

        assert!(error.to_string().contains("failed to parse"));

        Ok(())
    }

    #[test]
    fn parse_project_skips_unchanged_fallback_candidate_after_corrupt_higher_duplicate()
    -> Result<()> {
        let memstack_root = unique_test_dir("parse-skip-fallback-duplicate");
        let project_root = memstack_root.join("repo");
        let sessions_root = memstack_root.join("projects/repo-abc123/sessions");
        let codex_root = sessions_root.join("codex");
        let fallback_path = codex_root
            .join("rollout-2026-04-01T09-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl");
        fs::create_dir_all(&project_root)?;
        write_parse_config(&memstack_root, &project_root, &sessions_root)?;

        let original = format!(
            concat!(
                "{{\"timestamp\":\"2026-04-01T09:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d3415-0b9c-7dc3-88e0-e9cb7a789e3f\",\"cwd\":\"{}\",\"cli_version\":\"0.118.0\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T09:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Original task\"}}}}\n",
                "{{\"timestamp\":\"2026-04-01T09:00:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Original reply\"}}]}}}}\n"
            ),
            project_root.display()
        );
        write_file(&fallback_path, &original)?;
        touch_file_timestamp(&fallback_path, "202604010900.00")?;
        write_file(
            &codex_root
                .join("rollout-2026-04-01T10-00-00-019d3415-0b9c-7dc3-88e0-e9cb7a789e3f.jsonl"),
            &format!("{{not-json\n{}\n", "x".repeat(4096)),
        )?;
        parse_project_codex_turns_from(&project_root, memstack_root.clone())?;

        write_file(&fallback_path, &"{".repeat(original.len()))?;
        touch_file_timestamp(&fallback_path, "202604010900.00")?;

        let report = parse_project_codex_turns_from(&project_root, memstack_root.clone())?;
        let connection = Connection::open(memstack_root.join(INDEX_DB_FILE_NAME))?;
        let indexed_turn: (String, String) = connection.query_row(
            "
            SELECT user_message, final_answer_text
            FROM codex_turns
            WHERE project_id = ?1 AND session_id = ?2 AND turn_ordinal = 0
            ",
            ["repo-abc123", "019d3415-0b9c-7dc3-88e0-e9cb7a789e3f"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        assert_eq!(report.sessions_indexed, 1);
        assert_eq!(report.turns_indexed, 1);
        assert_eq!(indexed_turn.0, "Original task");
        assert_eq!(indexed_turn.1, "Original reply");

        Ok(())
    }
}
