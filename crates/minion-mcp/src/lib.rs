//! Consuming external MCP servers (SDD §5.10, G6, T6).
//!
//! The client half of MCP. [`McpServers`] owns every configured server, spawns
//! each over stdio, publishes the tools it lists as `mcp__<server>__<tool>`, and
//! implements [`minion_core::tool::ToolCatalog`] so a registry built once follows
//! servers that come up, fall over and are retried.
//!
//! Two properties are the point of the module, and both are enforced by code
//! rather than by convention:
//!
//! - an external tool is an ordinary [`minion_core::tool::Tool`], so it passes
//!   the same approval gate as `run_command` — there is no path that reaches a
//!   server without a policy decision;
//! - a catalogue tool never shadows a registered one, because the registry skips
//!   a name that is already taken (§5.10).

mod client;
mod servers;
mod tool;

pub use client::{CallOutcome, McpClient, ToolInfo};
pub use servers::{McpServers, OnStart};
pub use tool::McpTool;
