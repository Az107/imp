//! Rendering of agent progress.
//!
//! Assistant text goes to stdout so that `minion run ... > answer.txt` captures
//! only the answer. Tool activity, approvals, and diagnostics go to stderr as
//! single dimmed lines: the terminal stays a scrollback, never a canvas.
//!
//! On a terminal, assistant text is rendered as markdown (§5.11). Rendering is
//! per block, so text already on screen is never revised — that is what keeps
//! the scrollback intact. When stdout is a pipe the raw markdown is emitted
//! instead, because that is the more useful thing to capture.

use std::io::Write;

use minion_core::agent::AgentEvent;

use crate::markdown::{self, Stream};

/// Prints streamed events in either prose or NDJSON form.
pub struct Renderer {
    json: bool,
    colour: bool,
    /// Present only when markdown is switched on and not in JSON mode.
    stream: Option<Stream>,
    /// Whether the cursor sits mid-line, awaiting more text.
    mid_line: bool,
}

impl Renderer {
    /// Build a renderer.
    ///
    /// `style` decides whether markdown is rendered and how it is coloured;
    /// `colour` separately controls the dimming of the stderr tool lines.
    pub fn new(json: bool, style: markdown::Style, colour: bool) -> Self {
        let stream = (!json && style.enabled).then(|| Stream::new(style));
        Self {
            json,
            colour,
            stream,
            mid_line: false,
        }
    }

    /// Handle one event.
    pub fn handle(&mut self, event: &AgentEvent) {
        if self.json {
            self.handle_json(event);
            return;
        }
        match event {
            AgentEvent::TextDelta(text) => self.text(text),
            AgentEvent::ToolStarted { name, arguments } => {
                self.end_line();
                let line = format!("▸ {name} {}", clip(arguments, 140));
                eprintln!("{}", self.dim(&line));
            }
            AgentEvent::ToolFinished { ok, summary, .. } => {
                let mark = if *ok { "↳" } else { "✗" };
                eprintln!("{}", self.dim(&format!("  {mark} {summary}")));
            }
            AgentEvent::Failed(message) => {
                self.end_line();
                eprintln!("{}", self.fail(&format!("provider error: {message}")));
            }
        }
    }

    /// Terminate any partial line and flush the last block.
    pub fn finish(&mut self) {
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
            AgentEvent::ToolStarted { name, arguments } => {
                serde_json::json!({ "type": "tool_call", "name": name, "arguments": arguments })
            }
            AgentEvent::ToolFinished { name, ok, summary } => {
                serde_json::json!({ "type": "tool_result", "name": name, "ok": ok, "summary": summary })
            }
            AgentEvent::Failed(message) => {
                serde_json::json!({ "type": "error", "message": message })
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

    fn dim(&self, text: &str) -> String {
        if self.colour {
            format!("\x1b[2m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn fail(&self, text: &str) -> String {
        if self.colour {
            format!("\x1b[31m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

/// Clip to `width` characters, respecting character boundaries.
fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let clipped: String = text.chars().take(width.saturating_sub(3)).collect();
    format!("{clipped}...")
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
        let renderer = Renderer::new(true, Style::rendered(true, 80), false);
        assert!(renderer.stream.is_none());
    }

    #[test]
    fn markdown_is_skipped_when_the_style_is_off() {
        // A pipe gets raw markdown, so no stream is created.
        let renderer = Renderer::new(false, Style::plain(), false);
        assert!(renderer.stream.is_none());
    }

    #[test]
    fn markdown_is_used_on_a_terminal() {
        let renderer = Renderer::new(false, Style::rendered(true, 80), true);
        assert!(renderer.stream.is_some());
    }
}
