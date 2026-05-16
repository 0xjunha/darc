use std::process::Command;

use anyhow::{Context, Result};
use darc_test_utils::{unique_test_dir, write_file};

/// Stores one minimal Darc config fixture for share CLI integration tests.
#[derive(serde::Serialize)]
struct ConfigFixture {
    version: u32,
    root: String,
    projects: Vec<()>,
}

/// Returns the compiled `darc` binary path exposed by Cargo integration tests.
fn darc_binary() -> &'static str {
    env!("CARGO_BIN_EXE_darc")
}

/// Writes one minimal workspace config file.
fn write_config_fixture(root: &std::path::Path) -> Result<()> {
    let config = ConfigFixture {
        version: 1,
        root: root.to_string_lossy().into_owned(),
        projects: Vec::new(),
    };
    write_file(
        &root.join("config.toml"),
        &toml::to_string(&config).context("failed to serialize config fixture TOML")?,
    )
}

/// Asserts credential-bearing URL parts are absent from captured output.
fn assert_redacted_remote_output(output: &str) {
    assert!(output.contains("https://example.invalid/team/share.git"));
    assert!(!output.contains("user:token"));
    assert!(!output.contains("access_token"));
    assert!(!output.contains("secret"));
}

#[test]
fn remote_add_and_list_redact_credentialed_urls() -> Result<()> {
    let root = unique_test_dir("cli-share-remote-redaction");
    write_config_fixture(&root)?;
    let root_arg = root.to_str().context("test root is not UTF-8")?;
    let credentialed_url =
        "https://user:token@example.invalid/team/share.git?access_token=secret#frag";

    let add_output = Command::new(darc_binary())
        .args([
            "remote",
            "--root",
            root_arg,
            "add",
            "team",
            credentialed_url,
        ])
        .output()
        .context("failed to run darc remote add")?;
    assert!(
        add_output.status.success(),
        "remote add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );
    let add_stdout = String::from_utf8(add_output.stdout)?;
    assert_redacted_remote_output(&add_stdout);

    let list_output = Command::new(darc_binary())
        .args(["remote", "--root", root_arg, "list"])
        .output()
        .context("failed to run darc remote list")?;
    assert!(
        list_output.status.success(),
        "remote list failed: {}",
        String::from_utf8_lossy(&list_output.stderr)
    );
    let list_stdout = String::from_utf8(list_output.stdout)?;
    assert_redacted_remote_output(&list_stdout);

    Ok(())
}
