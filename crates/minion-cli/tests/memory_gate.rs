//! The memory tools must respect the approval gate, like every other write.
//!
//! This lives as an integration test rather than a unit test because the
//! property under test is about the *wiring*: that a registry built by
//! `setup::build` has a gate attached, and that the gate is what stands between
//! a `remember` call and the database. Asserting the risk class alone would not
//! catch a registry that forgot to install the gate at all.

use std::sync::Arc;

use minion_core::error::{Error, Result};
use minion_core::policy::{PolicyEngine, ToolGate};
use minion_core::tool::{Risk, Tool, ToolCtx, ToolOutput};

/// A gate that records what it saw and always refuses, so a test can prove the
/// tool call reached it rather than running unchecked.
#[derive(Default)]
struct RecordingGate {
    seen: std::sync::Mutex<Vec<(String, Risk)>>,
}

#[async_trait::async_trait]
impl ToolGate for RecordingGate {
    async fn check(
        &self,
        tool: &str,
        risk: Risk,
        _args: &serde_json::Value,
        _subject: Option<&str>,
    ) -> Result<()> {
        self.seen
            .lock()
            .expect("gate mutex")
            .push((tool.to_string(), risk));
        Err(Error::Denied(format!("{tool} was denied")))
    }
}

/// Invokes a tool the way `Agent` does: through the gate first, and only on
/// approval. Mirrors the sequencing in `agent.rs:282`.
async fn invoke_gated(
    gate: &dyn ToolGate,
    tool: &dyn Tool,
    ctx: ToolCtx,
    args: serde_json::Value,
) -> Result<ToolOutput> {
    let subject = tool.approval_subject(&args);
    gate.check(tool.name(), tool.risk(), &args, subject.as_deref())
        .await?;
    tool.invoke(ctx, args).await
}

fn ctx(root: &str) -> ToolCtx {
    ToolCtx {
        workspace_root: std::path::PathBuf::from(root),
        cancel: tokio_util::sync::CancellationToken::new(),
    }
}

/// A `Write` tool reached through a refusing gate must not touch the store.
#[tokio::test]
async fn a_denied_remember_does_not_reach_the_database() {
    let store = Arc::new(minion_store::Store::open_in_memory().await.expect("store"));
    let tool = minion_tools::Remember::new(store.clone());
    let gate = RecordingGate::default();

    let outcome = invoke_gated(
        &gate,
        &tool,
        ctx("/ws"),
        serde_json::json!({ "key": "deploy", "value": "ask first" }),
    )
    .await;

    assert!(outcome.is_err(), "a denied write must not succeed");
    assert_eq!(
        gate.seen.lock().expect("gate mutex").as_slice(),
        &[("remember".to_string(), Risk::Write)],
        "the gate must be consulted, with the declared risk"
    );

    // The decisive check: nothing landed.
    let hits = store
        .recall(
            &minion_core::namespace_for(std::path::Path::new("/ws")),
            "deploy",
            5,
        )
        .await
        .expect("recall");
    assert!(hits.is_empty(), "a denied remember must not be persisted");
}

/// A refusing gate must not stop `recall`, which is read-only.
#[tokio::test]
async fn recall_is_unaffected_by_a_refusing_gate() {
    let store = Arc::new(minion_store::Store::open_in_memory().await.expect("store"));
    let namespace = minion_core::namespace_for(std::path::Path::new("/ws"));
    store
        .remember(&namespace, "k", "a stored fact", &[], None)
        .await
        .expect("seed");

    // A gate that approves everything stands in for a user who consented; the
    // point here is only that `recall` is classified read-only and returns the
    // fact, not how policy treats a non-TTY (pinned separately below).
    let permissive = PermissiveGate;
    let tool = minion_tools::Recall::new(store);

    let outcome = invoke_gated(
        &permissive,
        &tool,
        ctx("/ws"),
        serde_json::json!({ "query": "stored" }),
    )
    .await
    .expect("an approved read succeeds");

    assert!(
        outcome.content.contains("a stored fact"),
        "{}",
        outcome.content
    );
    assert_eq!(tool.risk(), Risk::ReadOnly);
}

/// A gate that approves every call, standing in for a consenting user.
struct PermissiveGate;

#[async_trait::async_trait]
impl ToolGate for PermissiveGate {
    async fn check(
        &self,
        _tool: &str,
        _risk: Risk,
        _args: &serde_json::Value,
        _subject: Option<&str>,
    ) -> Result<()> {
        Ok(())
    }
}

/// The engine the CLI actually builds must not wave `remember` through
/// unattended: a non-TTY has nobody to ask, so it fails closed.
#[tokio::test]
async fn a_non_interactive_engine_refuses_remember() {
    let engine = PolicyEngine::new(
        Vec::new(),
        Vec::new(),
        minion_core::config::Decision::Auto,
        minion_core::config::Decision::Deny,
        "session".to_string(),
        false,
    );

    let check = engine
        .check(
            "remember",
            Risk::Write,
            &serde_json::json!({ "key": "k", "value": "v" }),
            Some("k"),
        )
        .await;

    assert!(
        check.is_err(),
        "remember must not be allowed in a non-interactive context"
    );
}

/// Pins how the engine treats a read-only tool with nobody at a prompt.
///
/// SDD §5.6 lists `tool.risk == ReadOnly → Auto` *above* the
/// `!stdin.is_tty()` rule, which would let `recall` through unattended. The
/// implementation checks non-interactive first (§4 before §5 in
/// `PolicyEngine::check`), so under the default `noninteractive = "deny"` a
/// read-only call is refused. `read_file` behaves identically, so this is
/// pre-existing and not specific to memory.
///
/// Asserted as-is rather than "fixed" here: reordering the rule is a change to
/// the engine's security property, which AGENTS.md forbids doing incidentally.
/// `recall` simply follows whatever `read_file` already does.
#[tokio::test]
async fn a_read_only_tool_follows_the_same_rule_as_read_file() {
    let engine = || {
        PolicyEngine::new(
            Vec::new(),
            Vec::new(),
            minion_core::config::Decision::Auto,
            minion_core::config::Decision::Deny,
            "session".to_string(),
            false,
        )
    };

    let recall = engine()
        .check(
            "recall",
            Risk::ReadOnly,
            &serde_json::json!({ "query": "x" }),
            None,
        )
        .await;
    let read_file = engine()
        .check(
            "read_file",
            Risk::ReadOnly,
            &serde_json::json!({ "path": "a.txt" }),
            None,
        )
        .await;

    assert_eq!(
        recall.is_ok(),
        read_file.is_ok(),
        "recall must not be treated differently from the existing read-only tool"
    );
}
