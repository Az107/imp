//! The approval prompt.
//!
//! Kept out of `imp-core` so the policy engine has no dependency on a
//! terminal. Everything here goes to stderr, leaving stdout clean for the
//! conversation, and a non-interactive process never reaches this at all.

use std::io::{BufRead, IsTerminal, Write};
use std::sync::Arc;

use async_trait::async_trait;
use imp_core::error::{Error, Result};
use imp_core::policy::{ApprovalChoice, ApprovalRequest, ApprovalUi};

use crate::style::{Glyph, Theme};

/// Asks on stderr and reads one line from stdin.
pub struct TerminalUi {
    theme: Theme,
}

/// A UI that always refuses, for when nothing can be asked.
pub struct DenyUi;

#[async_trait]
impl ApprovalUi for DenyUi {
    async fn request(&self, request: &ApprovalRequest) -> Result<ApprovalChoice> {
        Err(Error::Denied(format!(
            "`{}` needs approval but this is non-interactive",
            request.tool
        )))
    }
}

#[async_trait]
impl ApprovalUi for TerminalUi {
    async fn request(&self, request: &ApprovalRequest) -> Result<ApprovalChoice> {
        let theme = self.theme;
        let mut stderr = std::io::stderr();

        // The prompt is built as whole lines so the whole block can be erased
        // once it is answered and replaced by a single decision line.
        let mut block: Vec<String> = Vec::new();
        block.push(String::new());
        block.push(format!(
            "  {} {}",
            theme.warn(theme.glyph(Glyph::Warn)),
            theme.bold(&format!("{} wants to run", request.tool))
        ));
        block.push(format!("     {}", request.summary));
        block.push(format!(
            "     {} {}{}",
            theme.dim("risk:"),
            theme.risk(request.risk),
            if request.flags.is_empty() {
                String::new()
            } else {
                format!(" · flagged: {}", request.flags.join(", "))
            }
        ));
        if request.tool == "run_command" {
            block.push(format!(
                "     {}",
                theme.dim(&format!(
                    "always would allow any `{}` command",
                    request.pattern
                ))
            ));
        }
        block.push(format!(
            "     {}",
            theme.dim("[o]nce  [s]ession  [a]lways  [d]eny")
        ));

        for line in &block {
            writeln!(stderr, "{line}")?;
        }
        let glyph = theme.glyph(Glyph::Prompt);
        write!(stderr, "{} ", if glyph.is_empty() { ">" } else { glyph })?;
        stderr.flush()?;

        // rusty's line editor owns stdin, so read directly rather than through it.
        let stdin = std::io::stdin();
        let mut line = String::new();
        let answered = stdin.lock().read_line(&mut line)? != 0;
        let choice = if answered {
            parse_choice(&line)
        } else {
            // EOF: treating it as consent would be the dangerous default.
            ApprovalChoice::Deny
        };

        // Collapse the questionnaire into one line, so the scrollback keeps the
        // decision and not the prompt. Cursor movement is only safe on a
        // terminal; a redirected stderr keeps the full prompt instead.
        if std::io::stderr().is_terminal() {
            // The block lines are newline-terminated; the prompt line is closed
            // by the user's Enter and left open by an EOF or a mid-line Ctrl-D.
            let newline = line.ends_with('\n');
            let up = block.len() + usize::from(newline);
            let _ = write!(stderr, "\x1b[{up}A\x1b[J");
        }
        let (mark, verdict) = match choice {
            ApprovalChoice::Once => (
                theme.success(theme.glyph(Glyph::Done)),
                theme.success("approved (once)"),
            ),
            ApprovalChoice::Session => (
                theme.success(theme.glyph(Glyph::Done)),
                theme.success("approved (this session)"),
            ),
            ApprovalChoice::Always => (
                theme.success(theme.glyph(Glyph::Done)),
                theme.success("approved (always)"),
            ),
            ApprovalChoice::Deny if answered => {
                (theme.error(theme.glyph(Glyph::Fail)), theme.error("denied"))
            }
            ApprovalChoice::Deny => (
                theme.error(theme.glyph(Glyph::Fail)),
                theme.error("refused (no answer)"),
            ),
        };
        writeln!(stderr, "  {mark} {} · {verdict}", theme.bold(&request.tool))?;
        Ok(choice)
    }
}

/// Map one typed line to a decision.
///
/// Only an exact keystroke or word is honoured. Matching on the first character
/// alone would be friendlier but unsafe: "sure" would silently become `session`,
/// and any stray paste could grant something. Anything unrecognised refuses,
/// because the cost of a wrong denial is a retry, while the cost of a wrong
/// grant is the thing this prompt exists to prevent.
pub fn parse_choice(line: &str) -> ApprovalChoice {
    match line.trim().to_ascii_lowercase().as_str() {
        "o" | "once" => ApprovalChoice::Once,
        "s" | "session" => ApprovalChoice::Session,
        "a" | "always" => ApprovalChoice::Always,
        _ => ApprovalChoice::Deny,
    }
}

/// The UI to use, given whether anyone can answer.
pub fn ui_for(interactive: bool, theme: Theme) -> Arc<dyn ApprovalUi> {
    if interactive {
        Arc::new(TerminalUi { theme })
    } else {
        Arc::new(DenyUi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_letters_map_to_their_choices() {
        assert_eq!(parse_choice("o"), ApprovalChoice::Once);
        assert_eq!(parse_choice("s"), ApprovalChoice::Session);
        assert_eq!(parse_choice("a"), ApprovalChoice::Always);
        assert_eq!(parse_choice("d"), ApprovalChoice::Deny);
    }

    #[test]
    fn surrounding_whitespace_and_newlines_are_ignored() {
        assert_eq!(parse_choice("  o \n"), ApprovalChoice::Once);
        assert_eq!(parse_choice("\n"), ApprovalChoice::Deny);
    }

    #[test]
    fn the_full_words_work_too() {
        assert_eq!(parse_choice("once"), ApprovalChoice::Once);
        assert_eq!(parse_choice("session"), ApprovalChoice::Session);
        assert_eq!(parse_choice("always"), ApprovalChoice::Always);
        assert_eq!(parse_choice("deny"), ApprovalChoice::Deny);
    }

    #[test]
    fn capitalisation_does_not_matter() {
        assert_eq!(parse_choice("A"), ApprovalChoice::Always);
        assert_eq!(parse_choice("DENY"), ApprovalChoice::Deny);
    }

    #[test]
    fn anything_unrecognised_refuses() {
        // Regression: first-character matching made "sure" grant session scope.
        for line in ["y", "yes", "1", "", "sure", "x", "oka", "allow", "true"] {
            assert_eq!(
                parse_choice(line),
                ApprovalChoice::Deny,
                "`{line}` must not grant consent"
            );
        }
    }
}
