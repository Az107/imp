//! Delegation to a peer, driven against a real MCP server over stdio.
//!
//! The peer is `mcp-stub-server`, the fixture binary in this crate, standing in
//! for another minion's `mcp serve`: a real process, speaking real MCP, exposing
//! a real `agent_ask`. That is the only way to test the parts of M10.2 that are
//! about plumbing — spawning, the handshake, the call, the answer — rather than
//! about a mock that agrees with whatever we wrote.
//!
//! What the tests pin, in the milestone's own terms: the size cap is honoured;
//! the gate decides *before* anything is sent; a peer that fails is a tool error
//! and not a hang; and the tokens spent remotely reach the turn's usage.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{Value, json};

use minion_core::agent::{Agent, AgentOptions, StopReason};
use minion_core::config::{Decision, Peer};
use minion_core::message::Message;
use minion_core::policy::{ApprovalStore, AuditEntry, PolicyEngine, RecordingGate, ToolGate};
use minion_core::provider::{ChatEvent, ChatRequest, FinishReason, Provider};
use minion_core::tool::{Risk, Tool, ToolRegistry};
use tokio_util::sync::CancellationToken;

use minion_mcp::peer_tools;

/// The fixture server, built by cargo next to the tests.
const STUB: &str = env!("CARGO_BIN_EXE_mcp-stub-server");

fn peer(command: &str) -> Peer {
    Peer {
        command: command.to_string(),
        ..Peer::default()
    }
}

fn peers(entries: &[(&str, Peer)]) -> BTreeMap<String, Peer> {
    entries
        .iter()
        .map(|(name, config)| (name.to_string(), config.clone()))
        .collect()
}

fn registry(entries: &[(&str, Peer)]) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for tool in peer_tools(&peers(entries)) {
        let tool = Arc::new(tool) as Arc<dyn minion_core::tool::Tool>;
        registry.register_arc(tool);
    }
    registry
}

// ------------------------------------------------------------------ the gate

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

// -------------------------------------------------------------- the provider

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
            temperature: None,
            max_tokens: None,
            parallel_tool_calls: None,
            include_usage: false,
            max_tool_calls_per_turn: 0,
            workspace_root: std::env::temp_dir(),
            ..AgentOptions::default()
        },
    )
    .with_gate(gate)
}

/// A turn that calls `peer__stub_ask` once and then stops.
fn asking(brief: Value) -> Arc<Scripted> {
    Scripted::new(vec![
        vec![
            ChatEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("peer__stub_ask".to_string()),
                arguments: brief.to_string(),
            },
            ChatEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ],
        vec![
            ChatEvent::TextDelta("answered from the peer".to_string()),
            ChatEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ],
    ])
}

fn auto_gate() -> (Arc<dyn ToolGate>, Arc<AuditLog>) {
    let engine = PolicyEngine::new(
        Vec::new(),
        Vec::new(),
        Decision::Auto,
        Decision::Auto,
        "/workspace",
        true,
    );
    let log = Arc::new(AuditLog::default());
    let gate = RecordingGate::arc(Arc::new(engine), log.clone() as Arc<dyn ApprovalStore>);
    (gate, log)
}

async fn run(agent: &Agent, prompt: &str) -> (minion_core::agent::TurnOutcome, Vec<Message>) {
    let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut history = vec![Message::user(prompt)];
    let outcome = agent
        .run(&mut history, &sink, CancellationToken::new())
        .await;
    (outcome, history)
}

// --------------------------------------------------------------------- tests

/// The happy path, end to end: a brief reaches the peer, only the peer's *text*
/// comes back, its usage is folded into the turn, and the decision is audited
/// under the tool name the gate keys on.
#[tokio::test]
async fn a_delegation_reaches_the_peer_and_its_usage_lands_in_the_turn() {
    let (gate, log) = auto_gate();
    let provider = asking(json!({ "brief": "why is the sky blue?" }));
    let agent = agent(provider, registry(&[("stub", peer(STUB))]), gate);

    let (outcome, history) = run(&agent, "ask the peer").await;

    assert_eq!(outcome.stop, StopReason::Completed);
    assert_eq!(outcome.text.as_deref(), Some("answered from the peer"));

    let result = history[2].content.clone().unwrap_or_default();
    assert_eq!(
        result, "brief received: why is the sky blue?",
        "the peer's own text, unwrapped from the agent_ask JSON, is the tool result"
    );
    assert!(
        !result.contains("session_id"),
        "the JSON envelope must not leak into the transcript: {result}"
    );

    // The remote tokens are the turn's tokens (R8, advisory but visible).
    assert_eq!(outcome.usage.prompt_tokens, 11);
    assert_eq!(outcome.usage.completion_tokens, 22);
    assert_eq!(outcome.usage.total_tokens, 33);

    let audits = log.entries();
    assert_eq!(audits.len(), 1, "one decision, one row: {audits:?}");
    assert_eq!(audits[0].tool, "peer__stub_ask");
    assert_eq!(audits[0].risk, "network");
    assert_eq!(audits[0].decision, "allow");
}

/// The gate decides *before* the peer is contacted. The configured command does
/// not exist, so anything that tried to spawn it would fail with "could not be
/// reached"; the refusal below is the proof that nothing was attempted.
#[tokio::test]
async fn the_gate_decides_before_the_peer_is_contacted() {
    let engine = PolicyEngine::new(
        Vec::new(),
        Vec::new(),
        Decision::Ask,
        Decision::Deny,
        "/workspace",
        false,
    );
    let log = Arc::new(AuditLog::default());
    let gate = RecordingGate::arc(Arc::new(engine), log.clone() as Arc<dyn ApprovalStore>);

    let provider = Scripted::new(vec![
        vec![
            ChatEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("peer__stub_ask".to_string()),
                arguments: json!({ "brief": "hi" }).to_string(),
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
    let agent = agent(
        provider,
        registry(&[("stub", peer("/nonexistent/peer-that-would-explode"))]),
        gate,
    );

    let (outcome, history) = run(&agent, "escalate").await;

    assert_eq!(outcome.stop, StopReason::Completed, "the turn survived");
    let result = history[2].content.clone().unwrap_or_default();
    assert!(
        result.contains("non-interactive"),
        "the non-interactive decision must refuse it: {result}"
    );
    assert!(
        !result.contains("could not be reached"),
        "the peer was contacted before the gate decided: {result}"
    );

    let audits = log.entries();
    assert_eq!(audits.len(), 1, "the refusal is recorded: {audits:?}");
    assert_eq!(audits[0].tool, "peer__stub_ask");
    assert_eq!(audits[0].decision, "deny");
}

/// The result cap is honoured: a long answer is cut, marked, and stays the
/// peer's *text* rather than its JSON envelope.
#[tokio::test]
async fn a_long_answer_is_cut_to_the_configured_cap() {
    let capped = Peer {
        command: STUB.to_string(),
        result_cap_bytes: 8192,
        ..Peer::default()
    };
    let (gate, _log) = auto_gate();
    let provider = asking(json!({ "brief": "huge" }));
    let agent = agent(provider, registry(&[("stub", capped)]), gate);

    let (outcome, history) = run(&agent, "ask for a lot").await;

    assert_eq!(outcome.stop, StopReason::Completed);
    let result = history[2].content.clone().unwrap_or_default();
    assert!(
        result.len() <= 8192 + "\n… [truncated to the output cap]".len(),
        "the answer exceeds the cap: {} bytes",
        result.len()
    );
    assert!(result.contains("truncated"), "the cut must be marked");
    assert!(
        result.starts_with("xxxx"),
        "the peer's text, not its envelope: {}",
        &result[..result.len().min(60)]
    );
}

/// A peer that refuses the brief is a tool error the model can read — no panic,
/// and the turn carries on.
#[tokio::test]
async fn a_peer_that_refuses_the_brief_is_a_tool_error() {
    let (gate, _log) = auto_gate();
    let provider = asking(json!({ "brief": "explode please" }));
    let agent = agent(provider, registry(&[("stub", peer(STUB))]), gate);

    let (outcome, history) = run(&agent, "ask the flaky peer").await;

    assert_eq!(outcome.stop, StopReason::Completed, "the turn survived");
    let result = history[2].content.clone().unwrap_or_default();
    assert!(
        result.contains("could not answer the brief"),
        "the peer's own words reach the model: {result}"
    );
    assert!(
        result.contains("peer__stub_ask"),
        "the error names the tool that failed: {result}"
    );
}

/// A peer that is not there is a tool error too, not a hang or a panic.
#[tokio::test]
async fn a_peer_that_is_not_there_is_a_tool_error() {
    let (gate, _log) = auto_gate();
    let provider = asking(json!({ "brief": "are you there?" }));
    let agent = agent(
        provider,
        registry(&[("stub", peer("/nonexistent/ghost-peer"))]),
        gate,
    );

    let (outcome, history) = run(&agent, "ask the ghost").await;

    assert_eq!(outcome.stop, StopReason::Completed, "the turn survived");
    let result = history[2].content.clone().unwrap_or_default();
    assert!(
        result.contains("could not be reached"),
        "an unreachable peer is explained: {result}"
    );
}

/// A peer tool is `Network`, always — the class that puts it behind the gate
/// (D15) and keeps it out of a delegated turn's read-only surface (§5.13).
#[test]
fn a_peer_tool_is_network_and_therefore_not_delegable() {
    let tools = peer_tools(&peers(&[("stub", peer(STUB))]));
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "peer__stub_ask");
    assert_eq!(tools[0].risk(), Risk::Network);
    assert!(
        !tools[0].risk().is_observation(),
        "a Network tool must never be handed to a delegated turn"
    );
}
