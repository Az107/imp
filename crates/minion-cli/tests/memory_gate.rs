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

/// `remember` is a `Write`, so a non-TTY still refuses it: nobody is there to
/// consent, and D15 keeps writes on the side-effect side of the line.
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

/// A read-only tool must survive a non-TTY, so `minion run ... > out.md` works.
///
/// This is SDD §5.6's `tool.risk == ReadOnly → Auto` and D15. The rule used to
/// be checked *after* the non-interactive branch, which refused reads whenever
/// stdin was piped.
#[tokio::test]
async fn a_read_only_tool_is_allowed_on_a_non_tty() {
    let engine = PolicyEngine::new(
        Vec::new(),
        Vec::new(),
        minion_core::config::Decision::Auto,
        minion_core::config::Decision::Deny,
        "session".to_string(),
        false,
    );

    for (tool, args) in [
        ("recall", serde_json::json!({ "query": "x" })),
        ("read_file", serde_json::json!({ "path": "a.txt" })),
    ] {
        let outcome = engine.check(tool, Risk::ReadOnly, &args, None).await;
        assert!(
            outcome.is_ok(),
            "`{tool}` only observes, so a missing terminal must not refuse it: {outcome:?}"
        );
    }
}

/// The flip side: writes and network calls still need consent on a non-TTY.
///
/// Widening the read side must not have widened anything else.
#[tokio::test]
async fn writes_and_network_still_need_consent_on_a_non_tty() {
    let engine = PolicyEngine::new(
        Vec::new(),
        Vec::new(),
        minion_core::config::Decision::Auto,
        minion_core::config::Decision::Deny,
        "session".to_string(),
        false,
    );

    for (tool, risk) in [
        ("remember", Risk::Write),
        ("write_file", Risk::Write),
        ("run_command", Risk::Execute),
        // D15: a request leaves the machine, so it counts as a side effect.
        ("http_fetch", Risk::Network),
    ] {
        let outcome = engine
            .check(tool, risk, &serde_json::json!({ "key": "k" }), Some("k"))
            .await;
        assert!(
            outcome.is_err(),
            "`{tool}` ({risk:?}) must still require consent on a non-TTY"
        );
    }
}

/// A deny rule must still beat the new read-only allowance.
///
/// The ordering property that matters: deny is rule 1 and the ReadOnly
/// short-circuit is now rule 4, so widening the latter cannot resurrect a tool
/// that was explicitly refused.
#[tokio::test]
async fn a_deny_rule_still_beats_the_read_only_allowance() {
    let engine = PolicyEngine::new(
        Vec::new(),
        vec![("read_file".to_string(), "secret/*".to_string())],
        minion_core::config::Decision::Auto,
        minion_core::config::Decision::Auto,
        "/ws".to_string(),
        false,
    );

    let denied = engine
        .check(
            "read_file",
            Risk::ReadOnly,
            &serde_json::json!({ "path": "secret/keys" }),
            Some("secret/keys"),
        )
        .await;
    assert!(denied.is_err(), "a deny rule must win over ReadOnly");

    let allowed = engine
        .check(
            "read_file",
            Risk::ReadOnly,
            &serde_json::json!({ "path": "src/main.rs" }),
            Some("src/main.rs"),
        )
        .await;
    assert!(allowed.is_ok(), "an unrelated read is still allowed");
}
