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
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            },
        })
    }
}

/// Strip schema keywords that cost tokens and break strict local decoders.
///
/// `schemars` emits `$schema`, `title`, and `format`, and represents an
/// `Option<T>` as `"type": ["T","null"]`. OpenAI tolerates all of it, but
/// grammar-based local backends (llama.cpp's GBNF, some MLX paths) compile the
/// schema and can choke on the extras. Removing them is lossless for tool
/// calling: `description`, `enum`, `required` and numeric bounds are kept, and a
/// closed object gets `additionalProperties: false` so a grammar stays tight.
pub fn sanitize_schema(schema: &mut serde_json::Value) {
    match schema {
        serde_json::Value::Object(map) => {
            map.remove("$schema");
            map.remove("title");
            map.remove("format");

            // `["integer","null"]` -> `"integer"`; absence already means
            // nullable for an optional argument.
            if let Some(serde_json::Value::Array(types)) = map.get("type") {
                let mut non_null = types
                    .iter()
                    .filter(|kind| kind.as_str() != Some("null"))
                    .cloned();
                if let (Some(only), None) = (non_null.next(), non_null.next()) {
                    map.insert("type".to_string(), only);
                }
            }

            for key in ["properties", "patternProperties", "$defs", "definitions"] {
                if let Some(serde_json::Value::Object(subschemas)) = map.get_mut(key) {
                    for subschema in subschemas.values_mut() {
                        sanitize_schema(subschema);
                    }
                }
            }
            for key in [
                "items",
                "additionalProperties",
                "not",
                "if",
                "then",
                "else",
                "contains",
                "propertyNames",
            ] {
                if let Some(subschema) = map.get_mut(key) {
                    sanitize_schema(subschema);
                }
            }
            for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
                if let Some(serde_json::Value::Array(subschemas)) = map.get_mut(key) {
                    for subschema in subschemas.iter_mut() {
                        sanitize_schema(subschema);
                    }
                }
            }

            if map.contains_key("properties") && !map.contains_key("additionalProperties") {
                map.insert(
                    "additionalProperties".to_string(),
                    serde_json::Value::Bool(false),
                );
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sanitize_schema(item);
            }
        }
        _ => {}
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

    #[test]
    fn sanitize_strips_noise_and_collapses_nullable_types() {
        let mut schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "ReadFileArgs",
            "type": "object",
            "properties": {
                "path": { "type": "string", "format": "path" },
                "limit": { "type": ["integer", "null"], "minimum": 1 },
            },
            "required": ["path"],
        });

        sanitize_schema(&mut schema);

        assert!(schema.get("$schema").is_none());
        assert!(schema.get("title").is_none());
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert!(schema["properties"]["path"].get("format").is_none());
        assert_eq!(schema["properties"]["limit"]["type"], "integer");
        assert_eq!(schema["properties"]["limit"]["minimum"], 1);
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
        assert_eq!(schema["required"], serde_json::json!(["path"]));
    }

    #[test]
    fn sanitize_leaves_a_map_schema_open() {
        let mut schema = serde_json::json!({
            "type": "object",
            "additionalProperties": { "type": "string" },
        });

        sanitize_schema(&mut schema);

        assert_eq!(schema["additionalProperties"]["type"], "string");
    }

    #[test]
    fn sanitize_recurses_into_definitions() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": { "op": { "$ref": "#/$defs/Op" } },
            "$defs": { "Op": { "title": "Op", "enum": ["a", "b"] } },
        });

        sanitize_schema(&mut schema);

        assert!(schema["$defs"]["Op"].get("title").is_none());
        assert_eq!(schema["$defs"]["Op"]["enum"], serde_json::json!(["a", "b"]));
    }
}
