//! The approval prompt.
//!
//! Kept out of `imp-core` so the policy engine has no dependency on a
//! terminal. Everything here goes to stderr, leaving stdout clean for the
//! conversation, and a non-interactive process never reaches this at all.

use std::io::{BufRead, Write};
use std::sync::Arc;

use async_trait::async_trait;
use imp_core::error::{Error, Result};
use imp_core::policy::{ApprovalChoice, ApprovalRequest, ApprovalUi};

/// Asks on stderr and reads one line from stdin.
pub struct TerminalUi;

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
        // rusty's line editor owns stdin, so read directly rather than through it.
        let stdin = std::io::stdin();
        let mut line = String::new();

        let mut stderr = std::io::stderr();
        writeln!(stderr)?;
        writeln!(stderr, "  ⚠  {} wants to run", request.tool)?;
        writeln!(stderr, "     {}", request.summary)?;
        writeln!(
            stderr,
            "     risk: {}{}",
            request.risk.as_str(),
            if request.flags.is_empty() {
                String::new()
            } else {
                format!(" · flagged: {}", request.flags.join(", "))
            }
        )?;
        if request.tool == "run_command" {
            let pattern = &request.pattern;
            writeln!(stderr, "     always would allow any `{pattern}` command")?;
        }
        write!(stderr, "     [o]nce  [s]ession  [a]lways  [d]eny > ")?;
        stderr.flush()?;

        if stdin.lock().read_line(&mut line)? == 0 {
            // EOF: treating it as consent would be the dangerous default.
            writeln!(stderr, "     no answer; refusing")?;
            return Ok(ApprovalChoice::Deny);
        }

        Ok(parse_choice(&line))
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
pub fn ui_for(interactive: bool) -> Arc<dyn ApprovalUi> {
    if interactive {
        Arc::new(TerminalUi)
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
