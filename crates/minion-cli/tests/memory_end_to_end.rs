//! End-to-end check of the memory tools through a scripted provider.
//!
//! The unit tests cover `remember` and `recall` in isolation. This drives them
//! the way a real session does — a provider that calls the tool, the agent loop
//! dispatching it, a transcript the next turn can read — because the property
//! that matters is that a fact the model wrote in one turn is available to it in
//! the next, through the same store the session persists through.
//!
//! No network and no API key: the provider is a stub that emits tool calls.

use std::sync::Arc;

use futures::stream::BoxStream;
use minion_core::error::Result;
use minion_core::message::{Message, Role};
use minion_core::provider::{ChatEvent, ChatRequest, FinishReason, Provider, ToolSchema};
use minion_core::tool::Risk;
use minion_core::{Agent, AgentEvent, AgentOptions, StopReason};
use minion_store::Store;
use tokio_util::sync::CancellationToken;

/// A provider that calls `remember` on its first turn, answers in prose when it
/// sees the tool result, then calls `recall` on the next user turn.
struct ScriptedProvider {
    turn: std::sync::atomic::AtomicUsize,
    /// Tool names seen advertised across every request.
    advertised: std::sync::Mutex<Vec<Vec<String>>>,
}

impl ScriptedProvider {
    fn new() -> Self {
        Self {
            turn: std::sync::atomic::AtomicUsize::new(0),
            advertised: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl Provider for ScriptedProvider {
    fn stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> BoxStream<'static, Result<ChatEvent>> {
        self.advertised
            .lock()
            .expect("advertised mutex")
            .push(request.tools.iter().map(|tool| tool.name.clone()).collect());

        // A tool result already in the history means the model has been told
        // what happened and can speak its answer.
        let answered = request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool);
        let last_tool = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .and_then(|message| message.content.clone())
            .unwrap_or_default();

        let turn = self.turn.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let events: Vec<Result<ChatEvent>> = if answered {
            let spoken = if turn >= 2 {
                // The second answered turn is after `recall`, so repeat what it
                // found: that is the assertion the test cares about.
                format!("you said: {}", last_tool.trim())
            } else {
                "noted".to_string()
            };
            vec![
                Ok(ChatEvent::TextDelta(spoken)),
                Ok(ChatEvent::Done {
                    finish_reason: FinishReason::Stop,
                }),
            ]
        } else if turn == 0 {
            vec![Ok(ChatEvent::ToolCallDelta {
                index: 0,
                id: Some("call-1".to_string()),
                name: Some("remember".to_string()),
                arguments: serde_json::json!({
                    "key": "release",
                    "value": "always cut a release from main",
                    "tags": ["process"]
                })
                .to_string(),
            })]
        } else {
            vec![Ok(ChatEvent::ToolCallDelta {
                index: 0,
                id: Some("call-2".to_string()),
                name: Some("recall".to_string()),
                arguments: serde_json::json!({ "query": "release" }).to_string(),
            })]
        };

        Box::pin(futures::stream::iter(events))
    }
}

fn registry(store: Arc<Store>) -> minion_core::ToolRegistry {
    minion_tools::default_registry(
        &minion_tools::ToolConfig {
            max_file_bytes: 1 << 20,
            shell: "/bin/sh".to_string(),
            default_timeout: std::time::Duration::from_secs(5),
            max_timeout: std::time::Duration::from_secs(10),
            output_cap_bytes: 1 << 16,
            http_fetch: minion_core::HttpFetchConfig::default(),
        },
        store,
        minion_tools::CronContext::default(),
    )
}

fn agent(provider: Arc<ScriptedProvider>, store: Arc<Store>, root: &std::path::Path) -> Agent {
    // No gate is installed on purpose. This test is about the tools and the
    // store, and installing a gate here would be testing the engine, which
    // `memory_gate.rs` covers. Wiring the gate into a real session is
    // `setup::build`'s job.
    Agent::new(
        provider,
        Arc::new(registry(store)),
        AgentOptions {
            model: "scripted".to_string(),
            max_iterations: 8,
            temperature: None,
            max_tokens: None,
            parallel_tool_calls: None,
            include_usage: false,
            max_tool_calls_per_turn: 0,
            workspace_root: root.to_path_buf(),
        },
    )
}

/// Run one user turn, discarding the events.
async fn turn(agent: &Agent, history: &mut Vec<Message>, prompt: &str) -> StopReason {
    let (sink, mut events) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    history.push(Message::user(prompt.to_string()));
    // The sink must be drained or the loop's sends are cheap no-ops either way;
    // drain to keep the transcript honest.
    let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
    let stop = agent
        .run(history, &sink, CancellationToken::new())
        .await
        .stop;
    drop(sink);
    let _ = drain.await;
    stop
}

#[tokio::test]
async fn a_fact_the_model_writes_comes_back_to_it_on_the_next_turn() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().to_path_buf();

    let store = Arc::new(Store::open_in_memory().await.expect("store"));
    let provider = Arc::new(ScriptedProvider::new());
    let agent = agent(provider.clone(), store.clone(), &root);
    let namespace = minion_core::namespace_for(&root);

    // Turn 1: the model writes a fact.
    let mut history = vec![Message::system("sys")];
    assert_eq!(
        turn(&agent, &mut history, "remember our release process").await,
        StopReason::Completed
    );

    let stored = store
        .recall(&namespace, "release", 5)
        .await
        .expect("recall after write");
    assert_eq!(stored.len(), 1, "the model's write must have landed");
    assert_eq!(stored[0].key, "release");
    assert_eq!(stored[0].tags, vec!["process".to_string()]);

    // Turn 2: a fresh history, the way a resumed session starts. The model
    // recalls and is told the fact it wrote an hour and a session ago.
    let mut history = vec![Message::system("sys")];
    assert_eq!(
        turn(&agent, &mut history, "what is our release process?").await,
        StopReason::Completed
    );

    let spoken: Vec<&str> = history
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .filter_map(|message| message.content.as_deref())
        .collect();
    assert!(
        spoken
            .iter()
            .any(|text| text.contains("cut a release from main")),
        "the model should have been told the remembered fact; assistant turns: {spoken:?}"
    );

    // The tools were actually advertised, or nothing above proves anything.
    let advertised = provider
        .advertised
        .lock()
        .expect("advertised mutex")
        .clone();
    assert!(
        advertised.iter().all(|names| {
            names.contains(&"remember".to_string()) && names.contains(&"recall".to_string())
        }),
        "both memory tools must be advertised on every request: {advertised:?}"
    );
}

#[tokio::test]
async fn memory_written_in_one_workspace_is_invisible_in_another() {
    let temp = tempfile::tempdir().expect("tempdir");
    let one = temp.path().join("one");
    let two = temp.path().join("two");
    std::fs::create_dir_all(&one).expect("mkdir one");
    std::fs::create_dir_all(&two).expect("mkdir two");

    let store = Arc::new(Store::open_in_memory().await.expect("store"));
    let agent = agent(Arc::new(ScriptedProvider::new()), store.clone(), &one);

    let mut history = vec![Message::system("sys")];
    turn(&agent, &mut history, "remember our release process").await;

    assert_eq!(
        store
            .recall(&minion_core::namespace_for(&one), "release", 5)
            .await
            .expect("recall one")
            .len(),
        1
    );
    assert!(
        store
            .recall(&minion_core::namespace_for(&two), "release", 5)
            .await
            .expect("recall two")
            .is_empty(),
        "a sibling workspace must not see the fact"
    );
}

#[tokio::test]
async fn the_memory_tools_carry_the_risk_classes_the_spec_names() {
    let store = Arc::new(Store::open_in_memory().await.expect("store"));
    let registry = registry(store);
    let risks = registry.risks();
    let risk_of = |name: &str| {
        risks
            .iter()
            .find(|(tool, _)| *tool == name)
            .map(|(_, risk)| *risk)
    };

    // SDD §5.5: remember is Write, recall is ReadOnly. These classes are what
    // put `remember` behind the gate.
    assert_eq!(risk_of("remember"), Some(Risk::Write));
    assert_eq!(risk_of("recall"), Some(Risk::ReadOnly));

    let schemas: Vec<ToolSchema> = registry.schemas();
    for name in ["remember", "recall"] {
        let schema = schemas
            .iter()
            .find(|schema| schema.name == name)
            .unwrap_or_else(|| panic!("{name} has no schema"));
        assert_eq!(
            schema.parameters["type"], "object",
            "{name} schema is not an object: {schema:?}"
        );
    }
}
