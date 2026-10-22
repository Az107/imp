//! Rendering of agent progress.
//!
//! Assistant text goes to stdout so that `imp run ... > answer.txt` captures
//! only the answer. Tool activity, approvals, and diagnostics go to stderr as
//! single dimmed lines: the terminal stays a scrollback, never a canvas.
//!
//! On a terminal, assistant text is rendered as markdown (§5.11). Rendering is
//! per block, so text already on screen is never revised — that is what keeps
//! the scrollback intact. When stdout is a pipe the raw markdown is emitted
//! instead, because that is the more useful thing to capture.
//!
//! A tool line carries its risk class and, once it has run, how long it took:
//! `▸ run_command … execute` then `  ↳ 12ms …`. The class is spelled out as well
//! as coloured, so `NO_COLOR` loses only the paint (§NFR-8).

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use imp_core::agent::AgentEvent;
use tokio::task::JoinHandle;

use crate::markdown::{self, Stream};
use crate::style::{self, Glyph, Theme};

/// Prints streamed events in either prose or NDJSON form.
pub struct Renderer {
    json: bool,
    theme: Theme,
    /// Present only when markdown is switched on and not in JSON mode.
    stream: Option<Stream>,
    /// Whether the cursor sits mid-line, awaiting more text.
    mid_line: bool,
    /// The transient activity line, cleared before anything durable is written.
    activity: Option<Arc<Activity>>,
}

impl Renderer {
    /// Build a renderer.
    ///
    /// `style` decides whether markdown is rendered and how it is coloured;
    /// `theme` separately styles the stderr tool lines. The two agree on colour
    /// because both are built from the same `[ui].color` decision.
    pub fn new(json: bool, style: markdown::Style, theme: Theme) -> Self {
        let stream = (!json && style.enabled).then(|| Stream::new(style));
        Self {
            json,
            theme,
            stream,
            mid_line: false,
            activity: None,
        }
    }

    /// Attach a transient activity line, cleared when the first event lands.
    pub fn with_activity(mut self, activity: Arc<Activity>) -> Self {
        self.activity = Some(activity);
        self
    }

    /// Handle one event.
    pub fn handle(&mut self, event: &AgentEvent) {
        // Real output is about to be written, so the spinner goes first. This
        // is the only place the scrollback rule allows a transient line: it is
        // erased, not scrolled past.
        self.clear_activity();
        if self.json {
            self.handle_json(event);
            return;
        }
        match event {
            AgentEvent::TextDelta(text) => self.text(text),
            AgentEvent::ToolStarted {
                name,
                arguments,
                risk,
            } => {
                self.end_line();
                let head = format!(
                    "{} {} {}",
                    self.theme.dim(self.theme.glyph(Glyph::Tool)),
                    self.theme.bold(name),
                    self.theme.dim(&clip(arguments, 140))
                );
                eprintln!("{head}  {}", self.theme.risk(*risk));
            }
            AgentEvent::ToolFinished {
                ok,
                summary,
                duration_ms,
                ..
            } => {
                let (mark, text) = if *ok {
                    (Glyph::Done, summary.to_string())
                } else {
                    (Glyph::Fail, self.theme.error(summary))
                };
                let glyph = if *ok {
                    self.theme.success(self.theme.glyph(mark))
                } else {
                    self.theme.glyph(mark).to_string()
                };
                let time = match duration_ms {
                    Some(ms) => self.theme.dim(&format!("{} ", format_duration(*ms))),
                    None => String::new(),
                };
                eprintln!("  {glyph} {time}{text}");
            }
            AgentEvent::Failed(message) => {
                self.end_line();
                eprintln!(
                    "{}",
                    self.theme.error(&format!("provider error: {message}"))
                );
            }
            AgentEvent::Notice(message) => {
                self.end_line();
                let line = format!("{} {message}", self.theme.glyph(Glyph::Notice));
                eprintln!("{}", self.theme.dim(&line));
            }
        }
    }

    /// Terminate any partial line and flush the last block.
    pub fn finish(&mut self) {
        self.clear_activity();
        if let Some(stream) = &mut self.stream {
            let out = stream.flush();
            if !out.is_empty() {
                print!("{out}");
                let _ = std::io::stdout().flush();
                // Only continue the line if the block did not already end one.
                self.mid_line = !out.ends_with('\n');
            }
        }
        self.end_line();
    }

    /// Emit a text delta, through the markdown stream when it is active.
    fn text(&mut self, delta: &str) {
        let out = match &mut self.stream {
            Some(stream) => stream.push(delta),
            None => delta.to_string(),
        };
        if out.is_empty() {
            return;
        }
        print!("{out}");
        let _ = std::io::stdout().flush();
        self.mid_line = !out.ends_with('\n');
    }

    fn handle_json(&mut self, event: &AgentEvent) {
        let value = match event {
            AgentEvent::TextDelta(text) => serde_json::json!({ "type": "text", "text": text }),
            AgentEvent::ToolStarted {
                name,
                arguments,
                risk,
            } => {
                serde_json::json!({ "type": "tool_call", "name": name, "arguments": arguments, "risk": risk.as_str() })
            }
            AgentEvent::ToolFinished {
                name,
                ok,
                summary,
                duration_ms,
            } => {
                serde_json::json!({ "type": "tool_result", "name": name, "ok": ok, "summary": summary, "duration_ms": duration_ms })
            }
            AgentEvent::Failed(message) => {
                serde_json::json!({ "type": "error", "message": message })
            }
            AgentEvent::Notice(message) => {
                serde_json::json!({ "type": "notice", "message": message })
            }
        };
        println!("{value}");
        let _ = std::io::stdout().flush();
    }

    fn end_line(&mut self) {
        if self.mid_line {
            println!();
            let _ = std::io::stdout().flush();
            self.mid_line = false;
        }
    }

    /// Stop and erase the activity line, once.
    fn clear_activity(&self) {
        if let Some(activity) = &self.activity {
            activity.clear();
        }
    }
}

/// A transient activity line on stderr.
///
/// The REPL is a scrollback, not a canvas (D2), so this never redraws durable
/// output: it owns one line via a carriage return, erases it before anything is
/// printed, and stops for good at the first real event. It is created only when
/// `[ui].spinner` is on and stderr is a terminal, so a pipe or `--json` never
/// sees it.
pub struct Activity {
    stop: AtomicBool,
    /// Serialises a frame against the erase, so the two never interleave.
    lock: Mutex<()>,
}

impl Activity {
    /// Spawn the animation, returning the handle to stop it and the task.
    ///
    /// The task is separate from the handle so a caller can abort it on the way
    /// out; clearing still happens through [`Activity::clear`] so the line is
    /// erased even when the task is asleep.
    pub fn spawn(theme: Theme) -> (Arc<Self>, JoinHandle<()>) {
        let activity = Arc::new(Self {
            stop: AtomicBool::new(false),
            lock: Mutex::new(()),
        });
        let task = {
            let activity = activity.clone();
            tokio::spawn(async move { activity.run(theme).await })
        };
        (activity, task)
    }

    /// Stop the animation and erase the line. Idempotent.
    pub fn clear(&self) {
        // `swap` makes the erase happen exactly once: a second call (the next
        // event, `finish`) has nothing to remove and must not blank a line the
        // renderer has since written.
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(_guard) = self.lock.lock() {
            erase();
        }
    }

    async fn run(&self, theme: Theme) {
        let frames = style::spinner_frames(theme.icons());
        let mut index = 0usize;
        loop {
            {
                let Ok(_guard) = self.lock.lock() else {
                    return;
                };
                if self.stop.load(Ordering::SeqCst) {
                    return;
                }
                let frame = theme.accent(frames[index % frames.len()]);
                let mut stderr = std::io::stderr();
                let _ = write!(stderr, "\r\x1b[K{frame}");
                let _ = stderr.flush();
            }
            index = index.wrapping_add(1);
            tokio::time::sleep(Duration::from_millis(90)).await;
        }
    }
}

/// Erase the current stderr line.
fn erase() {
    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "\r\x1b[K");
    let _ = stderr.flush();
}

/// Clip to `width` characters, respecting character boundaries.
fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let clipped: String = text.chars().take(width.saturating_sub(3)).collect();
    format!("{clipped}...")
}

/// A compact duration: milliseconds under a second, seconds above it.
pub fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::Style;

    #[test]
    fn clip_leaves_short_text_alone() {
        assert_eq!(clip("{}", 10), "{}");
    }

    #[test]
    fn clip_respects_char_boundaries() {
        let text = "é".repeat(50);
        let clipped = clip(&text, 10);
        assert!(clipped.chars().count() <= 10);
    }

    #[test]
    fn markdown_is_skipped_in_json_mode() {
        // JSON consumers want the model's own text, not a drawn table.
        let renderer = Renderer::new(true, Style::rendered(true, 80), Theme::new(false));
        assert!(renderer.stream.is_none());
    }

    #[test]
    fn markdown_is_skipped_when_the_style_is_off() {
        // A pipe gets raw markdown, so no stream is created.
        let renderer = Renderer::new(false, Style::plain(), Theme::new(false));
        assert!(renderer.stream.is_none());
    }

    #[test]
    fn markdown_is_used_on_a_terminal() {
        let renderer = Renderer::new(false, Style::rendered(true, 80), Theme::new(true));
        assert!(renderer.stream.is_some());
    }

    #[test]
    fn short_durations_stay_in_milliseconds() {
        assert_eq!(format_duration(0), "0ms");
        assert_eq!(format_duration(12), "12ms");
        assert_eq!(format_duration(999), "999ms");
    }

    #[test]
    fn long_durations_become_seconds() {
        assert_eq!(format_duration(1000), "1.0s");
        assert_eq!(format_duration(1500), "1.5s");
    }

    #[tokio::test]
    async fn clearing_an_activity_is_idempotent() {
        let (activity, task) = Activity::spawn(Theme::new(false));
        activity.clear();
        // A second clear must be a no-op, not another erase.
        activity.clear();
        task.abort();
    }
}
