use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use clap::{ColorChoice, Parser};
use darc_core::{
    IndexReport, RefreshAllBestEffortReport, RefreshProgress, RefreshProjectAttempt,
    RefreshProjectFailure, RefreshReport, SourceKind, SyncReport,
    config::{ClaudeSourceConfig, CodexSourceConfig, SharedConfig, SourcesConfig, WatchConfig},
    index_rebuild_command,
};
use darc_rollout_audit::claude::{
    ClaudeSchemaAuditFailure, ClaudeSchemaAuditReport, ClaudeSchemaDrift,
    ClaudeSchemaDriftBoundaryPrecision, ClaudeSchemaDriftWindow, ClaudeSchemaSurveyMode,
    ClaudeSdkSchemaDrift,
};
use darc_rollout_audit::codex::{CodexSchemaAuditReport, CodexSchemaDrift};
use darc_rollout_audit::{claude::ClaudeSchemaAuditOutcome, codex::CodexSchemaAuditOutcome};
use darc_test_utils::{unique_test_dir, write_file};
use serde_json::Value;

use super::*;

fn compatible_report() -> CodexSchemaAuditReport {
    CodexSchemaAuditReport {
        release_source: "GitHub Releases (openai/codex)".to_owned(),
        binary_cache_dir: "/tmp/darc-cache".into(),
        latest_stable_release_tag: "rust-v0.118.0".to_owned(),
        latest_exact_covered_version: "0.118.0".to_owned(),
        audited_tags: vec!["rust-v0.118.0".to_owned()],
        outcome: CodexSchemaAuditOutcome::Compatible,
    }
}

/// Renders long help for one nested command path.
fn help_for_command_path(path: &[&str]) -> String {
    let mut command = cli_command();
    let mut current = &mut command;
    for name in path {
        current = current
            .find_subcommand_mut(name)
            .unwrap_or_else(|| panic!("subcommand `{name}` should be present"));
    }
    current.render_long_help().to_string()
}

/// Asserts that the given help sections appear in the expected order.
fn assert_contains_in_order(haystack: &str, needles: &[&str]) {
    let mut cursor = 0;
    for needle in needles {
        let tail = &haystack[cursor..];
        let Some(offset) = tail.find(needle) else {
            panic!("expected `{needle}` after byte {cursor} in:\n{haystack}");
        };
        cursor += offset + needle.len();
    }
}

fn compatible_claude_report() -> ClaudeSchemaAuditReport {
    ClaudeSchemaAuditReport {
        release_source: "npm registry (@anthropic-ai/claude-code)".to_owned(),
        binary_cache_dir: "/tmp/darc-claude-cache".into(),
        latest_published_version: "2.1.92".to_owned(),
        latest_exact_covered_version: "2.1.87".to_owned(),
        audited_versions: vec!["2.1.92".to_owned(), "2.1.87".to_owned()],
        inspected_versions: vec!["2.1.92".to_owned(), "2.1.87".to_owned()],
        compatible_inspected_versions: vec!["2.1.92".to_owned(), "2.1.87".to_owned()],
        failed_versions: Vec::new(),
        assumed_compatible_intervals: Vec::new(),
        sample_stride: 1,
        used_host_auth: false,
        survey_mode: ClaudeSchemaSurveyMode::Refine,
        transcript_drift_windows: Vec::new(),
        outcome: ClaudeSchemaAuditOutcome::Compatible,
        supplementary_sdk_drift: Some(ClaudeSdkSchemaDrift {
            first_drift_version: "2.1.92".to_owned(),
            difference_summary: vec![
                "$/agent_sdk_version: changed from \"0.2.87\" to \"0.2.92\"".to_owned(),
            ],
        }),
    }
}

fn sample_refresh_report(project_name: &str) -> RefreshReport {
    let project_root = PathBuf::from(format!("/tmp/{project_name}"));
    let sessions_root = project_root.join("sessions");
    RefreshReport {
        sync: SyncReport {
            project_name: project_name.to_owned(),
            project_root: project_root.clone(),
            sessions_root: sessions_root.clone(),
            sources: vec![SourceKind::Codex],
            sessions_copied: 1,
            sessions_unchanged: 0,
            auxiliary_copied: 0,
            auxiliary_unchanged: 0,
            new_known_paths: Vec::new(),
            warnings: Vec::new(),
            manifest_written: false,
            config_written: false,
        },
        index: IndexReport {
            project_name: project_name.to_owned(),
            project_root: project_root.clone(),
            sessions_root,
            index_db_path: project_root.join("index.sqlite"),
            providers: vec![SourceKind::Codex],
            sessions_discovered: 1,
            sessions_skipped_this_run: 0,
            sessions_currently_indexed: 1,
            turns_currently_indexed: 2,
            skipped_rollouts: Vec::new(),
        },
    }
}

/// Strips ANSI control sequences from rendered text for unit assertions.
fn strip_ansi_text(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for ch in chars.by_ref() {
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
        } else {
            output.push(ch);
        }
    }
    output
}

mod cli;
mod command_queries;
mod output;
mod schema_audit;
mod service_watch;
mod share_progress;
mod upgrade;
