use super::*;

#[test]
fn parses_hidden_codex_schema_audit_command() {
    let cli = Cli::try_parse_from(["darc", "codex-schema-audit"]).unwrap();
    assert!(matches!(
        cli.command,
        Commands::CodexSchemaAudit(super::CodexSchemaAuditArgs { .. })
    ));
}

#[test]
fn parses_hidden_claude_schema_audit_command() {
    let cli = Cli::try_parse_from([
        "darc",
        "claude-schema-audit",
        "--sample-stride",
        "10",
        "--use-host-auth",
        "--from-version",
        "2.1.84",
        "--survey-mode",
        "coarse",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Commands::ClaudeSchemaAudit(super::ClaudeSchemaAuditArgs {
            sample_stride,
            use_host_auth,
            from_version,
            survey_mode,
            ..
        }) if sample_stride == 10
            && use_host_auth
            && from_version.as_deref() == Some("2.1.84")
            && matches!(survey_mode, super::ClaudeSurveyModeArg::Coarse)
    ));
}

#[test]
fn codex_schema_audit_help_mentions_github_tokens() {
    let mut command = cli_command();
    let help = command
        .find_subcommand_mut("codex-schema-audit")
        .expect("hidden subcommand should still be present")
        .render_long_help()
        .to_string();

    assert!(help.contains("GH_TOKEN"));
    assert!(help.contains("GITHUB_TOKEN"));
    assert!(help.contains("Personal access tokens are accepted."));
}

#[test]
fn claude_schema_audit_help_mentions_explicit_host_auth() {
    let mut command = cli_command();
    let help = command
        .find_subcommand_mut("claude-schema-audit")
        .expect("hidden subcommand should still be present")
        .render_long_help()
        .to_string();

    assert!(help.contains("--use-host-auth"));
    assert!(help.contains("does not provide an OS-level sandbox"));
}

#[test]
fn formats_compatible_codex_schema_audit_report() {
    let report = compatible_report();
    let output = format_codex_schema_audit_report(&report);

    assert_eq!(codex_schema_audit_exit_code(&report), 0);
    assert!(output.contains("Status: compatible"));
    assert!(output.contains("Release Source: GitHub Releases (openai/codex)"));
    assert!(output.contains("Compatible across 1 audited stable release tag(s)."));
}

#[test]
fn formats_drift_codex_schema_audit_report() {
    let report = CodexSchemaAuditReport {
        outcome: CodexSchemaAuditOutcome::Drift(CodexSchemaDrift {
            first_drift_tag: "rust-v0.119.0".to_owned(),
            difference_summary: vec!["$/required: array length changed from 2 to 3".to_owned()],
            likely_files_to_update: vec![
                "crates/core/src/rollout/codex/version.rs".to_owned(),
                "crates/core/src/rollout/codex/mod.rs".to_owned(),
            ],
        }),
        ..compatible_report()
    };
    let output = format_codex_schema_audit_report(&report);

    assert_eq!(codex_schema_audit_exit_code(&report), 1);
    assert!(output.contains("Status: schema drift detected"));
    assert!(output.contains("First Drift Tag: rust-v0.119.0"));
    assert!(output.contains("Likely Darc Files To Update:"));
}

#[test]
fn formats_compatible_claude_schema_audit_report() {
    let report = compatible_claude_report();
    let output = format_claude_schema_audit_report(&report);

    assert_eq!(claude_schema_audit_exit_code(&report), 0);
    assert!(output.contains("Status: compatible"));
    assert!(output.contains("Latest Published Claude Version: 2.1.92"));
    assert!(output.contains("Compatible Inspected Versions: 2.1.92, 2.1.87"));
    assert!(output.contains("Compatible across 2 inspected Claude version(s)."));
    assert!(output.contains("Supplementary Agent SDK Drift Version: 2.1.92"));
    assert!(output.contains("Sampling Stride: 1"));
    assert!(output.contains("Survey Mode: refine"));
    assert!(output.contains("Auth Mode: isolated (no auth)"));
}

#[test]
fn formats_failed_claude_schema_audit_versions() {
    let report = ClaudeSchemaAuditReport {
        compatible_inspected_versions: vec!["2.1.92".to_owned()],
        failed_versions: vec![ClaudeSchemaAuditFailure {
            version: "2.1.91".to_owned(),
            reason: "fixture `read_tool` did not trigger required Claude tool `Read`".to_owned(),
        }],
        supplementary_sdk_drift: None,
        ..compatible_claude_report()
    };
    let output = format_claude_schema_audit_report(&report);

    assert_eq!(claude_schema_audit_exit_code(&report), 1);
    assert!(output.contains("Status: audit incomplete"));
    assert!(output.contains("Compatible Inspected Versions: 2.1.92"));
    assert!(output.contains(
        "No transcript drift detected across 1 compatible inspected Claude version(s), but 1 inspected version(s) failed."
    ));
    assert!(output.contains("Failed Inspected Versions:"));
    assert!(
        output
            .contains("- 2.1.91: fixture `read_tool` did not trigger required Claude tool `Read`")
    );
}

#[test]
fn formats_drift_claude_schema_audit_report() {
    let report = ClaudeSchemaAuditReport {
        survey_mode: ClaudeSchemaSurveyMode::Coarse,
        transcript_drift_windows: vec![ClaudeSchemaDriftWindow {
            window_start_version: "2.1.88".to_owned(),
            window_end_version: "2.1.90".to_owned(),
            sampled_compatible_version: "2.1.87".to_owned(),
            sampled_drift_version: "2.1.90".to_owned(),
            difference_summary: vec!["$/line_types: array length changed from 4 to 5".to_owned()],
        }],
        outcome: ClaudeSchemaAuditOutcome::Drift(ClaudeSchemaDrift {
            first_drift_version: "2.1.90".to_owned(),
            boundary_precision: ClaudeSchemaDriftBoundaryPrecision::Exact,
            difference_summary: vec![
                "$/line_types[2]: changed from \"system\" to \"mystery-event\"".to_owned(),
            ],
            likely_files_to_update: vec![
                "crates/core/src/rollout/claude/version.rs".to_owned(),
                "crates/core/src/rollout/claude/mod.rs".to_owned(),
            ],
        }),
        supplementary_sdk_drift: None,
        ..compatible_claude_report()
    };
    let output = format_claude_schema_audit_report(&report);

    assert_eq!(claude_schema_audit_exit_code(&report), 1);
    assert!(output.contains("Status: schema drift detected"));
    assert!(output.contains("First Drift Version: 2.1.90"));
    assert!(output.contains("Likely Darc Files To Update:"));
    assert!(output.contains("Survey Mode: coarse"));
    assert!(output.contains("Sampled Transcript Drift Windows:"));
}

#[test]
fn formats_sampled_claude_schema_drift_without_first_boundary_claim() {
    let report = ClaudeSchemaAuditReport {
        outcome: ClaudeSchemaAuditOutcome::Drift(ClaudeSchemaDrift {
            first_drift_version: "2.1.90".to_owned(),
            boundary_precision: ClaudeSchemaDriftBoundaryPrecision::Sampled,
            difference_summary: vec![
                "$/line_types[2]: changed from \"system\" to \"mystery-event\"".to_owned(),
            ],
            likely_files_to_update: vec!["crates/rollout/src/claude/version.rs".to_owned()],
        }),
        supplementary_sdk_drift: None,
        ..compatible_claude_report()
    };
    let output = format_claude_schema_audit_report(&report);

    assert!(output.contains("Status: schema drift detected"));
    assert!(output.contains("Sampled Drift Version: 2.1.90"));
    assert!(output.contains("Drift Boundary Precision: sampled window (first drift unproven)"));
    assert!(!output.contains("First Drift Version: 2.1.90"));
}
