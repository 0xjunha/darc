use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use darc_index::open_index_database;
use darc_paths::SourceKind;
use darc_test_utils::{
    IndexedSessionFixture, IndexedTurnFixture, insert_indexed_session, insert_indexed_turn,
    unique_test_dir, write_file,
};
use serde_json::{Value, json};

const PROJECT_ID: &str = "bench-repo";
const PROJECT_NAME: &str = "bench-repo";
const DEFAULT_SESSION_COUNT: usize = 120;
const DEFAULT_TURNS_PER_SESSION: usize = 16;
const DEFAULT_REPEAT_COUNT: usize = 5;

/// Stores one minimal darc config fixture for synthetic CLI scale benchmarks.
#[derive(serde::Serialize)]
struct ConfigFixture {
    version: u32,
    root: String,
    projects: Vec<ProjectFixture>,
}

/// Stores one configured project fixture for synthetic CLI scale benchmarks.
#[derive(serde::Serialize)]
struct ProjectFixture {
    id: String,
    name: String,
    local_path: String,
    sessions_root: String,
    known_paths: Vec<String>,
}

/// Stores one timed CLI benchmark scenario.
struct BenchScenario {
    name: &'static str,
    args: Vec<String>,
}

/// Stores timing output for one repeated benchmark scenario.
struct BenchResult {
    name: &'static str,
    repeat_count: usize,
    min: Duration,
    median: Duration,
    max: Duration,
    stdout_bytes: usize,
}

/// Runs the synthetic read/discovery CLI benchmark suite.
#[test]
#[ignore = "run explicitly: cargo test -p darc-cli --test read_scale_bench -- --ignored --nocapture"]
fn read_and_discovery_commands_scale_on_synthetic_history() -> Result<()> {
    let session_count = env_usize("DARC_BENCH_SESSIONS", DEFAULT_SESSION_COUNT)?;
    let turns_per_session = env_usize("DARC_BENCH_TURNS", DEFAULT_TURNS_PER_SESSION)?;
    let repeat_count = env_usize("DARC_BENCH_REPEAT", DEFAULT_REPEAT_COUNT)?;
    let fixture = SyntheticReadFixture::create(session_count, turns_per_session)?;
    let scenarios = fixture.scenarios();

    println!("scenario\trepeats\tmedian_ms\tmin_ms\tmax_ms\tstdout_bytes\tcommand");
    for scenario in scenarios {
        let result = run_repeated(&scenario, repeat_count)?;
        println!(
            "{}\t{}\t{:.3}\t{:.3}\t{:.3}\t{}\t{}",
            result.name,
            result.repeat_count,
            millis(result.median),
            millis(result.min),
            millis(result.max),
            result.stdout_bytes,
            shell_command(&scenario.args)
        );
    }

    remove_root(&fixture.root)?;
    Ok(())
}

/// Stores one generated Darc root and selected stable benchmark pivots.
struct SyntheticReadFixture {
    root: PathBuf,
    first_session_id: String,
    broad_path: String,
    narrow_path: String,
}

impl SyntheticReadFixture {
    /// Creates one synthetic indexed Darc root with deterministic sessions, turns, and file touches.
    fn create(session_count: usize, turns_per_session: usize) -> Result<Self> {
        if session_count == 0 || turns_per_session == 0 {
            bail!("DARC_BENCH_SESSIONS and DARC_BENCH_TURNS must be greater than zero");
        }

        let root = unique_test_dir("darc-cli-read-scale-bench");
        let project_root = root.join("repo");
        let sessions_root = root.join("projects").join(PROJECT_ID).join("sessions");
        fs::create_dir_all(&project_root)?;
        fs::create_dir_all(&sessions_root)?;
        write_config_fixture(&root, &project_root, &sessions_root)?;

        let mut connection = open_index_database(&root.join("index.sqlite"))?;
        let transaction = connection.transaction()?;
        let first_session_id = synthetic_session_id(0);
        for session_index in 0..session_count {
            let session_id = synthetic_session_id(session_index);
            insert_indexed_session(
                &transaction,
                IndexedSessionFixture::new(
                    PROJECT_ID,
                    SourceKind::Codex,
                    &session_id,
                    project_root.to_string_lossy().as_ref(),
                ),
            )?;
            for turn_index in 0..turns_per_session {
                insert_synthetic_turn(
                    &transaction,
                    &session_id,
                    session_index,
                    turn_index,
                    turns_per_session,
                )?;
            }
        }
        transaction.commit()?;

        Ok(Self {
            root,
            first_session_id,
            broad_path: "src/shared.rs".to_owned(),
            narrow_path: "crates/package-000/file-000.rs".to_owned(),
        })
    }

    /// Returns the canonical CLI benchmark scenarios over the generated root.
    fn scenarios(&self) -> Vec<BenchScenario> {
        let root = self.root.to_string_lossy().into_owned();
        let mut scenarios = vec![
            scenario("list projects", ["list", "projects", "--root", &root]),
            scenario("show workspace", ["show", "workspace", "--root", &root]),
            scenario(
                "list sessions page",
                [
                    "list",
                    "sessions",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "list sessions existence probe",
                [
                    "list",
                    "sessions",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--limit",
                    "0",
                ],
            ),
            scenario(
                "list sessions paginated offset",
                [
                    "list",
                    "sessions",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--limit",
                    "10",
                    "--offset",
                    "40",
                ],
            ),
            scenario(
                "list sessions large output",
                [
                    "list",
                    "sessions",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--limit",
                    "100",
                ],
            ),
            scenario(
                "list sessions file pivot",
                [
                    "list",
                    "sessions",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--touching",
                    &self.broad_path,
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "list sessions file pivot no match",
                [
                    "list",
                    "sessions",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--touching",
                    "missing/no-match.rs",
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "list files top",
                [
                    "list",
                    "files",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "list files path broad",
                [
                    "list",
                    "files",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    &self.broad_path,
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "list files path narrow",
                [
                    "list",
                    "files",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    &self.narrow_path,
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "list files co-touched",
                [
                    "list",
                    "files",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--co-touched-with",
                    &self.broad_path,
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "list turns page",
                [
                    "list",
                    "turns",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    &self.first_session_id,
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "show session bounded",
                [
                    "show",
                    "session",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    &self.first_session_id,
                    "--turn-limit",
                    "5",
                    "--step-limit",
                    "10",
                ],
            ),
            scenario(
                "show session large output",
                [
                    "show",
                    "session",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    &self.first_session_id,
                    "--turn-limit",
                    "25",
                    "--step-limit",
                    "50",
                ],
            ),
            scenario(
                "show session turn offset",
                [
                    "show",
                    "session",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    &self.first_session_id,
                    "--turn-limit",
                    "5",
                    "--turn-offset",
                    "2",
                    "--step-limit",
                    "10",
                ],
            ),
            scenario(
                "show turn bounded",
                [
                    "show",
                    "turn",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    &self.first_session_id,
                    "0",
                    "--step-limit",
                    "10",
                ],
            ),
            scenario(
                "search keyword broad",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "shared-scale-token",
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "search keyword narrow",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "needle-000-000",
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "search keyword no match",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "__synthetic_no_match__",
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "search keyword existence probe",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "shared-scale-token",
                    "--limit",
                    "0",
                ],
            ),
            scenario(
                "search literal exact",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--mode",
                    "literal",
                    "--query",
                    "literal-scale-token-000-000",
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "search literal no match",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--mode",
                    "literal",
                    "--query",
                    "__synthetic_no_match__",
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "search literal no match with tool output",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--mode",
                    "literal",
                    "--query",
                    "__synthetic_no_match__",
                    "--include-tool-output",
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "search regex exact",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--mode",
                    "regex",
                    "--query",
                    "literal-scale-token-000-[0-9]{3}",
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "search regex no match",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--mode",
                    "regex",
                    "--query",
                    "__synthetic_no_match__",
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "search regex no match with tool output",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--mode",
                    "regex",
                    "--query",
                    "__synthetic_no_match__",
                    "--include-tool-output",
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "search file-name",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--mode",
                    "file-name",
                    "shared.rs",
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "search file-path glob",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--mode",
                    "file-path",
                    "crates/**/*.rs",
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "search file-path",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--mode",
                    "file-path",
                    &self.broad_path,
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "search path-fragment broad",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--mode",
                    "path-fragment",
                    "package",
                    "--limit",
                    "10",
                ],
            ),
            scenario(
                "search path-fragment no match",
                [
                    "search",
                    "--root",
                    &root,
                    "--project-id",
                    PROJECT_ID,
                    "--mode",
                    "path-fragment",
                    "__synthetic_no_match__",
                    "--limit",
                    "10",
                ],
            ),
        ];

        scenarios.push(scenario(
            "search file-path no match",
            [
                "search",
                "--root",
                &root,
                "--project-id",
                PROJECT_ID,
                "--mode",
                "file-path",
                "missing/**/*.rs",
                "--limit",
                "10",
            ],
        ));
        scenarios
    }
}

/// Writes one minimal Darc config fixture.
fn write_config_fixture(root: &Path, project_root: &Path, sessions_root: &Path) -> Result<()> {
    let config = ConfigFixture {
        version: 1,
        root: root.to_string_lossy().into_owned(),
        projects: vec![ProjectFixture {
            id: PROJECT_ID.to_owned(),
            name: PROJECT_NAME.to_owned(),
            local_path: project_root.to_string_lossy().into_owned(),
            sessions_root: sessions_root.to_string_lossy().into_owned(),
            known_paths: Vec::new(),
        }],
    };
    write_file(
        &root.join("config.toml"),
        &toml::to_string(&config).context("failed to serialize config fixture TOML")?,
    )
}

/// Inserts one synthetic turn with search evidence and several file touches.
fn insert_synthetic_turn(
    connection: &rusqlite::Connection,
    session_id: &str,
    session_index: usize,
    turn_index: usize,
    turns_per_session: usize,
) -> Result<()> {
    let started_at = synthetic_timestamp(session_index, turn_index);
    let narrow_path = format!("crates/package-{session_index:03}/file-{turn_index:03}.rs");
    let rotating_path = format!("docs/topic-{:03}.md", turn_index % 20);
    let output = format!(
        "synthetic output for session {session_index:03} turn {turn_index:03} shared-scale-token"
    );
    let steps = json!([
        {
            "type": "tool_call",
            "timestamp": started_at,
            "call_id": format!("call-read-{session_index:03}-{turn_index:03}"),
            "name": "Read",
            "arguments": json!({ "file_path": "src/shared.rs" }).to_string()
        },
        {
            "type": "tool_call_output",
            "timestamp": started_at,
            "call_id": format!("call-read-{session_index:03}-{turn_index:03}"),
            "output": output
        },
        {
            "type": "tool_call",
            "timestamp": started_at,
            "call_id": format!("call-edit-{session_index:03}-{turn_index:03}"),
            "name": "Edit",
            "arguments": json!({ "file_path": narrow_path }).to_string()
        },
        {
            "type": "tool_call",
            "timestamp": started_at,
            "call_id": format!("call-list-{session_index:03}-{turn_index:03}"),
            "name": "Read",
            "arguments": json!({ "file_path": rotating_path }).to_string()
        }
    ])
    .to_string();
    let user_message = format!(
        "shared-scale-token needle-{session_index:03}-{turn_index:03} literal-scale-token-{session_index:03}-{turn_index:03}"
    );
    let final_answer = format!(
        "Completed synthetic turn {turn_index} of {turns_per_session} with literal-scale-token-{session_index:03}-{turn_index:03}."
    );
    insert_indexed_turn(
        connection,
        IndexedTurnFixture {
            turn_id: Some("synthetic-turn"),
            completed_at: Some(&started_at),
            user_message: &user_message,
            final_answer_at: Some(&started_at),
            final_answer_text: Some(&final_answer),
            step_count: 4,
            tool_call_count: 3,
            tool_output_count: 1,
            has_final_answer: true,
            changed_file_count: 1,
            added_line_count: 3,
            removed_line_count: 1,
            ..IndexedTurnFixture::new(
                PROJECT_ID,
                SourceKind::Codex,
                session_id,
                i64::try_from(turn_index).context("turn index should fit in i64")?,
                &started_at,
                "completed",
                &steps,
            )
        },
    )
}

/// Returns a deterministic UUID-like synthetic session id.
fn synthetic_session_id(index: usize) -> String {
    format!("00000000-0000-4000-8000-{index:012x}")
}

/// Returns a deterministic timestamp that sorts newest sessions first by index.
fn synthetic_timestamp(session_index: usize, turn_index: usize) -> String {
    let minute = session_index
        .checked_mul(5)
        .and_then(|value| value.checked_add(turn_index))
        .expect("synthetic benchmark timestamp should fit in usize");
    let total_hours = minute / 60;
    let total_days = total_hours / 24;
    let year = 2026 + total_days / 336;
    let day_of_year = total_days % 336;
    let month = 1 + day_of_year / 28;
    let day = 1 + day_of_year % 28;
    let hour = total_hours % 24;
    let minute = minute % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:00Z")
}

/// Builds one benchmark scenario from string-like arguments.
fn scenario<const N: usize>(name: &'static str, args: [&str; N]) -> BenchScenario {
    BenchScenario {
        name,
        args: args.into_iter().map(str::to_owned).collect(),
    }
}

/// Runs one scenario repeatedly and validates every JSON response.
fn run_repeated(scenario: &BenchScenario, repeat_count: usize) -> Result<BenchResult> {
    if repeat_count == 0 {
        bail!("DARC_BENCH_REPEAT must be greater than zero");
    }

    run_once(scenario)?;
    let mut timings = Vec::with_capacity(repeat_count);
    let mut stdout_bytes = 0usize;
    for _ in 0..repeat_count {
        let (elapsed, bytes) = run_once(scenario)?;
        stdout_bytes = bytes;
        timings.push(elapsed);
    }

    timings.sort();
    Ok(BenchResult {
        name: scenario.name,
        repeat_count,
        min: *timings.first().expect("repeat count should be nonzero"),
        median: timings[timings.len() / 2],
        max: *timings.last().expect("repeat count should be nonzero"),
        stdout_bytes,
    })
}

/// Runs one benchmark scenario once and validates the JSON envelope.
fn run_once(scenario: &BenchScenario) -> Result<(Duration, usize)> {
    let started = Instant::now();
    let output = Command::new(darc_binary())
        .args(&scenario.args)
        .output()
        .with_context(|| format!("failed to run {}", shell_command(&scenario.args)))?;
    let elapsed = started.elapsed();
    if !output.status.success() {
        bail!(
            "{} failed: {}",
            shell_command(&scenario.args),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let value: Value =
        serde_json::from_slice(&output.stdout).context("benchmark stdout should be JSON")?;
    if value.get("schema").and_then(Value::as_str).is_none() {
        bail!("benchmark response is missing a schema");
    }
    Ok((elapsed, output.stdout.len()))
}

/// Returns the compiled `darc` binary path exposed by Cargo integration tests.
fn darc_binary() -> &'static str {
    env!("CARGO_BIN_EXE_darc")
}

/// Returns one positive usize environment override.
fn env_usize(name: &str, default: usize) -> Result<usize> {
    env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<usize>()
                .with_context(|| format!("failed to parse {name}={value:?} as usize"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

/// Returns milliseconds for one duration.
fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

/// Returns one display command for benchmark output.
fn shell_command(args: &[String]) -> String {
    std::iter::once("darc")
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Removes one temporary test root after the benchmark finishes.
fn remove_root(root: &Path) -> Result<()> {
    fs::remove_dir_all(root)
        .with_context(|| format!("failed to remove temporary test root {}", root.display()))
}
