//! Rendering of agent progress.
//!
//! Assistant text goes to stdout so that `minion run ... > answer.txt` captures
//! only the answer. Tool activity, approvals, and diagnostics go to stderr as
//! single dimmed lines: the terminal stays a scrollback, never a canvas.

use std::io::Write;

use minion_core::agent::AgentEvent;

/// Width at which a rendered tool argument list is clipped.
const ARG_WIDTH: usize = 140;

/// Prints streamed events in either prose or NDJSON form.
pub struct Renderer {
    json: bool,
    color: bool,
    mid_line: bool,
}

impl Renderer {
    /// Build a renderer. `color` should already account for `NO_COLOR` and TTY.
    pub fn new(json: bool, color: bool) -> Self {
        Self {
            json,
            color,
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
            AgentEvent::TextDelta(text) => {
                print!("{text}");
                let _ = std::io::stdout().flush();
                self.mid_line = true;
            }
            AgentEvent::ToolStarted { name, arguments } => {
                self.end_line();
                let line = format!("▸ {name} {}", clip(arguments, ARG_WIDTH));
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

    /// Terminate any partial line of streamed text.
    pub fn finish(&mut self) {
        self.end_line();
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
        if self.color {
            format!("\x1b[2m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn fail(&self, text: &str) -> String {
        if self.color {
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
}
