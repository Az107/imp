//! An external tool, wearing minion's [`Tool`] interface.
//!
//! Everything an external tool *is* comes from the server: its name, its
//! description and its argument schema. What minion decides is how it is named
//! (`mcp__<server>__<tool>`), how dangerous it is, and how big a result it may
//! return.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use minion_core::error::{Error, Result};
use minion_core::tool::{Risk, Tool, ToolCtx, ToolOutput};

use crate::client::McpClient;

/// Wall-clock budget for one external call.
///
/// Longer than the default 30 s because the call crosses a process boundary and
/// the server may be doing real work; short enough that a wedged server cannot
/// hang a turn indefinitely (§5.4).
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// One tool published by an external server.
pub struct McpTool {
    /// Name of the server that owns it.
    pub server: String,
    /// The server's own name for it, used on the wire.
    pub tool: String,
    name: &'static str,
    description: &'static str,
    schema: Value,
    client: Arc<McpClient>,
    cap: usize,
}

impl McpTool {
    /// Build a tool from what the server advertised.
    ///
    /// `name` and `description` are interned by the caller: [`Tool::name`]
    /// returns `&'static str`, so a name known only at runtime has to live for
    /// the process. Interning bounds that to one allocation per distinct name
    /// rather than one per reconnect.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        server: String,
        tool: String,
        name: &'static str,
        description: &'static str,
        schema: Value,
        client: Arc<McpClient>,
        cap: usize,
    ) -> Self {
        Self {
            server,
            tool,
            name,
            description,
            schema,
            client,
            cap,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    /// Always at least `Network`.
    ///
    /// §5.5 says an external tool inherits the target's declared risk with
    /// `Network` as the floor, but MCP risk is expressed in `ToolAnnotations` —
    /// hints from a server the operator has not vouched for, whose own
    /// documentation says a client must not make tool-use decisions from them.
    /// With the floor at `Network` an inherited hint could only ever *lower* the
    /// class, so the honest reading is the floor itself: every external tool is
    /// at least as gated as an outbound request, and the gate treats `Network`
    /// like a write (D15). See T6.
    fn risk(&self) -> Risk {
        Risk::Network
    }

    fn timeout(&self) -> Duration {
        CALL_TIMEOUT
    }

    async fn invoke(&self, _ctx: ToolCtx, args: Value) -> Result<ToolOutput> {
        let outcome = self.client.call(&self.tool, args, self.cap).await?;

        let metadata = serde_json::json!({
            "server": self.server,
            "tool": self.tool,
            "truncated": outcome.truncated,
        });

        // A server-reported error is a tool error: the agent has one path for
        // "this did not work", and the model reads the server's own words.
        if outcome.is_error {
            return Err(Error::Tool {
                tool: self.name.to_string(),
                message: outcome.content,
            });
        }

        Ok(ToolOutput {
            truncated: outcome.truncated,
            content: outcome.content,
            metadata,
        })
    }
}
