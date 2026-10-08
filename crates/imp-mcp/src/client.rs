//! One live connection to one external MCP server, over stdio or Streamable
//! HTTP (SDD §5.10, D25).
//!
//! Everything protocol-shaped stops here: the rest of the crate works in terms
//! of a tool list and a text result, so nothing above this file has to know that
//! `rmcp` exists — or which transport carried the call.

use std::sync::Arc;

use rmcp::RoleClient;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::Value;
use tokio::sync::Mutex;

use imp_core::error::{Error, Result};

/// One tool as the server describes it.
///
/// `input_schema` is kept as the server sent it. imp does not rewrite a
/// third-party schema (§5.10): the model sees the server's own contract, and a
/// schema imp cannot parse is the server's problem to fix, not something to
/// be silently normalised into a different one.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInfo {
    /// The server's own name for the tool, without the `mcp__…__` prefix.
    pub name: String,
    /// What the server says the tool does.
    pub description: Option<String>,
    /// The server's `inputSchema`, verbatim.
    pub input_schema: Value,
}

/// What a tool call returned.
#[derive(Debug, Clone, PartialEq)]
pub struct CallOutcome {
    /// The result rendered as text, capped.
    pub content: String,
    /// Whether [`content`](Self::content) was cut to fit the cap.
    pub truncated: bool,
    /// Whether the server reported the call itself as an error.
    pub is_error: bool,
}

/// A spawned server, and the transport that talks to it.
///
/// Dropping the value kills the child (`TokioChildProcess`'s drop guard), which
/// is what makes a session that ends leave nothing behind.
pub struct McpClient {
    server: String,
    /// Serialises calls and lets `close` take `&mut`. The agent runs tool calls
    /// one at a time, so the lock costs nothing and keeps one writer per pipe.
    service: Mutex<RunningService<RoleClient, ()>>,
}

impl McpClient {
    /// Spawn `command` and complete the MCP handshake.
    ///
    /// The program is executed directly, never through a shell, so an argument
    /// from the config cannot become a second command.
    pub async fn connect(server: &str, command: &str, args: &[String]) -> Result<Arc<Self>> {
        let arguments = args.to_vec();
        let transport =
            TokioChildProcess::new(tokio::process::Command::new(command).configure(|cmd| {
                cmd.args(&arguments);
            }))
            .map_err(|err| {
                Error::Config(format!(
                    "MCP server `{server}`: cannot spawn `{command}`: {err}"
                ))
            })?;

        let service = ().serve(transport).await.map_err(|err| {
            Error::Config(format!(
                "MCP server `{server}`: the handshake failed: {err}"
            ))
        })?;

        Ok(Arc::new(Self {
            server: server.to_string(),
            service: Mutex::new(service),
        }))
    }

    /// Reach a server over Streamable HTTP and complete the MCP handshake.
    ///
    /// `url` is the endpoint — `http://host:port/mcp` — and `token`, when
    /// present, is sent as `Authorization: Bearer <token>` on every request.
    ///
    /// No TLS is configured, deliberately: inside a tailnet the traffic is
    /// already encrypted by WireGuard, and the token — not the source address —
    /// is what authenticates the caller, because any process on a tailnet node
    /// can reach that port (D26). An `https://` URL would still work if a future
    /// decision added certificates; nothing here forbids one.
    pub async fn connect_http(server: &str, url: &str, token: Option<&str>) -> Result<Arc<Self>> {
        let mut config = StreamableHttpClientTransportConfig::with_uri(url);
        if let Some(token) = token {
            config = config.auth_header(token);
        }
        let transport = StreamableHttpClientTransport::with_client(reqwest::Client::new(), config);

        let service = ().serve(transport).await.map_err(|err| {
            Error::Config(format!(
                "MCP server `{server}`: the HTTP handshake failed: {err}"
            ))
        })?;

        Ok(Arc::new(Self {
            server: server.to_string(),
            service: Mutex::new(service),
        }))
    }

    /// The server's tool list.
    pub async fn list_tools(&self) -> Result<Vec<ToolInfo>> {
        let service = self.service.lock().await;
        let result = service.peer().list_tools(None).await.map_err(|err| {
            Error::Config(format!(
                "MCP server `{}`: tools/list failed: {err}",
                self.server
            ))
        })?;

        Ok(result
            .tools
            .into_iter()
            .map(|tool| ToolInfo {
                name: tool.name.to_string(),
                description: tool.description.as_ref().map(|text| text.to_string()),
                input_schema: Value::Object((*tool.input_schema).clone()),
            })
            .collect())
    }

    /// Call one tool and render what came back.
    ///
    /// `cap` bounds the text handed to the model; a result larger than it is cut
    /// and marked, rather than filling the transcript (§5.5, T7).
    pub async fn call(&self, tool: &str, args: Value, cap: usize) -> Result<CallOutcome> {
        let mut params = CallToolRequestParams::new(tool.to_string());
        match args {
            Value::Object(object) => params = params.with_arguments(object),
            Value::Null => {}
            other => {
                return Err(Error::ToolArgs {
                    tool: format!("{}__{tool}", self.server),
                    message: format!("arguments must be a JSON object, got {other}"),
                });
            }
        }

        let service = self.service.lock().await;
        let result = service
            .peer()
            .call_tool(params)
            .await
            .map_err(|err| Error::Tool {
                tool: format!("{}__{tool}", self.server),
                message: format!("MCP server `{}`: {err}", self.server),
            })?;

        let (content, truncated) = render(&result.content, cap);
        Ok(CallOutcome {
            content,
            truncated,
            is_error: result.is_error.unwrap_or(false),
        })
    }

    /// Close the transport and wait for the child to exit.
    ///
    /// Best effort: a server that ignores the shutdown is killed by the
    /// transport's own drop guard.
    pub async fn close(&self) {
        let mut service = self.service.lock().await;
        if let Err(err) = service.close().await {
            tracing::debug!(server = %self.server, error = %err, "the MCP server did not close cleanly");
        }
    }
}

/// Flatten MCP content blocks into the text a tool result carries.
///
/// A block imp cannot use is described rather than dropped: silently
/// discarding an image would make a server look like it returned nothing.
fn render(content: &[ContentBlock], cap: usize) -> (String, bool) {
    let mut text = String::new();
    for block in content {
        let piece = match block {
            ContentBlock::Text(block) => block.text.clone(),
            ContentBlock::Image(block) => {
                format!("[image {} · {} bytes]", block.mime_type, block.data.len())
            }
            ContentBlock::Audio(block) => {
                format!("[audio {} · {} bytes]", block.mime_type, block.data.len())
            }
            ContentBlock::Resource(block) => match &block.resource {
                rmcp::model::ResourceContents::TextResourceContents { uri, text, .. } => {
                    format!("[{uri}]\n{text}")
                }
                rmcp::model::ResourceContents::BlobResourceContents { uri, blob, .. } => {
                    format!("[{uri} · {} bytes]", blob.len())
                }
                _ => "[resource]".to_string(),
            },
            ContentBlock::ResourceLink(link) => format!("[{}]({})", link.name, link.uri),
            other => format!("[unsupported content: {other:?}]"),
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&piece);
    }

    cap_text(text, cap)
}

/// Cut `text` to `cap` bytes on a character boundary, marking the cut.
///
/// A cap that splits a code point would produce a string the provider rejects
/// outright, so the boundary is searched for rather than assumed. `truncated`
/// is `true` only when something was actually removed.
pub fn cap_text(mut text: String, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text, false);
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n… [truncated to the output cap]");
    (text, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::TextContent;

    fn text(value: &str) -> ContentBlock {
        ContentBlock::Text(TextContent::new(value))
    }

    #[test]
    fn several_blocks_become_one_text_result() {
        let (rendered, truncated) = render(&[text("one"), text("two")], 1024);
        assert_eq!(rendered, "one\ntwo");
        assert!(!truncated);
    }

    #[test]
    fn a_non_text_block_is_described_not_dropped() {
        let image = ContentBlock::image("AAAA", "image/png");

        let (rendered, _) = render(&[image], 1024);

        assert!(rendered.contains("image/png"), "was: {rendered}");
    }

    #[test]
    fn an_oversized_result_is_cut_on_a_character_boundary() {
        let body = "é".repeat(40);
        let (rendered, truncated) = render(&[text(&body)], 21);

        assert!(truncated);
        assert!(rendered.starts_with("ééé"), "was: {rendered}");
        assert!(rendered.contains("truncated"), "was: {rendered}");
    }
}
