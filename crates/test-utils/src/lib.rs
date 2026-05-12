use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
pub use darc_store::test_support::{
    IndexedSessionFixture, IndexedTurnFixture, create_pre_analytics_index_schema,
    insert_indexed_session, insert_indexed_turn, insert_pre_analytics_turn,
    seed_legacy_codex_index,
};

static UNIQUE_TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Builds one unique temporary directory path for filesystem-based tests.
pub fn unique_test_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    let counter = UNIQUE_TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!(
        "test-{prefix}-{}-{nanos}-{counter}",
        std::process::id()
    ))
}

/// Writes one UTF-8 test file after creating any missing parent directories.
pub fn write_file(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().context("missing parent directory")?;
    fs::create_dir_all(parent)?;
    fs::write(path, content)?;
    Ok(())
}

/// Runs one Git command in a test repository and returns an error on failure.
pub fn run_git(cwd: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run git {:?} in {}", args, cwd.display()))?;
    if output.status.success() {
        return Ok(());
    }

    bail!(
        "git {:?} failed in {}: {}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

/// Initializes one Git repository fixture with a fixed identity and remote.
pub fn init_git_repo(path: &Path, remote: &str) -> Result<()> {
    fs::create_dir_all(path)?;
    run_git(path, &["init"])?;
    run_git(path, &["config", "user.name", "Darc Test"])?;
    run_git(path, &["config", "user.email", "darc-tests@example.com"])?;
    run_git(path, &["config", "commit.gpgsign", "false"])?;
    run_git(path, &["remote", "add", "origin", remote])
}
