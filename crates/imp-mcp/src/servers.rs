//! The set of configured external servers, and the tool catalogue they feed.
//!
//! This is the piece that answers "what can the model call right now?". It owns
//! every connection, spawns servers on the schedule the config asks for, filters
//! what reaches the model, and turns a failure into a notice instead of an
//! error: a server that is down costs its tools, never the turn (§5.10).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use imp_core::config::{McpClientConfig, McpServerConfig};
use imp_core::error::{Error, Result};
use imp_core::policy::ToolPolicy;
use imp_core::tool::{Tool, ToolCatalog};

use crate::client::{McpClient, ToolInfo};
use crate::tool::McpTool;

/// Which servers a pass over the config is allowed to contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnStart {
    /// Everything except the servers configured `lazy`. Used while the session
    /// is assembled, so opening one does not spawn a process nobody asked for.
    Eager,
    /// Every configured server. Used at the start of a turn, which makes this
    /// the retry §5.10 promises for a server that was down last time.
    All,
}

/// What is known about one server.
#[derive(Clone)]
enum State {
    /// Connected, with the tools it published on the last successful listing.
    Up {
        client: Arc<McpClient>,
        /// Everything the server listed, before `tool_allow`.
        infos: Vec<ToolInfo>,
        /// The subset the model may see and call.
        tools: Vec<Arc<dyn Tool>>,
    },
    /// The last attempt failed, and why.
    Down(String),
}

/// A comparable summary of a state, for deciding whether anything changed.
#[derive(PartialEq, Eq)]
enum Snapshot {
    Up,
    Down(String),
}

/// Keeps one leaked copy of each runtime string.
///
/// [`Tool::name`] and [`Tool::description`] return `&'static str`, and a name
/// discovered at runtime has to live for the process. Interning bounds that to
/// one allocation per distinct string instead of one per reconnect, so a server
/// that flaps does not grow the heap on every turn.
#[derive(Default)]
struct Interner {
    strings: Mutex<HashMap<String, &'static str>>,
}

impl Interner {
    fn intern(&self, text: &str) -> &'static str {
        let mut strings = self.strings.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(existing) = strings.get(text) {
            return existing;
        }
        let leaked: &'static str = Box::leak(text.to_string().into_boxed_str());
        strings.insert(text.to_string(), leaked);
        leaked
    }
}

/// Every configured server, and what each one currently offers.
pub struct McpServers {
    servers: BTreeMap<String, McpServerConfig>,
    /// Output cap applied to one external result.
    cap: usize,
    state: Mutex<BTreeMap<String, State>>,
    strings: Interner,
}

impl McpServers {
    /// Build the set from `[mcp.client]`.
    ///
    /// `cap` is the byte budget for one external result, the same
    /// `exec.output_cap_bytes` the built-in tools honour.
    pub fn new(config: &McpClientConfig, cap: u64) -> Arc<Self> {
        Arc::new(Self {
            servers: config.servers.clone(),
            cap: cap as usize,
            state: Mutex::new(BTreeMap::new()),
            strings: Interner::default(),
        })
    }

    /// Every configured server, in name order, whether or not it is up.
    pub fn configured(&self) -> impl Iterator<Item = (&str, &McpServerConfig)> {
        self.servers
            .iter()
            .map(|(name, config)| (name.as_str(), config))
    }

    /// The approval families the engine needs, one per server that named a
    /// policy. A server without one is governed by the global policy and is
    /// deliberately absent here.
    pub fn policy_families(&self) -> Vec<ToolPolicy> {
        self.servers
            .iter()
            .filter_map(|(name, config)| {
                config
                    .approval
                    .map(|decision| ToolPolicy::new(McpServerConfig::tool_prefix(name), decision))
            })
            .collect()
    }

    /// Whether `name` is configured `lazy`.
    pub fn is_lazy(&self, name: &str) -> bool {
        self.servers.get(name).map(|c| c.lazy).unwrap_or(false)
    }

    /// Contact the servers `on` covers, and describe what changed.
    ///
    /// Only transitions are reported: a server that was already down with the
    /// same reason is silent the second time, so a turn does not repeat a notice
    /// the user has already read. The returned strings are meant to become
    /// `system` messages, which is how §5.10's "explains why" reaches the model
    /// and the transcript.
    pub async fn refresh(&self, on: OnStart) -> Vec<String> {
        let mut notices = Vec::new();

        for (name, config) in &self.servers {
            if on == OnStart::Eager && config.lazy {
                continue;
            }
            if self.snapshot(name) == Some(Snapshot::Up) {
                continue;
            }
            let before = self.snapshot(name);

            match self.establish(name, config).await {
                Ok((total, published)) => {
                    let hidden = total - published;
                    if before != Some(Snapshot::Up) {
                        notices.push(if published == 0 {
                            format!(
                                "MCP server `{name}` is available but publishes no tool: \
                                 tool_allow names none of its {total} tool(s)."
                            )
                        } else if hidden == 0 {
                            format!(
                                "MCP server `{name}` is available: {published} tool(s) published."
                            )
                        } else {
                            format!(
                                "MCP server `{name}` is available: {published} tool(s) published, \
                                 {hidden} hidden by tool_allow."
                            )
                        });
                    }
                }
                Err(err) => {
                    let reason = err.to_string();
                    if before != Some(Snapshot::Down(reason.clone())) {
                        notices.push(format!(
                            "MCP server `{name}` is unavailable ({reason}). Its tools are not in \
                             the catalogue, and it will be retried on the next turn."
                        ));
                    }
                }
            }
        }

        notices
    }

    /// Contact one server by name, whatever its `lazy` setting.
    ///
    /// Used by `imp mcp tools`, where the operator is asking about a server
    /// explicitly.
    pub async fn connect(&self, name: &str) -> Result<()> {
        let config = self
            .servers
            .get(name)
            .ok_or_else(|| Error::Config(format!("no MCP server named `{name}` is configured")))?;
        self.establish(name, config).await.map(|_| ())
    }

    /// Whether the server is connected right now.
    pub fn is_up(&self, name: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(name)
            .is_some_and(|state| matches!(state, State::Up { .. }))
    }

    /// Why the last attempt failed, if it did.
    pub fn failure(&self, name: &str) -> Option<String> {
        match self
            .state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(name)
        {
            Some(State::Down(reason)) => Some(reason.clone()),
            _ => None,
        }
    }

    /// Everything the server listed, before `tool_allow` is applied.
    pub fn discovered(&self, name: &str) -> Vec<ToolInfo> {
        match self.state(name) {
            Some(State::Up { infos, .. }) => infos,
            _ => Vec::new(),
        }
    }

    /// The tools the server publishes to the model.
    pub fn published(&self, name: &str) -> Vec<Arc<dyn Tool>> {
        match self.state(name) {
            Some(State::Up { tools, .. }) => tools,
            _ => Vec::new(),
        }
    }

    /// Close every connection, best effort.
    ///
    /// Called on the way out so a session does not leave an MCP server behind.
    /// The close hands the child back to the transport's drop guard, which kills
    /// it from a spawned task; the short pause is what lets that task run before
    /// the runtime goes away.
    pub async fn shutdown(&self) {
        let clients: Vec<Arc<McpClient>> = self
            .state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .values()
            .filter_map(|state| match state {
                State::Up { client, .. } => Some(client.clone()),
                State::Down(_) => None,
            })
            .collect();

        if clients.is_empty() {
            return;
        }
        for client in &clients {
            client.close().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

impl McpServers {
    fn state(&self, name: &str) -> Option<State> {
        self.state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(name)
            .cloned()
    }

    fn snapshot(&self, name: &str) -> Option<Snapshot> {
        self.state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(name)
            .map(|state| match state {
                State::Up { .. } => Snapshot::Up,
                State::Down(reason) => Snapshot::Down(reason.clone()),
            })
    }

    /// Connect, list, filter, and record. Returns `(total, published)`.
    async fn establish(&self, name: &str, config: &McpServerConfig) -> Result<(usize, usize)> {
        let client = self.attempt(name, config).await?;
        let infos = client.list_tools().await?;

        let tools: Vec<Arc<dyn Tool>> = infos
            .iter()
            .filter(|info| config.allows(&info.name))
            .map(|info| {
                let flattened = McpServerConfig::flattened(name, &info.name);
                let description = match &info.description {
                    Some(text) if !text.trim().is_empty() => text.clone(),
                    _ => format!("External tool `{}` on MCP server `{name}`.", info.name),
                };
                Arc::new(McpTool::new(
                    name.to_string(),
                    info.name.clone(),
                    self.strings.intern(&flattened),
                    self.strings.intern(&description),
                    info.input_schema.clone(),
                    client.clone(),
                    self.cap,
                )) as Arc<dyn Tool>
            })
            .collect();

        let total = infos.len();
        let published = tools.len();
        tracing::info!(
            server = %name,
            discovered = total,
            published,
            "MCP server connected"
        );

        self.state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(
                name.to_string(),
                State::Up {
                    client,
                    infos,
                    tools,
                },
            );
        Ok((total, published))
    }

    /// Connect and handshake, recording the failure so `failure()` can explain it.
    ///
    /// A server is reached one of two ways (D25): spawned over stdio when it has
    /// a `command`, or spoken to over Streamable HTTP when it has a `url`. Both
    /// paths end at the same [`McpClient`], so everything above this line —
    /// listing, filtering, gating, retrying — is transport-agnostic.
    async fn attempt(&self, name: &str, config: &McpServerConfig) -> Result<Arc<McpClient>> {
        let outcome = if config.is_http() {
            match config.bearer_token() {
                Ok(token) => {
                    McpClient::connect_http(name, config.url.trim(), token.as_deref()).await
                }
                Err(err) => Err(err),
            }
        } else {
            McpClient::connect(name, &config.command, &config.args).await
        };

        match outcome {
            Ok(client) => Ok(client),
            Err(err) => {
                tracing::warn!(server = %name, error = %err, "MCP server is unavailable");
                self.state
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .insert(name.to_string(), State::Down(err.to_string()));
                Err(err)
            }
        }
    }
}

impl ToolCatalog for McpServers {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        // Iterating the config rather than the state keeps the advertised order
        // stable, independent of the order the servers happened to come up in.
        self.servers
            .keys()
            .filter_map(|name| match state.get(name) {
                Some(State::Up { tools, .. }) => Some(tools.iter().cloned()),
                _ => None,
            })
            .flatten()
            .collect()
    }
}
