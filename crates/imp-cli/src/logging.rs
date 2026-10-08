//! Log sinks and redaction (NFR-9, §7).
//!
//! Two things happen here before a byte reaches stderr or the log file:
//!
//! 1. Any value that looks like a credential is replaced with a fixed marker —
//!    the resolved API key, the value of the configured key environment
//!    variable, any `[provider.headers]` value whose name looks like a secret,
//!    and the token half of any `Bearer <token>` pair.
//! 2. The line is emitted to stderr, and to `logging.file` when one is set.
//!
//! Redaction is a property of the sink, not of each call site: a `tracing`
//! macro cannot be trusted to remember, so the writer scrubs every line it is
//! handed, whoever emitted it. That is what makes NFR-9 a guarantee rather than
//! a convention.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use imp_core::config::Config;

/// The text a masked value is replaced with.
pub const MASK: &str = "[redacted]";

/// Values that must never reach a log sink.
#[derive(Debug, Clone, Default)]
pub struct Secrets {
    values: Vec<String>,
}

impl Secrets {
    /// Gather every credential the configuration can name.
    ///
    /// The *names* of environment variables are not secrets and are left alone;
    /// only the values they hold are masked.
    pub fn from_config(config: &Config) -> Self {
        let mut secrets = Self::default();
        if let Some(key) = config.api_key().ok().flatten() {
            secrets.push(&key);
        }
        let env_name = config.provider.api_key_env.trim();
        if !env_name.is_empty()
            && let Ok(value) = std::env::var(env_name)
        {
            secrets.push(&value);
        }
        for (name, value) in &config.provider.headers {
            if looks_secret(name) {
                secrets.push(value);
            }
        }
        secrets
    }

    /// Add one candidate, ignoring the empty and the trivially short.
    ///
    /// A three-character value matches half the log by accident, so redacting
    /// it would mangle every line for no security gain.
    pub fn push(&mut self, value: &str) {
        let value = value.trim();
        if value.len() >= 4 && !self.values.iter().any(|known| known == value) {
            self.values.push(value.to_string());
        }
    }

    /// The secrets, longest first so a short one cannot mask a longer match
    /// leaving a tail of the longer value behind.
    fn ordered(&self) -> Vec<String> {
        let mut values = self.values.clone();
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values
    }

    /// Share the list with a writer.
    pub fn into_shared(self) -> Arc<Vec<String>> {
        Arc::new(self.ordered())
    }
}

/// Whether a header name suggests its value is a credential.
fn looks_secret(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "authorization",
        "auth",
        "token",
        "key",
        "secret",
        "cookie",
        "password",
        "credential",
    ]
    .iter()
    .any(|needle| name.contains(needle))
}

/// Replace secrets and `Bearer` tokens in `text`.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut redacted = mask_bearer(text);
    for secret in secrets {
        if redacted.contains(secret.as_str()) {
            redacted = redacted.replace(secret.as_str(), MASK);
        }
    }
    redacted
}

/// Replace the token following any `Bearer` with [`MASK`].
///
/// Covers the common case where a key is not one this process knows by name —
/// an SDK prints it, or a header dump includes it — which the name-based list
/// cannot catch.
fn mask_bearer(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = find_bearer(rest) {
        let (before, tail) = rest.split_at(index);
        out.push_str(before);
        // `tail` starts at "Bearer"; keep the scheme and its separating space.
        let space_at = tail.find(char::is_whitespace).unwrap_or(tail.len());
        out.push_str(&tail[..space_at]);
        let after_space = &tail[space_at..];
        let token_at = after_space.len() - after_space.trim_start().len();
        out.push_str(&after_space[..token_at]);
        let token = after_space[token_at..].trim_start();
        let end = token
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';'))
            .unwrap_or(token.len());
        out.push_str(MASK);
        rest = &token[end..];
    }
    out.push_str(rest);
    out
}

/// Case-insensitively locate a `Bearer` whose token is worth masking.
fn find_bearer(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(offset) = lower[from..].find("bearer") {
        let at = from + offset;
        // Only a scheme boundary: "Bearer" as a word, not part of a longer one.
        let boundary = at == 0 || !text.as_bytes()[at - 1].is_ascii_alphanumeric();
        if boundary {
            return Some(at);
        }
        from = at + "bearer".len();
    }
    None
}

/// Builds one [`RedactingWriter`] per log event.
#[derive(Clone)]
pub struct RedactingMakeWriter {
    secrets: Arc<Vec<String>>,
    sink: Sink,
}

impl RedactingMakeWriter {
    /// Write to stderr, or to stderr and `path` at once.
    pub fn new(secrets: Arc<Vec<String>>, file: Option<&str>) -> Self {
        let sink = match file.filter(|path| !path.trim().is_empty()) {
            Some(path) => {
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map(Some);
                match file {
                    Ok(handle) => Sink::Both(Arc::new(Mutex::new(handle))),
                    Err(err) => {
                        // Logging to a file that cannot be opened must not take
                        // the process with it; say so once and use stderr.
                        eprintln!("imp: cannot open the log file {path}: {err}");
                        Sink::Stderr
                    }
                }
            }
            None => Sink::Stderr,
        };
        Self { secrets, sink }
    }
}

/// Where scrubbed lines go.
#[derive(Clone)]
enum Sink {
    Stderr,
    Both(Arc<Mutex<Option<std::fs::File>>>),
}

impl Sink {
    fn emit(&self, bytes: &[u8]) {
        let _ = io::stderr().write_all(bytes);
        if let Sink::Both(file) = self
            && let Ok(mut guard) = file.lock()
            && let Some(handle) = guard.as_mut()
        {
            let _ = handle.write_all(bytes);
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RedactingMakeWriter {
    type Writer = RedactingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            secrets: self.secrets.clone(),
            sink: self.sink.clone(),
            buffer: Vec::new(),
        }
    }
}

/// A writer that buffers whole lines, scrubs them, and forwards.
pub struct RedactingWriter {
    secrets: Arc<Vec<String>>,
    sink: Sink,
    buffer: Vec<u8>,
}

impl RedactingWriter {
    /// Emit every complete line currently buffered.
    fn drain_lines(&mut self) {
        while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=index).collect();
            self.emit(&line);
        }
    }

    fn emit(&self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        let scrubbed = redact(&text, &self.secrets);
        self.sink.emit(scrubbed.as_bytes());
    }
}

impl Write for RedactingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        self.drain_lines();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.drain_lines();
        // A writer may flush without a trailing newline; emit what is left so
        // nothing is silently dropped.
        if !self.buffer.is_empty() {
            let remaining = std::mem::take(&mut self.buffer);
            self.emit(&remaining);
        }
        Ok(())
    }
}

impl Drop for RedactingWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets(values: &[&str]) -> Vec<String> {
        let mut secrets = Secrets::default();
        for value in values {
            secrets.push(value);
        }
        secrets.into_shared().as_ref().clone()
    }

    #[test]
    fn a_bearer_token_is_masked() {
        let text = "Authorization: Bearer sk-live-abcdef0123456789 sent";

        let redacted = redact(text, &[]);

        assert_eq!(redacted, "Authorization: Bearer [redacted] sent");
        assert!(!redacted.contains("sk-live"));
    }

    #[test]
    fn a_known_secret_is_masked_wherever_it_appears() {
        let known = "sk-live-abcdef0123456789";

        let redacted = redact(&format!("key={known} again {known}"), &secrets(&[known]));

        assert_eq!(redacted, "key=[redacted] again [redacted]");
    }

    #[test]
    fn a_short_value_is_left_alone() {
        // "abc" happens to appear everywhere; masking it would be noise.
        let redacted = redact("abc abcd", &secrets(&["abc"]));
        assert_eq!(redacted, "abc abcd");
    }

    #[test]
    fn clean_text_is_untouched() {
        let text = "turn did the thing in 12 ms";
        assert_eq!(redact(text, &secrets(&["sk-secret-value"])), text);
    }

    #[test]
    fn a_secret_inside_a_bearer_token_is_masked_once() {
        let known = "sk-live-abcdef0123456789";
        let redacted = redact(&format!("Bearer {known}"), &secrets(&[known]));
        assert_eq!(redacted, "Bearer [redacted]");
    }

    #[test]
    fn the_writer_scrubs_the_text_it_emits() {
        // The sink is stderr, which the test cannot capture; what matters is
        // that the writer routes every line through `redact`, so assert on the
        // pure function the writer delegates to.
        let secrets = Secrets {
            values: vec!["sk-live-abcdef".to_string()],
        }
        .into_shared();
        assert_eq!(
            redact("token sk-live-abcdef\n", &secrets),
            "token [redacted]\n"
        );
    }

    #[test]
    fn a_header_that_looks_like_a_credential_is_collected() {
        assert!(looks_secret("Authorization"));
        assert!(looks_secret("x-api-key"));
        assert!(looks_secret("Cookie"));
        assert!(!looks_secret("x-opencode-session"));
    }
}
