use std::{
    cell::Cell,
    io::{self, Write},
};

use darc_core::{
    ShareFetchReport, ShareMergeReport, SharePullProgress, SharePullReport, SharePushProgress,
    SharePushReport, ShareUploadKind,
};

use super::*;

#[derive(Default)]
struct FlushCountingWriter {
    bytes: Vec<u8>,
    flushes: usize,
}

impl Write for FlushCountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

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

fn sample_share_pull_report() -> SharePullReport {
    SharePullReport {
        fetch: ShareFetchReport {
            branch: "team".to_owned(),
            git_branch: "darc/team".to_owned(),
            remote_name: "origin".to_owned(),
            remote_url: "https://example.invalid/team/share.git".to_owned(),
            fetched: true,
        },
        merge: ShareMergeReport {
            branch: "team".to_owned(),
            git_branch: "darc/team".to_owned(),
            remote_name: "origin".to_owned(),
            project_key: "git:https://example.invalid/team/repo.git".to_owned(),
            imported_turn_count: 2,
            skipped_turn_count: 1,
            warning_count: 0,
            warnings: Vec::new(),
        },
    }
}

#[test]
fn share_push_progress_printer_throttles_hot_session_updates() {
    let mut output = FlushCountingWriter::default();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::share::SharePushProgressPrinter::new(&mut output, style, true);
        printer.record(SharePushProgress::ExportingSessions {
            exported_sessions: 0,
            total_sessions: 10_000,
        });
        for exported_sessions in 1..10_000 {
            printer.record(SharePushProgress::ExportingSessions {
                exported_sessions,
                total_sessions: 10_000,
            });
        }
        printer.record(SharePushProgress::ExportingSessions {
            exported_sessions: 10_000,
            total_sessions: 10_000,
        });
    }

    assert_eq!(output.flushes, 2);
}

#[test]
fn share_pull_progress_printer_throttles_hot_session_updates() {
    let mut output = FlushCountingWriter::default();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::share::SharePullProgressPrinter::new(&mut output, style, true);
        printer.record(SharePullProgress::ImportingSessions {
            processed_sessions: 0,
            total_sessions: 10_000,
        });
        for processed_sessions in 1..10_000 {
            printer.record(SharePullProgress::ImportingSessions {
                processed_sessions,
                total_sessions: 10_000,
            });
        }
        printer.record(SharePullProgress::ImportingSessions {
            processed_sessions: 10_000,
            total_sessions: 10_000,
        });
    }

    assert_eq!(output.flushes, 2);
}

#[test]
fn share_push_progress_printer_ignores_unrendered_git_fragments_without_flush() {
    let mut output = FlushCountingWriter::default();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::share::SharePushProgressPrinter::new(&mut output, style, true);
        for _ in 0..100 {
            printer.record(SharePushProgress::GitProgress {
                kind: ShareUploadKind::Git,
                message: "Counting objects: synthetic diagnostic".to_owned(),
            });
        }
    }

    assert_eq!(output.flushes, 0);
    assert!(output.bytes.is_empty());
}

#[test]
fn share_push_progress_printer_writes_session_and_upload_bars() {
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
        printer.record(SharePushProgress::ExportingSessions {
            exported_sessions: 2,
            total_sessions: 4,
        });
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
    assert!(output.contains("Exporting sessions [############------------] 2/4  50%"));
    assert!(!output.contains("Exporting turns"));
    assert!(output.contains("Uploading          [############------------]  50%\x1b[K"));
    assert!(output.contains("done abc123"));
}

#[test]
fn share_push_progress_printer_uses_styled_unicode_bar_when_available() {
    let mut output = Vec::new();
    {
        let style = super::HumanStyle::new(true, false, Some("xterm-256color"));
        let mut printer = super::share::SharePushProgressPrinter::new(&mut output, style, true);
        printer.record(SharePushProgress::ExportingSessions {
            exported_sessions: 2,
            total_sessions: 4,
        });
    }

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains('━'));
    assert!(output.contains('─'));
    assert!(output.contains("\x1b[32m"));
    assert_eq!(
        strip_ansi_text(&output),
        "\r      Exporting sessions ━━━━━━━━━━━━──────────── 2/4  50%"
    );
}

#[test]
fn share_push_progress_printer_keeps_turn_bar_without_session_events() {
    let mut output = Vec::new();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::share::SharePushProgressPrinter::new(&mut output, style, true);
        printer.record(SharePushProgress::ExportingTurns {
            exported_turns: 50,
            total_turns: 100,
        });
    }

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Exporting turns"));
    assert!(output.contains("[############------------]  50/100  50%"));
}

#[test]
fn share_pull_progress_printer_writes_session_bar() {
    let mut output = Vec::new();
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::share::SharePullProgressPrinter::new(&mut output, style, true);
        printer.record(SharePullProgress::Started {
            git_branch: "darc/team".to_owned(),
            remote_name: "origin".to_owned(),
            remote_url: "https://example.invalid/team/share.git".to_owned(),
        });
        printer.record(SharePullProgress::ReadingCache);
        printer.record(SharePullProgress::ImportingSessions {
            processed_sessions: 2,
            total_sessions: 4,
        });
        printer.record(SharePullProgress::Finished {
            imported_turn_count: 2,
            skipped_turn_count: 1,
            warning_count: 0,
        });
    }

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Pulling darc/team from origin"));
    assert!(output.contains("Importing sessions [############------------] 2/4  50%"));
    assert!(output.contains("done imported 2 turns, skipped 1, warnings 0"));
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
fn share_step_line_colors_spinner_frame_when_styled() {
    let style = super::HumanStyle::new(true, false, Some("xterm-256color"));
    let output =
        super::share::render_share_step_line(style, 1, Some("⠋"), "Preparing share cache...");

    assert!(output.contains("\x1b[36m⠋\x1b[0m"));
    assert_eq!(strip_ansi_text(&output), "  ⠋ [1] Preparing share cache...");
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
    assert!(output.contains("1/2  50%\x1b[K\nerror: failed"));
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
    assert!(output.contains("Uploading          [############------------]  50%\x1b[K"));
    assert!(output.contains("done abc123"));
}

#[test]
fn run_pull_uses_progress_path_when_printer_is_enabled() {
    let mut output = Vec::new();
    let quiet_called = Cell::new(false);
    let progress_called = Cell::new(false);
    {
        let style = super::HumanStyle::new(false, false, None);
        let mut printer = super::share::SharePullProgressPrinter::new(&mut output, style, true);
        super::share::run_pull_with_progress_printer(
            ShareBranchArgs {
                branch: "team".to_owned(),
                remote: Some("origin".to_owned()),
                root: PathBuf::from("/tmp/darc-root"),
            },
            &mut printer,
            |_, _, _| {
                quiet_called.set(true);
                Ok(sample_share_pull_report())
            },
            |root, branch, remote, progress| {
                progress_called.set(true);
                assert_eq!(root, Some(PathBuf::from("/tmp/darc-root")));
                assert_eq!(branch, "team");
                assert_eq!(remote, Some("origin"));
                progress(SharePullProgress::Started {
                    git_branch: "darc/team".to_owned(),
                    remote_name: "origin".to_owned(),
                    remote_url: "https://example.invalid/team/share.git".to_owned(),
                });
                progress(SharePullProgress::ImportingSessions {
                    processed_sessions: 2,
                    total_sessions: 4,
                });
                progress(SharePullProgress::Finished {
                    imported_turn_count: 2,
                    skipped_turn_count: 1,
                    warning_count: 0,
                });
                Ok(sample_share_pull_report())
            },
        )
        .unwrap();
    }

    let output = String::from_utf8(output).unwrap();
    assert!(progress_called.get());
    assert!(!quiet_called.get());
    assert!(output.contains("Pulling darc/team from origin"));
    assert!(output.contains("Importing sessions [############------------] 2/4  50%"));
    assert!(output.contains("done imported 2 turns, skipped 1, warnings 0"));
}
