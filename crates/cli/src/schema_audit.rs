use super::*;

/// Runs the hidden Codex rollout schema audit command with dedicated exit codes.
pub(crate) fn run_codex_schema_audit_command(args: CodexSchemaAuditArgs) -> i32 {
    match run_codex_schema_audit_with_progress(
        CodexSchemaAuditOptions {
            cache_dir: args.cache_dir,
        },
        |message| eprintln!("[codex-schema-audit] {message}"),
    ) {
        Ok(report) => {
            println!("{}", format_codex_schema_audit_report(&report));
            codex_schema_audit_exit_code(&report)
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            2
        }
    }
}

/// Runs the hidden Claude rollout schema audit command with dedicated exit codes.
pub(crate) fn run_claude_schema_audit_command(args: ClaudeSchemaAuditArgs) -> i32 {
    match run_claude_schema_audit_with_progress(
        ClaudeSchemaAuditOptions {
            cache_dir: args.cache_dir,
            use_host_auth: args.use_host_auth,
            sample_stride: args.sample_stride,
            from_version: args.from_version,
            survey_mode: args.survey_mode.into(),
        },
        |message| eprintln!("[claude-schema-audit] {message}"),
    ) {
        Ok(report) => {
            println!("{}", format_claude_schema_audit_report(&report));
            claude_schema_audit_exit_code(&report)
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            2
        }
    }
}

/// Returns the CLI exit code for one Codex schema audit report.
pub(crate) fn codex_schema_audit_exit_code(report: &CodexSchemaAuditReport) -> i32 {
    if report.is_compatible() { 0 } else { 1 }
}

/// Returns the CLI exit code for one Claude schema audit report.
pub(crate) fn claude_schema_audit_exit_code(report: &ClaudeSchemaAuditReport) -> i32 {
    if report.is_compatible() { 0 } else { 1 }
}

/// Formats one Codex schema audit report for the hidden CLI command.
pub(crate) fn format_codex_schema_audit_report(report: &CodexSchemaAuditReport) -> String {
    let mut lines = vec![
        format!(
            "Status: {}",
            if report.is_compatible() {
                "compatible"
            } else {
                "schema drift detected"
            }
        ),
        format!("Release Source: {}", report.release_source),
        format!("Binary Cache: {}", report.binary_cache_dir.display()),
        format!(
            "Latest Stable Codex Release Tag: {}",
            report.latest_stable_release_tag
        ),
        format!(
            "Latest Exact-Covered Darc Version: {}",
            report.latest_exact_covered_version
        ),
        format!("Audited Tag Range: {}", report.audited_tag_range()),
    ];

    match &report.outcome {
        CodexSchemaAuditOutcome::Compatible => {
            lines.push(format!(
                "Compatible across {} audited stable release tag(s).",
                report.audited_tags.len()
            ));
        }
        CodexSchemaAuditOutcome::Drift(drift) => {
            lines.push(format!("First Drift Tag: {}", drift.first_drift_tag));
            lines.push("Schema Differences:".to_owned());
            lines.extend(
                drift
                    .difference_summary
                    .iter()
                    .map(|line| format!("- {line}")),
            );
            lines.push("Likely Darc Files To Update:".to_owned());
            lines.extend(
                drift
                    .likely_files_to_update
                    .iter()
                    .map(|path| format!("- {path}")),
            );
        }
    }

    lines.join("\n")
}

/// Formats one Claude schema audit report for the hidden CLI command.
pub(crate) fn format_claude_schema_audit_report(report: &ClaudeSchemaAuditReport) -> String {
    let status = match &report.outcome {
        ClaudeSchemaAuditOutcome::Drift(_) => "schema drift detected",
        ClaudeSchemaAuditOutcome::Compatible if report.is_incomplete() => "audit incomplete",
        ClaudeSchemaAuditOutcome::Compatible => "compatible",
    };
    let mut lines = vec![
        format!("Status: {status}"),
        format!("Release Source: {}", report.release_source),
        format!("Binary Cache: {}", report.binary_cache_dir.display()),
        format!(
            "Latest Published Claude Version: {}",
            report.latest_published_version
        ),
        format!(
            "Latest Exact-Covered Darc Version: {}",
            report.latest_exact_covered_version
        ),
        format!("Audited Version Range: {}", report.audited_version_range()),
        format!(
            "Inspected Versions: {}",
            report.inspected_versions.join(", ")
        ),
        format!(
            "Compatible Inspected Versions: {}",
            report.compatible_inspected_versions.join(", ")
        ),
        format!("Sampling Stride: {}", report.sample_stride),
        format!(
            "Survey Mode: {}",
            match report.survey_mode {
                ClaudeSchemaSurveyMode::Refine => "refine",
                ClaudeSchemaSurveyMode::Coarse => "coarse",
            }
        ),
        format!(
            "Auth Mode: {}",
            if report.used_host_auth {
                "host (explicit opt-in)"
            } else {
                "isolated (no auth)"
            }
        ),
    ];

    match &report.outcome {
        ClaudeSchemaAuditOutcome::Compatible => {
            if report.is_incomplete() {
                lines.push(format!(
                    "No transcript drift detected across {} compatible inspected Claude version(s), but {} inspected version(s) failed.",
                    report.compatible_inspected_versions.len(),
                    report.failed_versions.len()
                ));
            } else if report.sample_stride == 1 {
                lines.push(format!(
                    "Compatible across {} inspected Claude version(s).",
                    report.compatible_inspected_versions.len()
                ));
            } else {
                lines.push(format!(
                    "Compatible across {} Claude version(s) in range with {} compatible directly inspected version(s).",
                    report.audited_versions.len(),
                    report.compatible_inspected_versions.len()
                ));
            }
        }
        ClaudeSchemaAuditOutcome::Drift(drift) => {
            if drift.boundary_precision.is_exact() {
                lines.push(format!(
                    "First Drift Version: {}",
                    drift.first_drift_version
                ));
            } else {
                lines.push(format!(
                    "Sampled Drift Version: {}",
                    drift.first_drift_version
                ));
                lines.push(
                    "Drift Boundary Precision: sampled window (first drift unproven)".to_owned(),
                );
            }
            lines.push("Transcript Differences:".to_owned());
            lines.extend(
                drift
                    .difference_summary
                    .iter()
                    .map(|line| format!("- {line}")),
            );
            lines.push("Likely Darc Files To Update:".to_owned());
            lines.extend(
                drift
                    .likely_files_to_update
                    .iter()
                    .map(|path| format!("- {path}")),
            );
        }
    }

    if !report.failed_versions.is_empty() {
        lines.push("Failed Inspected Versions:".to_owned());
        lines.extend(
            report
                .failed_versions
                .iter()
                .map(|failure| format!("- {}: {}", failure.version, failure.reason)),
        );
    }

    if let Some(drift) = &report.supplementary_sdk_drift {
        lines.push(format!(
            "Supplementary Agent SDK Drift Version: {}",
            drift.first_drift_version
        ));
        lines.push("Supplementary Agent SDK Differences:".to_owned());
        lines.extend(
            drift
                .difference_summary
                .iter()
                .map(|line| format!("- {line}")),
        );
    }

    if !report.assumed_compatible_intervals.is_empty() {
        lines.push("Assumed Compatible Unsampled Intervals:".to_owned());
        lines.extend(
            report
                .assumed_compatible_intervals
                .iter()
                .map(|line| format!("- {line}")),
        );
    }

    if !report.transcript_drift_windows.is_empty() {
        lines.push("Sampled Transcript Drift Windows:".to_owned());
        lines.extend(report.transcript_drift_windows.iter().map(|window| {
            format!(
                "- {} ..= {} (sampled compatible {}, sampled drift {})",
                window.window_start_version,
                window.window_end_version,
                window.sampled_compatible_version,
                window.sampled_drift_version
            )
        }));
    }

    lines.join("\n")
}
