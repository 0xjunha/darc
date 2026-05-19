use std::{cell::Cell, io::Write};

use darc_core::{SharePushProgress, SharePushReport, ShareUploadKind};

use super::*;

fn sample_share_push_report() -> SharePushReport {
    SharePushReport {
        branch: "team".to_owned(),
        git_branch: "darc/team".to_owned(),
        remote_name: "origin".to_owned(),
        remote_url: "https://example.invalid/team/share.git".to_owned(),
        project_key: "git:https://example.invalid/team/repo.git".to_owned(),
        exported_turn_count: 1,
        exported_session_count: 1,
        object_count: 2,
        commit_id: "abc123".to_owned(),
        pushed: true,
    }
}

#[test]
fn share_push_progress_printer_writes_export_and_upload_bars() {
    let mut output = Vec::new();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::share::SharePushProgressPrinter::new(&mut output, style, true);
        printer.record(SharePushProgress::Started {
            git_branch: "darc/team".to_owned(),
            remote_name: "origin".to_owned(),
            remote_url: "https://example.invalid/team/share.git".to_owned(),
        });
        printer.record(SharePushProgress::BuildingExport { total_turns: 100 });
        printer.record(SharePushProgress::ExportingTurns {
            exported_turns: 50,
            total_turns: 100,
        });
        printer.record(SharePushProgress::Uploading {
            kind: ShareUploadKind::Git,
        });
        printer.record(SharePushProgress::GitProgress {
            kind: ShareUploadKind::Git,
            message: "Writing objects: 50% (1/2)".to_owned(),
        });
        printer.record(SharePushProgress::Finished {
            commit_id: "abc123".to_owned(),
        });
    }

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Pushing darc/team to origin"));
    assert!(output.contains("[############------------] 50/100"));
    assert!(output.contains("Uploading [############------------] 50%\x1b[K"));
    assert!(output.contains("done abc123"));
}

#[test]
fn share_push_progress_printer_stays_silent_when_disabled() {
    let mut output = Vec::new();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::share::SharePushProgressPrinter::new(&mut output, style, false);
        printer.record(SharePushProgress::Started {
            git_branch: "darc/team".to_owned(),
            remote_name: "origin".to_owned(),
            remote_url: "https://example.invalid/team/share.git".to_owned(),
        });
        printer.record(SharePushProgress::ExportingTurns {
            exported_turns: 1,
            total_turns: 10,
        });
    }

    assert!(output.is_empty());
}

#[test]
fn share_push_progress_printer_numbers_emitted_steps_without_gaps() {
    let mut output = Vec::new();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::share::SharePushProgressPrinter::new(&mut output, style, true);
        printer.record(SharePushProgress::Started {
            git_branch: "darc/team".to_owned(),
            remote_name: "origin".to_owned(),
            remote_url: "https://example.invalid/team/share.git".to_owned(),
        });
        printer.record(SharePushProgress::PreparingCache);
        printer.record(SharePushProgress::FetchingRemote);
        printer.record(SharePushProgress::ReadingCache);
    }

    let output = String::from_utf8(output).unwrap();
    assert_contains_in_order(
        &output,
        &[
            "[1] Preparing share cache...",
            "[2] Fetching remote branch...",
            "[3] Reading cached share artifacts...",
        ],
    );
    assert!(!output.contains("[4] Reading cached share artifacts..."));
}

#[test]
fn share_push_progress_finish_clears_active_progress_row() {
    let mut output = Vec::new();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::share::SharePushProgressPrinter::new(&mut output, style, true);
        printer.record(SharePushProgress::ExportingTurns {
            exported_turns: 1,
            total_turns: 2,
        });
        printer.finish();
        writeln!(&mut output, "error: failed").unwrap();
    }

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("1/2\x1b[K\nerror: failed"));
}

#[test]
fn share_push_progress_printer_ignores_non_percent_git_diagnostics() {
    let mut output = Vec::new();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::share::SharePushProgressPrinter::new(&mut output, style, true);
        printer.record(SharePushProgress::GitProgress {
            kind: ShareUploadKind::Git,
            message: "fatal: synthetic upload failure".to_owned(),
        });
    }

    assert!(output.is_empty());
}

#[test]
fn run_push_uses_progress_path_when_printer_is_enabled() {
    let mut output = Vec::new();
    let quiet_called = Cell::new(false);
    let progress_called = Cell::new(false);
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::share::SharePushProgressPrinter::new(&mut output, style, true);
        super::share::run_push_with_progress_printer(
            ShareBranchArgs {
                branch: "team".to_owned(),
                remote: Some("origin".to_owned()),
                root: PathBuf::from("/tmp/darc-root"),
            },
            &mut printer,
            |_, _, _| {
                quiet_called.set(true);
                Ok(sample_share_push_report())
            },
            |root, branch, remote, progress| {
                progress_called.set(true);
                assert_eq!(root, Some(PathBuf::from("/tmp/darc-root")));
                assert_eq!(branch, "team");
                assert_eq!(remote, Some("origin"));
                progress(SharePushProgress::Started {
                    git_branch: "darc/team".to_owned(),
                    remote_name: "origin".to_owned(),
                    remote_url: "https://example.invalid/team/share.git".to_owned(),
                });
                progress(SharePushProgress::Uploading {
                    kind: ShareUploadKind::Git,
                });
                progress(SharePushProgress::GitProgress {
                    kind: ShareUploadKind::Git,
                    message: "Writing objects: 50% (1/2)".to_owned(),
                });
                progress(SharePushProgress::Finished {
                    commit_id: "abc123".to_owned(),
                });
                Ok(sample_share_push_report())
            },
        )
        .unwrap();
    }

    let output = String::from_utf8(output).unwrap();
    assert!(progress_called.get());
    assert!(!quiet_called.get());
    assert!(output.contains("Pushing darc/team to origin"));
    assert!(output.contains("Uploading [############------------] 50%\x1b[K"));
    assert!(output.contains("done abc123"));
}
