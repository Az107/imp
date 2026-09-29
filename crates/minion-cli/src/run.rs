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
    let mut session = setup::build(cli, config, cwd)?;
    session.history.push(Message::user(prompt));

    let (sender, receiver) = mpsc::unbounded_channel();
    let rendering = tokio::spawn(render_events(receiver, cli.json, colour_enabled(cli)));

    let cancel = CancellationToken::new();
    let watcher = spawn_interrupt_watcher(cancel.clone());

    let agent = session.agent();
    let outcome = agent.run(&mut session.history, &sender, cancel).await;

    drop(sender);
    let _ = rendering.await;
    watcher.abort();

    Ok(outcome.stop)
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
