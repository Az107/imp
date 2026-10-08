//! `imp mcp list`, `imp mcp tools <server>` and `imp mcp serve` (§5.12).
//!
//! The first two are read-only views of `[mcp.client.servers.*]`, and both
//! contact the servers to answer: a tool list is not something a config can
//! state, so "configured servers + discovered tools" means spawning them. A
//! server that will not start is reported, not fatal — the same degradation a
//! turn gets.
//!
//! `mcp serve` is the other direction: it publishes imp to an MCP host over
//! stdio and lives in [`crate::mcp_serve`]. It never returns to the REPL, which
//! is the whole of R5 — the two would otherwise fight over stdin/stdout.

use std::path::Path;
use std::process::ExitCode;

use imp_core::config::{Config, Decision, McpServerConfig};
use imp_core::error::{Error, Result};
use imp_mcp::{McpServers, OnStart};

use crate::cli::{Cli, McpAction, McpArgs};

/// Run a `imp mcp` subcommand.
pub async fn run(cli: &Cli, config: &Config, cwd: &Path, args: McpArgs) -> Result<ExitCode> {
    match args.action {
        McpAction::Serve { stdio } => crate::mcp_serve::run(cli, config, cwd, stdio).await,
        action => {
            let servers = McpServers::new(&config.mcp.client, config.exec.output_cap_bytes);
            match action {
                McpAction::List => list(cli, &servers).await,
                McpAction::Tools { server } => tools(cli, &servers, &server).await,
                McpAction::Serve { .. } => unreachable!("handled above"),
            }
        }
    }
}

/// Every configured server, with its state and the tools it publishes.
async fn list(cli: &Cli, servers: &McpServers) -> Result<ExitCode> {
    let configured: Vec<(String, McpServerConfig)> = servers
        .configured()
        .map(|(name, config)| (name.to_string(), config.clone()))
        .collect();
    if configured.is_empty() {
        if cli.json {
            println!("{}", serde_json::json!({ "type": "mcp", "servers": [] }));
        } else {
            println!("No MCP servers configured. Add one under [mcp.client.servers.<name>].");
        }
        return Ok(ExitCode::SUCCESS);
    }

    // A `lazy` server is contacted too: the operator asked about it by name, so
    // this is the "first use" its config was waiting for.
    servers.refresh(OnStart::All).await;

    if cli.json {
        for (name, server) in &configured {
            println!(
                "{}",
                serde_json::json!({
                    "type": "mcp_server",
                    "name": name,
                    "command": server.command,
                    "args": server.args,
                    "url": server.url,
                    "lazy": server.lazy,
                    "approval": server.approval.map(decision_name),
                    "tool_allow": server.tool_allow,
                    "status": status_of(servers, name),
                    "error": servers.failure(name),
                    "published": published_names(servers, name),
                    "discovered": discovered_names(servers, name),
                })
            );
        }
        servers.shutdown().await;
        return Ok(ExitCode::SUCCESS);
    }

    for (name, config) in &configured {
        println!("{name}  {}", status_of(servers, name));
        if config.is_http() {
            println!("  url: {}", config.url);
        } else {
            println!("  command: {} {}", config.command, config.args.join(" "));
        }
        println!(
            "  approval: {}  lazy: {}  tool_allow: [{}]",
            config
                .approval
                .map(decision_name)
                .unwrap_or_else(|| "global".to_string()),
            config.lazy,
            config.tool_allow.join(", ")
        );
        if let Some(error) = servers.failure(name) {
            println!("  {error}");
        }
        let published = published_names(servers, name);
        for tool in &published {
            println!("  ▸ {tool}");
        }
        if servers.is_up(name) {
            let hidden = discovered_names(servers, name).len() - published.len();
            if hidden > 0 {
                println!("  ({hidden} tool(s) hidden by tool_allow)");
            }
        }
    }

    servers.shutdown().await;
    Ok(ExitCode::SUCCESS)
}

/// Everything one server lists, and what imp publishes of it.
async fn tools(cli: &Cli, servers: &McpServers, server: &str) -> Result<ExitCode> {
    let Some((name, config)) = servers
        .configured()
        .find(|(name, _)| *name == server)
        .map(|(name, config)| (name.to_string(), config.clone()))
    else {
        return Err(Error::Config(format!(
            "no MCP server named `{server}` is configured"
        )));
    };

    // A failure is reported, not fatal: the command's job is to describe what
    // is there, and "nothing, because the server is down" is a description.
    if let Err(error) = servers.connect(&name).await {
        eprintln!("imp: MCP server `{name}` is unavailable: {error}");
        servers.shutdown().await;
        return Ok(ExitCode::from(4));
    }

    let discovered = servers.discovered(&name);

    if cli.json {
        for info in &discovered {
            println!(
                "{}",
                serde_json::json!({
                    "type": "mcp_tool",
                    "server": name,
                    "name": info.name,
                    "published_as": McpServerConfig::flattened(&name, &info.name),
                    "allowed": config.allows(&info.name),
                    "description": info.description,
                    "input_schema": info.input_schema,
                })
            );
        }
        servers.shutdown().await;
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "{} — {} tool(s) listed, {} published",
        name,
        discovered.len(),
        servers.published(&name).len()
    );
    for info in &discovered {
        let allowed = config.allows(&info.name);
        println!(
            "  {} {}  {}",
            if allowed { "▸" } else { "·" },
            McpServerConfig::flattened(&name, &info.name),
            if allowed {
                info.description.clone().unwrap_or_default()
            } else {
                "hidden by tool_allow".to_string()
            }
        );
        if allowed {
            println!(
                "      {}",
                serde_json::to_string(&info.input_schema).unwrap_or_default()
            );
        }
    }

    servers.shutdown().await;
    Ok(ExitCode::SUCCESS)
}

fn status_of(servers: &McpServers, name: &str) -> String {
    if servers.is_up(name) {
        "up".to_string()
    } else {
        "down".to_string()
    }
}

fn published_names(servers: &McpServers, name: &str) -> Vec<String> {
    servers
        .published(name)
        .iter()
        .map(|tool| tool.name().to_string())
        .collect()
}

fn discovered_names(servers: &McpServers, name: &str) -> Vec<String> {
    servers
        .discovered(name)
        .iter()
        .map(|info| info.name.clone())
        .collect()
}

fn decision_name(decision: Decision) -> String {
    match decision {
        Decision::Auto => "auto",
        Decision::Ask => "ask",
        Decision::Deny => "deny",
    }
    .to_string()
}
