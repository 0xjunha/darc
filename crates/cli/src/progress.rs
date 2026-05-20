use std::{
    io::{self, Write},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::output::HumanStyle;

const PROGRESS_BAR_WIDTH: usize = 24;
const PROGRESS_LABEL_WIDTH: usize = 18;
const PROGRESS_SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const PROGRESS_REDRAW_INTERVAL: Duration = Duration::from_millis(100);
const PROGRESS_COUNT_BUCKETS: u64 = 100;
const CLEAR_ACTIVE_LINE: &str = "\x1b[K";

/// Renders common CLI progress lines for interactive terminals.
pub(crate) struct ProgressOutput<W> {
    writer: W,
    style: HumanStyle,
    enabled: bool,
    live_spinner: bool,
    active_line: bool,
    active_step: Option<ActiveProgressStep>,
    active_bar: Option<ActiveProgressBar>,
    step_index: usize,
}

impl<W: Write> ProgressOutput<W> {
    /// Builds one common progress output from resolved terminal facts.
    #[cfg(test)]
    pub(crate) fn new(writer: W, style: HumanStyle, enabled: bool) -> Self {
        Self::new_with_live_spinner(writer, style, enabled, false)
    }

    /// Builds one common progress output with optional live step animation.
    pub(crate) fn new_with_live_spinner(
        writer: W,
        style: HumanStyle,
        enabled: bool,
        live_spinner: bool,
    ) -> Self {
        Self {
            writer,
            style,
            enabled,
            live_spinner: enabled && live_spinner,
            active_line: false,
            active_step: None,
            active_bar: None,
            step_index: 0,
        }
    }

    /// Returns whether this output will render progress.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the resolved human-output style.
    pub(crate) fn style(&self) -> HumanStyle {
        self.style
    }

    /// Returns the configured progress writer.
    pub(crate) fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Flushes the configured progress stream.
    pub(crate) fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }

    /// Finishes any active progress row before the caller prints another message.
    pub(crate) fn finish(&mut self) -> io::Result<()> {
        if self.enabled {
            self.finish_active_line()?;
            self.flush()?;
        }
        Ok(())
    }

    /// Writes one operation heading and resets numbered steps.
    pub(crate) fn heading(&mut self, message: &str) -> io::Result<()> {
        self.finish_active_line()?;
        self.step_index = 0;
        writeln!(self.writer, "{message}")
    }

    /// Writes one numbered step.
    pub(crate) fn step(&mut self, message: &str) -> io::Result<()> {
        self.step_with_indent("", message)
    }

    /// Writes one numbered step beneath the given indentation.
    pub(crate) fn step_with_indent(&mut self, indent: &str, message: &str) -> io::Result<()> {
        self.finish_active_line()?;
        self.step_index += 1;
        if self.live_spinner {
            let indent = indent.to_owned();
            let message = message.to_owned();
            write!(
                self.writer,
                "\r{}{}",
                render_progress_step_line_with_indent(
                    self.style,
                    &indent,
                    self.step_index,
                    Some(PROGRESS_SPINNER_FRAMES[0]),
                    &message
                ),
                CLEAR_ACTIVE_LINE
            )?;
            self.writer.flush()?;
            let spinner = LiveProgressStepSpinner::start(
                self.style,
                indent.clone(),
                self.step_index,
                message.clone(),
            );
            self.active_step = Some(ActiveProgressStep {
                indent,
                index: self.step_index,
                message,
                spinner: Some(spinner),
            });
            Ok(())
        } else {
            writeln!(
                self.writer,
                "{}",
                render_progress_step_line_with_indent(
                    self.style,
                    indent,
                    self.step_index,
                    None,
                    message
                )
            )
        }
    }

    /// Writes one in-place progress bar.
    pub(crate) fn write_bar(&mut self, label: &str, current: u64, total: u64) -> io::Result<()> {
        self.write_bar_with_indent("", label, current, total)
    }

    /// Writes one in-place progress bar beneath the given indentation.
    pub(crate) fn write_bar_with_indent(
        &mut self,
        indent: &str,
        label: &str,
        current: u64,
        total: u64,
    ) -> io::Result<()> {
        self.write_bar_with_indent_at(indent, label, current, total, Instant::now())
    }

    /// Writes one in-place progress bar when the redraw budget allows it.
    pub(crate) fn write_throttled_bar(
        &mut self,
        label: &str,
        current: u64,
        total: u64,
    ) -> io::Result<bool> {
        self.write_throttled_bar_with_indent("", label, current, total)
    }

    /// Writes one indented progress bar when the redraw budget allows it.
    pub(crate) fn write_throttled_bar_with_indent(
        &mut self,
        indent: &str,
        label: &str,
        current: u64,
        total: u64,
    ) -> io::Result<bool> {
        if !self.should_render_bar(indent, label, current, total) {
            return Ok(false);
        }
        let now = Instant::now();
        self.write_bar_with_indent_at(indent, label, current, total, now)?;
        Ok(true)
    }

    /// Writes one in-place percent progress bar when the redraw budget allows it.
    pub(crate) fn write_throttled_percent_bar(
        &mut self,
        label: &str,
        percent: u8,
    ) -> io::Result<bool> {
        if !self.should_render_bar("", label, u64::from(percent), 100) {
            return Ok(false);
        }
        let now = Instant::now();
        self.write_percent_bar_at(label, percent, now)?;
        Ok(true)
    }

    /// Writes one in-place progress bar at a known render time.
    fn write_bar_with_indent_at(
        &mut self,
        indent: &str,
        label: &str,
        current: u64,
        total: u64,
        rendered_at: Instant,
    ) -> io::Result<()> {
        self.finish_active_step()?;
        let bar = render_progress_bar(current, total, PROGRESS_BAR_WIDTH, self.style);
        let count = render_progress_count(current, total, self.style);
        let percent = render_progress_percent(current, total, self.style);
        write!(
            self.writer,
            "\r{indent}      {label:<PROGRESS_LABEL_WIDTH$} {bar} {count} {percent}{CLEAR_ACTIVE_LINE}"
        )?;
        self.active_line = true;
        self.active_bar = Some(ActiveProgressBar {
            indent: indent.to_owned(),
            label: label.to_owned(),
            current,
            total,
            rendered_at,
            next_check_current: next_progress_check_current(current, total),
        });
        Ok(())
    }

    /// Writes one in-place percent progress bar at a known render time.
    fn write_percent_bar_at(
        &mut self,
        label: &str,
        percent: u8,
        rendered_at: Instant,
    ) -> io::Result<()> {
        self.finish_active_step()?;
        let bar = render_progress_bar(u64::from(percent), 100, PROGRESS_BAR_WIDTH, self.style);
        let percent_text = render_percent(u64::from(percent), self.style);
        write!(
            self.writer,
            "\r      {label:<PROGRESS_LABEL_WIDTH$} {bar} {percent_text}{CLEAR_ACTIVE_LINE}"
        )?;
        self.active_line = true;
        self.active_bar = Some(ActiveProgressBar {
            indent: String::new(),
            label: label.to_owned(),
            current: u64::from(percent),
            total: 100,
            rendered_at,
            next_check_current: next_progress_check_current(u64::from(percent), 100),
        });
        Ok(())
    }

    /// Finishes any active in-place progress line before writing regular output.
    pub(crate) fn finish_active_line(&mut self) -> io::Result<()> {
        self.finish_active_step()?;
        if self.active_line {
            writeln!(self.writer)?;
            self.active_line = false;
        }
        self.active_bar = None;
        Ok(())
    }

    /// Finishes any active live step before rendering another progress shape.
    fn finish_active_step(&mut self) -> io::Result<()> {
        if let Some(mut step) = self.active_step.take() {
            if let Some(spinner) = &mut step.spinner {
                spinner.stop();
            }
            writeln!(
                self.writer,
                "\r{}{}",
                render_progress_step_line_with_indent(
                    self.style,
                    &step.indent,
                    step.index,
                    None,
                    &step.message
                ),
                CLEAR_ACTIVE_LINE
            )?;
        }
        Ok(())
    }

    /// Returns whether the latest bar state should be written to the terminal.
    fn should_render_bar(&mut self, indent: &str, label: &str, current: u64, total: u64) -> bool {
        let Some(rendered) = &mut self.active_bar else {
            return true;
        };
        if rendered.indent != indent || rendered.label != label || rendered.total != total {
            return true;
        }
        if rendered.current == current {
            return false;
        }
        if current == 0 || current >= total {
            return true;
        }
        if current < rendered.next_check_current {
            return false;
        }
        rendered.next_check_current = next_progress_check_current(current, total);
        Instant::now().duration_since(rendered.rendered_at) >= PROGRESS_REDRAW_INTERVAL
    }
}

/// Stores one active progress step currently animated by a spinner.
struct ActiveProgressStep {
    indent: String,
    index: usize,
    message: String,
    spinner: Option<LiveProgressStepSpinner>,
}

/// Stores the last in-place progress bar rendered to the terminal.
struct ActiveProgressBar {
    indent: String,
    label: String,
    current: u64,
    total: u64,
    rendered_at: Instant,
    next_check_current: u64,
}

/// Returns the next progress count that is worth checking against the time gate.
fn next_progress_check_current(current: u64, total: u64) -> u64 {
    let stride = (total / PROGRESS_COUNT_BUCKETS).max(1);
    current.saturating_add(stride).min(total)
}

/// Animates one active progress step on stderr while blocking work runs.
struct LiveProgressStepSpinner {
    stop: Option<mpsc::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl LiveProgressStepSpinner {
    /// Starts one live progress step spinner on stderr.
    fn start(style: HumanStyle, indent: String, step_index: usize, message: String) -> Self {
        let (stop, stop_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut frame_index = 1;
            let mut writer = io::stderr();
            while stop_rx.recv_timeout(Duration::from_millis(80)).is_err() {
                let frame = PROGRESS_SPINNER_FRAMES[frame_index % PROGRESS_SPINNER_FRAMES.len()];
                let _ = write!(
                    writer,
                    "\r{}{}",
                    render_progress_step_line_with_indent(
                        style,
                        &indent,
                        step_index,
                        Some(frame),
                        &message
                    ),
                    CLEAR_ACTIVE_LINE
                );
                let _ = writer.flush();
                frame_index += 1;
            }
        });
        Self {
            stop: Some(stop),
            handle: Some(handle),
        }
    }

    /// Stops the spinner thread and waits for it to exit.
    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for LiveProgressStepSpinner {
    /// Stops the spinner if its owner is dropped before normal completion.
    fn drop(&mut self) {
        self.stop();
    }
}

/// Renders one numbered progress step with an optional spinner frame.
#[cfg(test)]
pub(crate) fn render_progress_step_line(
    style: HumanStyle,
    step_index: usize,
    spinner: Option<&str>,
    message: &str,
) -> String {
    render_progress_step_line_with_indent(style, "", step_index, spinner, message)
}

/// Renders one indented numbered progress step with an optional spinner frame.
fn render_progress_step_line_with_indent(
    style: HumanStyle,
    indent: &str,
    step_index: usize,
    spinner: Option<&str>,
    message: &str,
) -> String {
    let step = format!("[{}]", style.count(step_index));
    if let Some(spinner) = spinner {
        format!("{indent}  {} {step} {message}", style.path(spinner))
    } else {
        format!("{indent}  {step} {message}")
    }
}

/// Renders a fixed-width progress bar with a styled terminal variant.
fn render_progress_bar(current: u64, total: u64, width: usize, style: HumanStyle) -> String {
    let filled = if total == 0 {
        width
    } else {
        let bounded = current.min(total);
        let width = u64::try_from(width).unwrap_or(u64::MAX);
        let scaled = (u128::from(bounded) * u128::from(width)) / u128::from(total);
        usize::try_from(scaled).unwrap_or(usize::MAX)
    };
    let filled = filled.min(width);
    let empty = width.saturating_sub(filled);
    if style.enabled {
        format!(
            "{}{}",
            style.ok("━".repeat(filled)),
            style.muted("─".repeat(empty))
        )
    } else {
        format!("[{}{}]", "#".repeat(filled), "-".repeat(empty))
    }
}

/// Renders a fixed-width current/total progress count.
fn render_progress_count(current: u64, total: u64, style: HumanStyle) -> String {
    let width = current.max(total).max(1).to_string().len();
    style.count(format!("{current:>width$}/{total}"))
}

/// Renders the percentage for one current/total progress pair.
fn render_progress_percent(current: u64, total: u64, style: HumanStyle) -> String {
    let percent = current
        .min(total)
        .saturating_mul(100)
        .checked_div(total)
        .unwrap_or(100);
    render_percent(percent, style)
}

/// Renders one right-aligned percentage.
fn render_percent(percent: u64, style: HumanStyle) -> String {
    style.count(format!("{percent:>3}%"))
}
