//! Salvaging tool arguments a small model got almost right.
//!
//! A weak or under-quantized model routinely wraps JSON in a code fence or
//! leaves a trailing comma. Both are one edit away from parsing, and feeding the
//! corrected call through is far cheaper than a retry. Repair is attempted only
//! after a normal parse fails, so a well-behaved provider is never second
//! guessed.

use serde_json::Value;

use crate::error::{Error, Result};

/// Parse a tool call's raw arguments, optionally repairing common mistakes.
///
/// An empty argument string is an empty object: several backends send it for a
/// no-argument call. On a parse failure with `repair` set, fenced code blocks
/// and trailing commas before `}`/`]` are stripped and parsing is retried once.
/// Anything still unparseable is an error the model can read and correct.
pub fn parse_tool_arguments(tool: &str, raw: &str, repair: bool) -> Result<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::json!({}));
    }
    match serde_json::from_str(trimmed) {
        Ok(value) => Ok(value),
        Err(original) => {
            if repair
                && let Some(repaired) = repair_json(trimmed)
                && let Ok(value) = serde_json::from_str(&repaired)
            {
                return Ok(value);
            }
            Err(Error::ToolArgs {
                tool: tool.to_string(),
                message: original.to_string(),
            })
        }
    }
}

/// Strip the mistakes a weak model makes around otherwise-valid JSON.
///
/// Returns `None` when nothing changed, so the caller can keep the original
/// parse error rather than retrying an identical string.
pub fn repair_json(raw: &str) -> Option<String> {
    let mut changed = false;
    let mut text = raw.trim();
    if let Some(rest) = text.strip_prefix("```json") {
        text = rest.trim_start();
        changed = true;
    } else if let Some(rest) = text.strip_prefix("```") {
        text = rest.trim_start();
        changed = true;
    }
    if let Some(rest) = text.strip_suffix("```") {
        text = rest.trim_end();
        changed = true;
    }

    // Character-based so multi-byte UTF-8 in a string argument survives.
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == ',' {
            let mut look = index + 1;
            while look < chars.len() && chars[look].is_whitespace() {
                look += 1;
            }
            if look < chars.len() && (chars[look] == '}' || chars[look] == ']') {
                index += 1;
                changed = true;
                continue;
            }
        }
        out.push(chars[index]);
        index += 1;
    }

    let out = out.trim();
    if !changed || out.is_empty() {
        return None;
    }
    Some(out.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_fence_is_stripped() {
        let repaired = repair_json("```json\n{\"path\":\"a.rs\"}\n```").unwrap();
        assert_eq!(repaired, "{\"path\":\"a.rs\"}");
    }

    #[test]
    fn a_trailing_comma_is_dropped() {
        let repaired = repair_json("{\"items\":[1,2,],}").unwrap();
        assert_eq!(repaired, "{\"items\":[1,2]}");
    }

    #[test]
    fn valid_json_is_left_alone() {
        assert_eq!(repair_json("{\"path\":\"a.rs\"}"), None);
    }

    #[test]
    fn parse_repairs_then_succeeds() {
        let parsed = parse_tool_arguments("edit_file", "{\"path\":\"a.rs\",}", true).unwrap();
        assert_eq!(parsed["path"], "a.rs");
    }

    #[test]
    fn parse_without_repair_reports_the_error() {
        let err = parse_tool_arguments("edit_file", "{\"path\":\"a.rs\",}", false).unwrap_err();
        assert!(matches!(err, Error::ToolArgs { .. }), "unexpected: {err}");
    }

    #[test]
    fn an_empty_argument_string_is_an_empty_object() {
        assert_eq!(
            parse_tool_arguments("recall", "", true).unwrap(),
            serde_json::json!({})
        );
    }
}
