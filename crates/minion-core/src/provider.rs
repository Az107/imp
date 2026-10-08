//! Provider abstraction: the model-facing surface the agent loop depends on.

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::message::Message;

/// A tool advertised to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    /// Registered tool name.
    pub name: String,
    /// Description the model uses to decide when to call the tool.
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: serde_json::Value,
}

impl ToolSchema {
    /// The OpenAI Chat Completions wire shape for a single tool.
    ///
    /// The API requires each entry of `tools` to be
    /// `{"type":"function","function":{"name":…,"description":…,"parameters":…}}`.
    /// Serializing [`ToolSchema`] directly produces a flattened object that
    /// compliant providers reject with a 400.
    pub fn to_wire(&self) -> serde_json::Value {
        self.to_wire_strict(false)
    }

    /// The wire shape, optionally asking the backend to constrain the arguments
    /// to the schema.
    ///
    /// When `strict`, the function carries `"strict": true`. That is what
    /// OpenAI-style strict function calling and Ollama's structured outputs read
    /// to force the arguments to validate; a backend that does not understand
    /// the field sees exactly today's request, since the flag is simply absent
    /// (M10.3, `[provider] strict_tool_arguments`). It is a hint to the backend,
    /// not a check of our own: the arguments are still parsed and validated by
    /// the tool layer, strict or not.
    pub fn to_wire_strict(&self, strict: bool) -> serde_json::Value {
        let mut function = serde_json::json!({
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters,
        });
        if strict {
            function["strict"] = serde_json::json!(true);
        }
        serde_json::json!({
            "type": "function",
            "function": function,
        })
    }
}

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// Natural stop.
    Stop,
    /// Hit `max_tokens`.
    Length,
    /// The model wants one or more tools invoked.
    ToolCalls,
    /// The provider's content filter intervened.
    ContentFilter,
    /// Any other or unrecognized reason.
    Other,
}

impl FinishReason {
    /// Map the wire value, defaulting to [`FinishReason::Other`].
    pub fn from_wire(value: &str) -> Self {
        match value {
            "stop" => Self::Stop,
            "length" => Self::Length,
            "tool_calls" => Self::ToolCalls,
            "content_filter" => Self::ContentFilter,
            _ => Self::Other,
        }
    }
}

/// Token accounting reported by the provider.
///
/// Advisory only: compatible providers may omit usage or report zeros, and no
/// turn should be blocked on a missing value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Tokens in the prompt.
    pub prompt_tokens: u32,
    /// Tokens generated.
    pub completion_tokens: u32,
    /// Sum as reported by the provider.
    pub total_tokens: u32,
}

impl Usage {
    /// Accumulate another usage report into this one.
    pub fn absorb(&mut self, other: Usage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
    }
}

/// A single request to the chat completions endpoint.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// Model identifier.
    pub model: String,
    /// Full conversation so far, oldest first.
    pub messages: Vec<Message>,
    /// Tools the model may call. Empty disables tool use.
    pub tools: Vec<ToolSchema>,
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Upper bound on generated tokens.
    pub max_tokens: Option<u32>,
    /// Request multiple tool calls per assistant turn.
    pub parallel_tool_calls: Option<bool>,
    /// Ask the provider to report usage on the final stream chunk.
    pub include_usage: bool,
}

/// An incremental event from a streaming completion.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// A fragment of assistant text.
    TextDelta(String),
    /// A fragment of a tool call. Fragments sharing an `index` are one call.
    ToolCallDelta {
        /// Position of the call within the assistant message.
        index: usize,
        /// Provider call id, present on the first fragment.
        id: Option<String>,
        /// Function name, present on the first fragment.
        name: Option<String>,
        /// Raw JSON fragment to be concatenated with its siblings.
        arguments: String,
    },
    /// Token accounting, usually only at the end of the stream.
    Usage(Usage),
    /// Terminal event carrying the reason generation stopped.
    Done {
        /// Why the model stopped.
        finish_reason: FinishReason,
    },
}

/// A streaming chat-completions backend.
///
/// Implementations must not emit any output before the request is accepted, so
/// that retries are safe: the agent loop treats a mid-stream error as terminal.
pub trait Provider: Send + Sync {
    /// Start a streaming completion. Returns a stream of events that ends after
    /// [`ChatEvent::Done`] or an error.
    fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> BoxStream<'static, Result<ChatEvent>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> ToolSchema {
        ToolSchema {
            name: "read_file".to_string(),
            description: "read".to_string(),
            parameters: serde_json::json!({ "type": "object" }),
        }
    }

    #[test]
    fn the_wire_shape_is_the_function_envelope() {
        let wire = schema().to_wire();
        assert_eq!(wire["type"], "function");
        assert_eq!(wire["function"]["name"], "read_file");
        assert!(wire.get("name").is_none(), "the flat form must not leak");
    }

    #[test]
    fn strict_is_absent_unless_asked_for() {
        assert!(
            schema()
                .to_wire()
                .get("function")
                .unwrap()
                .get("strict")
                .is_none()
        );
    }

    #[test]
    fn strict_adds_the_flag_inside_the_function_envelope() {
        let wire = schema().to_wire_strict(true);
        assert_eq!(wire["function"]["strict"], serde_json::json!(true));
        assert_eq!(wire["type"], "function", "the envelope is unchanged");
        assert_eq!(wire["function"]["name"], "read_file");
    }
}
