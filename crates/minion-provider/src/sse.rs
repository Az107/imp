//! Frame decoder for `text/event-stream` responses.
//!
//! Kept separate from the HTTP client so the parsing rules can be tested
//! without a network or a runtime.

/// Incremental SSE parser.
///
/// Bytes arrive in arbitrary chunks; [`SseDecoder::push`] returns only the
/// `data` payloads that are complete. Per the SSE specification, a payload
/// spans multiple `data:` lines joined by newlines, comment lines (keepalives)
/// are ignored, and the `[DONE]` sentinel is passed through as a normal payload
/// for the caller to interpret.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: String,
    data: Vec<String>,
}

impl SseDecoder {
    /// An empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of text, returning any payloads that are now complete.
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buffer.push_str(chunk);
        let mut payloads = Vec::new();

        while let Some(newline) = self.buffer.find('\n') {
            let mut line = self.buffer[..newline].to_string();
            self.buffer.drain(..=newline);
            if line.ends_with('\r') {
                line.pop();
            }
            self.dispatch(&line, &mut payloads);
        }

        payloads
    }

    /// Flush any payload left behind by a stream that ended without a blank line.
    pub fn finish(&mut self) -> Option<String> {
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            let mut sink = Vec::new();
            self.dispatch(line.trim_end_matches('\r'), &mut sink);
            return sink.pop();
        }
        self.take_data()
    }

    fn dispatch(&mut self, line: &str, payloads: &mut Vec<String>) {
        if line.is_empty() {
            if let Some(payload) = self.take_data() {
                payloads.push(payload);
            }
            return;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            self.data
                .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
        // `event:`, `id:`, `retry:`, and `:comment` lines carry nothing we need.
    }

    fn take_data(&mut self) -> Option<String> {
        if self.data.is_empty() {
            return None;
        }
        Some(self.data.drain(..).collect::<Vec<_>>().join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_single_event() {
        let mut decoder = SseDecoder::new();
        let payloads = decoder.push("data: {\"a\":1}\n\n");
        assert_eq!(payloads, vec!["{\"a\":1}".to_string()]);
    }

    #[test]
    fn reassembles_events_split_across_chunks() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push("data: {\"cho").is_empty());
        assert!(decoder.push("ices\":[]}").is_empty());
        let payloads = decoder.push("\n\n");
        assert_eq!(payloads, vec!["{\"choices\":[]}".to_string()]);
    }

    #[test]
    fn handles_crlf_line_endings() {
        let mut decoder = SseDecoder::new();
        let payloads = decoder.push("data: hello\r\n\r\n");
        assert_eq!(payloads, vec!["hello".to_string()]);
    }

    #[test]
    fn ignores_comments_and_keepalives() {
        let mut decoder = SseDecoder::new();
        let payloads = decoder.push(": keepalive\n\ndata: real\n\n");
        assert_eq!(payloads, vec!["real".to_string()]);
    }

    #[test]
    fn joins_multiline_data_fields() {
        let mut decoder = SseDecoder::new();
        let payloads = decoder.push("data: one\ndata: two\n\n");
        assert_eq!(payloads, vec!["one\ntwo".to_string()]);
    }

    #[test]
    fn passes_the_done_sentinel_through() {
        let mut decoder = SseDecoder::new();
        let payloads = decoder.push("data: [DONE]\n\n");
        assert_eq!(payloads, vec!["[DONE]".to_string()]);
    }

    #[test]
    fn yields_several_events_from_one_chunk() {
        let mut decoder = SseDecoder::new();
        let payloads = decoder.push("data: a\n\ndata: b\n\n");
        assert_eq!(payloads, vec!["a".to_string(), "b".to_string()]);
    }
}
