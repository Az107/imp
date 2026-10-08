//! A tiny, protocol-correct MCP server over stdio, for tests and for trying the
//! client against something real.
//!
//! It is deliberately *not* a useful server: what it is useful for is the parts
//! of the client that are hard to exercise otherwise. Its tool list includes a
//! schema with a non-standard keyword (so a test can prove minion passes a
//! third-party schema through untouched), a tool whose name collides with a
//! built-in (so a test can prove built-ins are not shadowed), and a tool that
//! reports an error (so a test can prove an error result is not a normal one).
//!
//! Say nothing on stdout but protocol.

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, JsonObject,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, serve_server};
use rmcp::{ErrorData, RoleServer, ServerHandler};

/// The tool list every run answers `tools/list` with.
fn tools() -> Vec<Tool> {
    let echo: JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": { "text": { "type": "string" } },
        "required": ["text"],
        "additionalProperties": false,
        "x-stub-marker": "passed through verbatim"
    }))
    .expect("a literal object");

    let read_file: JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": { "path": { "type": "string" } }
    }))
    .expect("a literal object");

    let explode: JsonObject =
        serde_json::from_value(serde_json::json!({ "type": "object" })).expect("a literal object");

    let secret: JsonObject =
        serde_json::from_value(serde_json::json!({ "type": "object" })).expect("a literal object");

    let agent_ask: JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": {
            "prompt": { "type": "string" },
            "max_tokens": { "type": "integer" }
        },
        "required": ["prompt"]
    }))
    .expect("a literal object");

    vec![
        Tool::new("echo", "Echo the `text` argument back.", echo),
        // Same name as a built-in tool, on purpose.
        Tool::new(
            "read_file",
            "A server-side file read, not minion's.",
            read_file,
        ),
        Tool::new("explode", "Always reports an error.", explode),
        Tool::new(
            "secret",
            "A tool a config may hide with tool_allow.",
            secret,
        ),
        // What a peer exposes: this is the tool `peer__<name>_ask` calls (M10.2).
        Tool::new(
            "agent_ask",
            "Stand-in for a minion peer: answers a brief with JSON, like the real server.",
            agent_ask,
        ),
    ]
}

struct Stub;

impl ServerHandler for Stub {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = request.arguments.clone().unwrap_or_default();
        let result = match request.name.as_ref() {
            "echo" => {
                let text = arguments
                    .get("text")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string();
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            "read_file" => CallToolResult::success(vec![ContentBlock::text(
                "the stub server's read_file, not minion's",
            )]),
            "explode" => CallToolResult::error(vec![ContentBlock::text(
                "the stub server refused this call",
            )]),
            "secret" => CallToolResult::success(vec![ContentBlock::text("secret reached")]),
            "agent_ask" => {
                let prompt = arguments
                    .get("prompt")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                if prompt.contains("explode") {
                    return Ok(CallToolResponse::from(CallToolResult::error(vec![
                        ContentBlock::text("the stub peer could not answer the brief"),
                    ])));
                }
                // A deliberately large answer, so the caller's size cap can be
                // exercised without a real model.
                let text = if prompt.contains("huge") {
                    "x".repeat(9_000)
                } else {
                    format!("brief received: {prompt}")
                };
                let answer = serde_json::json!({
                    "session_id": "stub-session",
                    "stop": "completed",
                    "text": text,
                    "iterations": 1,
                    "usage": {
                        "prompt_tokens": 11,
                        "completion_tokens": 22,
                        "total_tokens": 33
                    }
                });
                CallToolResult::success(vec![ContentBlock::text(answer.to_string())])
            }
            other => CallToolResult::error(vec![ContentBlock::text(format!(
                "the stub server has no tool `{other}`"
            ))]),
        };
        Ok(CallToolResponse::from(result))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let service = serve_server(Stub, (tokio::io::stdin(), tokio::io::stdout())).await?;
    // Run until the client closes the pipe, then exit rather than linger.
    service.waiting().await?;
    Ok(())
}
