//! The MCP client driven against a real server over stdio.
//!
//! The server is `mcp-stub-server`, the fixture binary in this crate: a real
//! process speaking real MCP over a real pipe, which is the only way to test the
//! parts of §5.10 that are about plumbing — spawning, listing, flattening,
//! filtering, and surviving a server that is not there.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{Value, json};

use minion_core::agent::{Agent, AgentOptions, StopReason};
use minion_core::config::{Decision, McpClientConfig, McpServerConfig};
use minion_core::message::Message;
use minion_core::policy::{ApprovalStore, AuditEntry, PolicyEngine, RecordingGate, ToolGate};
use minion_core::provider::{ChatEvent, ChatRequest, FinishReason, Provider};
use minion_core::tool::{Risk, ToolRegistry};
use tokio_util::sync::CancellationToken;

use minion_mcp::{McpServers, OnStart};

/// The fixture server, built by cargo next to the tests.
const STUB: &str = env!("CARGO_BIN_EXE_mcp-stub-server");

fn server(tool_allow: &[&str], approval: Option<Decision>) -> McpServerConfig {
    McpServerConfig {
        command: STUB.to_string(),
        args: Vec::new(),
        lazy: false,
        tool_allow: tool_allow.iter().map(|s| s.to_string()).collect(),
        approval,
    }
}

fn client_config(entries: &[(&str, McpServerConfig)]) -> McpClientConfig {
    McpClientConfig {
        servers: entries
            .iter()
            .map(|(name, config)| (name.to_string(), config.clone()))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn registry(servers: &Arc<McpServers>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.attach_catalog(servers.clone());
    registry
}

// ----------------------------------------------------------------- the gate

/// An [`ApprovalStore`] that keeps what it was told, so a test can read the
/// audit trail the gate produced.
#[derive(Default)]
struct AuditLog(Mutex<Vec<AuditEntry>>);

impl AuditLog {
    fn entries(&self) -> Vec<AuditEntry> {
        self.0.lock().unwrap().clone()
    }
}

#[async_trait]
impl ApprovalStore for AuditLog {
    async fn is_allowed(
        &self,
        _tool: &str,
        _pattern: &str,
        _scope: &str,
    ) -> minion_core::Result<bool> {
        Ok(false)
    }

    async fn remember_allow(
        &self,
        _tool: &str,
        _pattern: &str,
        _scope: &str,
    ) -> minion_core::Result<()> {
        Ok(())
    }

    async fn audit(&self, entry: AuditEntry) {
        self.0.lock().unwrap().push(entry);
    }
}

// ------------------------------------------------------------- the provider

/// A provider that replays scripted answers, so a turn can be driven without a
/// network or a model.
struct Scripted(Mutex<VecDeque<Vec<ChatEvent>>>);

impl Scripted {
    fn new(scripts: Vec<Vec<ChatEvent>>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(scripts.into())))
    }
}

impl Provider for Scripted {
    fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> BoxStream<'static, minion_core::Result<ChatEvent>> {
        let events = self.0.lock().unwrap().pop_front().unwrap_or_else(|| {
            vec![ChatEvent::Done {
                finish_reason: FinishReason::Stop,
            }]
        });
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }
}

fn agent(provider: Arc<dyn Provider>, registry: ToolRegistry, gate: Arc<dyn ToolGate>) -> Agent {
    Agent::new(
        provider,
        Arc::new(registry),
        AgentOptions {
            model: "scripted".to_string(),
            max_iterations: 4,
            workspace_root: std::env::temp_dir(),
            ..AgentOptions::default()
        },
    )
    .with_gate(gate)
}

// -------------------------------------------------------------------- tests

/// The golden tool list: what the model is offered, name for name, with the
/// server's own schema carried through untouched.
#[tokio::test]
async fn the_flattened_catalogue_of_a_real_server() {
    let servers = McpServers::new(&client_config(&[("stub", server(&["*"], None))]), 64 * 1024);
    let notices = servers.refresh(OnStart::All).await;
    assert_eq!(notices.len(), 1, "coming up is a transition: {notices:?}");

    let registry = registry(&servers);
    let catalogue: Vec<Value> = registry
        .schemas()
        .into_iter()
        .map(|schema| {
            json!({
                "name": schema.name,
                "description": schema.description,
                "parameters": schema.parameters,
            })
        })
        .collect();

    insta::assert_json_snapshot!("stub_tools", catalogue);

    servers.shutdown().await;
}

/// `tool_allow` is a filter on what reaches the model, not a warning: a tool
/// that is not listed is absent from the catalogue and cannot be invoked.
#[tokio::test]
async fn tool_allow_hides_and_blocks() {
    let servers = McpServers::new(
        &client_config(&[("stub", server(&["echo"], None))]),
        64 * 1024,
    );
    servers.refresh(OnStart::All).await;
    let registry = registry(&servers);

    assert!(registry.get("mcp__stub__echo").is_some());
    assert!(
        registry.get("mcp__stub__secret").is_none(),
        "a tool outside tool_allow must not reach the model"
    );
    assert_eq!(registry.len(), 1, "only the allowed tool is offered");

    // Every tool the server listed is still known, which is what makes
    // `minion mcp tools` able to say what was hidden.
    let discovered = servers.discovered("stub");
    assert_eq!(discovered.len(), 4, "was: {discovered:?}");

    servers.shutdown().await;
}

/// A server's tool never shadows a built-in: the `mcp__<server>__` prefix keeps
/// the names apart, and the registry would refuse a duplicate even if it did
/// not (see `minion_core::tool`'s own tests for that half).
#[tokio::test]
async fn a_server_tool_named_like_a_built_in_does_not_shadow_it() {
    let servers = McpServers::new(&client_config(&[("stub", server(&["*"], None))]), 64 * 1024);
    servers.refresh(OnStart::All).await;
    let mut registry = registry(&servers);
    registry.register(Native);

    let native = registry.get("read_file").expect("read_file is registered");
    assert_eq!(
        native.risk(),
        Risk::ReadOnly,
        "`read_file` must still be minion's own tool"
    );
    assert!(
        native.description().contains("native"),
        "was: {}",
        native.description()
    );

    let external = registry
        .get("mcp__stub__read_file")
        .expect("the server's read_file is offered under its prefixed name");
    assert_eq!(external.risk(), Risk::Network);
    assert_ne!(
        external.description(),
        native.description(),
        "the two tools are distinct entries"
    );

    servers.shutdown().await;
}

/// minion's own `read_file`, standing in for the real one.
struct Native;

#[async_trait]
impl minion_core::tool::Tool for Native {
    fn name(&self) -> &'static str {
        "read_file"
    }
    fn description(&self) -> &'static str {
        "the native read_file"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    async fn invoke(
        &self,
        _ctx: minion_core::tool::ToolCtx,
        _args: Value,
    ) -> minion_core::Result<minion_core::tool::ToolOutput> {
        Ok(minion_core::tool::ToolOutput::text("native"))
    }
}

/// `lazy = true` defers the spawn: a session that is assembled — which is what
/// `minion mcp list` would do — does not start the process, and the first turn
/// does. The tools still reach the catalogue, which is the whole difference
/// between `lazy` and `tool_allow`.
#[tokio::test]
async fn a_lazy_server_starts_on_the_first_turn() {
    let lazy = McpServerConfig {
        lazy: true,
        ..server(&["*"], None)
    };
    let servers = McpServers::new(&client_config(&[("slow", lazy)]), 64 * 1024);

    let eager = servers.refresh(OnStart::Eager).await;
    assert!(
        eager.is_empty(),
        "nothing to report about a server left alone"
    );
    assert!(
        !servers.is_up("slow"),
        "a lazy server must not be spawned yet"
    );
    assert!(registry(&servers).is_empty());

    let all = servers.refresh(OnStart::All).await;
    assert_eq!(all.len(), 1, "the first turn starts it: {all:?}");
    assert!(servers.is_up("slow"));
    assert!(!registry(&servers).is_empty());

    servers.shutdown().await;
}

/// Asking about one server explicitly is a use, so `minion mcp tools <name>`
/// starts a lazily-configured server too.
#[tokio::test]
async fn a_lazy_server_starts_when_it_is_asked_about() {
    let lazy = McpServerConfig {
        lazy: true,
        ..server(&["echo"], None)
    };
    let servers = McpServers::new(&client_config(&[("slow", lazy)]), 64 * 1024);

    servers
        .connect("slow")
        .await
        .expect("an explicit use starts it");

    assert!(servers.is_up("slow"));
    assert_eq!(servers.discovered("slow").len(), 4);
    assert_eq!(servers.published("slow").len(), 1);

    servers.shutdown().await;
}

/// A server that cannot be started costs its tools and nothing else: the turn
/// still completes, a notice explains the absence, and the retry next turn does
/// not repeat a notice the user has already read.
#[tokio::test]
async fn a_server_that_is_down_does_not_break_the_turn() {
    let broken = McpServerConfig {
        command: "/nonexistent/minion-mcp-no-such-server".to_string(),
        ..server(&["*"], None)
    };
    let servers = McpServers::new(&client_config(&[("gone", broken)]), 64 * 1024);

    let notices = servers.refresh(OnStart::All).await;
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("unavailable"), "was: {}", notices[0]);
    assert!(notices[0].contains("retried"), "was: {}", notices[0]);

    let retry = servers.refresh(OnStart::All).await;
    assert!(
        retry.is_empty(),
        "nothing changed, so there is nothing new to say"
    );

    let registry = registry(&servers);
    assert!(registry.is_empty(), "a down server offers no tool");

    let provider = Scripted::new(vec![
        vec![
            ChatEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("mcp__gone__echo".to_string()),
                arguments: json!({ "text": "hi" }).to_string(),
            },
            ChatEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ],
        vec![
            ChatEvent::TextDelta("carried on".to_string()),
            ChatEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ],
    ]);
    let engine = PolicyEngine::new(
        Vec::new(),
        Vec::new(),
        Decision::Auto,
        Decision::Auto,
        "/workspace",
        true,
    );
    let gate = RecordingGate::arc(Arc::new(engine), Arc::new(AuditLog::default()));
    let agent = agent(provider, registry, gate);

    let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut history = vec![Message::user("call the server")];
    let outcome = agent
        .run(&mut history, &sink, CancellationToken::new())
        .await;

    assert_eq!(outcome.stop, StopReason::Completed, "the turn survived");
    assert_eq!(outcome.text.as_deref(), Some("carried on"));
    let result = history[2].content.clone().unwrap_or_default();
    assert!(
        result.contains("unknown tool"),
        "the model is told the tool is not there: {result}"
    );
}

/// §5.10: the per-server policy substitutes the global one, so a trusted server
/// is `auto` while an untrusted one is not — and a family policy cannot widen a
/// refusal that already happened.
#[tokio::test]
async fn a_server_policy_beats_the_global_default() {
    let servers = McpServers::new(
        &client_config(&[
            ("trusted", server(&["*"], Some(Decision::Auto))),
            ("untrusted", server(&["*"], None)),
        ]),
        64 * 1024,
    );

    let engine = PolicyEngine::new(
        Vec::new(),
        vec![("mcp__trusted__secret".to_string(), "*".to_string())],
        Decision::Ask,
        Decision::Deny,
        "/workspace",
        false,
    )
    .with_tool_policies(servers.policy_families());

    // The trusted server runs unattended, without a prompt being available.
    engine
        .check("mcp__trusted__echo", Risk::Network, &json!({}), None)
        .await
        .expect("the server is trusted");

    // The one thing it is not trusted for still refuses.
    let err = engine
        .check("mcp__trusted__secret", Risk::Network, &json!({}), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("deny rule"), "was: {err}");

    // A server with no policy of its own takes the global non-interactive one.
    let err = engine
        .check("mcp__untrusted__echo", Risk::Network, &json!({}), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("non-interactive"), "was: {err}");
}

/// The exit criterion: an external tool, invoked by the model, passing the gate,
/// and recorded.
#[tokio::test]
async fn an_external_tool_is_invoked_through_the_gate_and_recorded() {
    let servers = McpServers::new(
        &client_config(&[("stub", server(&["echo"], Some(Decision::Auto)))]),
        64 * 1024,
    );
    servers.refresh(OnStart::All).await;

    let engine = PolicyEngine::new(
        Vec::new(),
        Vec::new(),
        Decision::Ask,
        Decision::Deny,
        "/workspace",
        true,
    )
    .with_tool_policies(servers.policy_families());
    let log = Arc::new(AuditLog::default());
    let gate = RecordingGate::arc(Arc::new(engine), log.clone() as Arc<dyn ApprovalStore>);

    let provider = Scripted::new(vec![
        vec![
            ChatEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("mcp__stub__echo".to_string()),
                arguments: json!({ "text": "pong" }).to_string(),
            },
            ChatEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ],
        vec![
            ChatEvent::TextDelta("the server said pong".to_string()),
            ChatEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ],
    ]);

    let agent = agent(provider, registry(&servers), gate);
    let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut history = vec![Message::user("echo pong on the server")];

    let outcome = agent
        .run(&mut history, &sink, CancellationToken::new())
        .await;

    assert_eq!(outcome.stop, StopReason::Completed);
    assert_eq!(outcome.text.as_deref(), Some("the server said pong"));
    assert_eq!(
        history[2].content.as_deref(),
        Some("pong"),
        "the server's own result, in the transcript"
    );

    let audits = log.entries();
    assert_eq!(audits.len(), 1, "one decision, one row: {audits:?}");
    assert_eq!(audits[0].tool, "mcp__stub__echo");
    assert_eq!(audits[0].decision, "allow");
    assert_eq!(audits[0].risk, "network");

    servers.shutdown().await;
}

/// A tool that is not in `tool_allow` cannot be called even by name: the call
/// never reaches the server.
#[tokio::test]
async fn a_hidden_tool_is_not_invocable() {
    let servers = McpServers::new(
        &client_config(&[("stub", server(&["echo"], Some(Decision::Auto)))]),
        64 * 1024,
    );
    servers.refresh(OnStart::All).await;

    let engine = PolicyEngine::new(
        Vec::new(),
        Vec::new(),
        Decision::Auto,
        Decision::Auto,
        "/workspace",
        true,
    )
    .with_tool_policies(servers.policy_families());
    let log = Arc::new(AuditLog::default());
    let gate = RecordingGate::arc(Arc::new(engine), log.clone() as Arc<dyn ApprovalStore>);

    let provider = Scripted::new(vec![
        vec![
            ChatEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("mcp__stub__secret".to_string()),
                arguments: "{}".to_string(),
            },
            ChatEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ],
        vec![
            ChatEvent::TextDelta("understood".to_string()),
            ChatEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ],
    ]);

    let agent = agent(provider, registry(&servers), gate);
    let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut history = vec![Message::user("read the secret")];
    agent
        .run(&mut history, &sink, CancellationToken::new())
        .await;

    let result = history[2].content.clone().unwrap_or_default();
    assert!(result.contains("unknown tool"), "was: {result}");
    assert!(
        log.entries().is_empty(),
        "the gate was never asked, so nothing was decided"
    );

    servers.shutdown().await;
}
