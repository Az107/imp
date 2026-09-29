//! One-shot execution.

use std::io::IsTerminal;
use std::path::Path;

use minion_core::agent::{AgentEvent, StopReason};
use minion_core::config::Config;
use minion_core::error::Result;
use minion_core::message::Message;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cli::Cli;
use crate::render::Renderer;
use crate::setup;

/// Run one prompt to completion and report why the turn ended.
pub async fn one_shot(
    cli: &Cli,
    config: &Config,
    cwd: &Path,
    prompt: String,
) -> Result<StopReason> {
    let mut session = setup::build(cli, config, cwd, None).await?;

    // Everything from this index on belongs to the turn, and is persisted as a
    // unit so the transcript never contains half of one.
    let turn_start = session.history.len();
    session.history.push(Message::user(prompt));

    // Printed before the renderer starts, so the two writers cannot interleave.
    // A session id is not an agent concern, so it does not go through AgentEvent.
    if cli.json {
        println!(
            "{}",
            serde_json::json!({ "type": "session", "id": session.session_id })
        );
    }

    let (sender, receiver) = mpsc::unbounded_channel();
    let rendering = tokio::spawn(render_events(receiver, cli.json, colour_enabled(cli)));

    let cancel = CancellationToken::new();
    let watcher = spawn_interrupt_watcher(cancel.clone());

    let agent = session.agent();
    let outcome = agent.run(&mut session.history, &sender, cancel).await;

    drop(sender);
    let _ = rendering.await;
    watcher.abort();

    session.persist_since(turn_start).await?;

    if !cli.json {
        // stderr, so `minion run ... > answer.txt` stays clean.
        eprintln!(
            "session {} · resume with: minion session resume {}",
            short_id(&session.session_id),
            session.session_id
        );
    }

    Ok(outcome.stop)
}

/// First few characters of an id, for one-line notes.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Drain agent events into a [`Renderer`] until the sender is dropped.
pub async fn render_events(
    mut receiver: mpsc::UnboundedReceiver<AgentEvent>,
    json: bool,
    colour: bool,
) {
    let mut renderer = Renderer::new(json, colour);
    while let Some(event) = receiver.recv().await {
        renderer.handle(&event);
    }
    renderer.finish();
}

/// Cancel `cancel` on the first Ctrl-C.
pub fn spawn_interrupt_watcher(cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
    })
}

/// Whether ANSI colour should be used: not `--no-color`, not `NO_COLOR`, and a TTY.
pub fn colour_enabled(cli: &Cli) -> bool {
    !cli.no_color && std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}
