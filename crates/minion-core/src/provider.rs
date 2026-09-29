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
