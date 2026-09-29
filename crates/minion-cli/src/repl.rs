//! The interactive REPL.
//!
//! Deliberately not a full-screen TUI: input is line-oriented, output is
//! appended, and scrollback is preserved. Tool activity and approvals go to
//! stderr so that piping stdout yields only the conversation.

use std::path::Path;
use std::sync::Arc;

use minion_core::agent::StopReason;
use minion_core::config::Config;
use minion_core::error::{Error, Result};
use minion_core::message::Message;
use minion_core::provider::Usage;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cli::Cli;
use crate::run;
use crate::setup::{self, Session};

/// Run the interactive loop until the user quits or stdin closes.
pub async fn interactive(cli: &Cli, config: &Config, cwd: &Path) -> Result<()> {
    let mut session = setup::build(cli, config, cwd)?;
    let mut editor = DefaultEditor::new()
        .map_err(|err| Error::Config(format!("cannot start the line editor: {err}")))?;

    // Ctrl-C mid-turn cancels the turn; the token is replaced so the next turn
    // starts fresh.
    let interrupt = Arc::new(Mutex::new(CancellationToken::new()));
    spawn_interrupt_swapper(interrupt.clone());

    println!(
        "minion {} · {} · {} · policy: {}",
        env!("CARGO_PKG_VERSION"),
        session.options.model,
        session.workspace_root.display(),
        decision_name(config.policy.default)
    );
    println!("Type /help for commands, /quit to exit.");
    println!("session {}", short_id(&session.session_id));

    let mut usage = Usage::default();

    loop {
        let line = match editor.readline("› ") {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => {
                println!("^C");
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(err) => {
                eprintln!("minion: input error: {err}");
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(trimmed);

        if let Some(command) = trimmed.strip_prefix('!') {
            shell_escape(command, &session.workspace_root).await;
            continue;
        }
        if let Some(command) = trimmed.strip_prefix('/') {
            if handle_slash(command, &mut session, &usage)? {
                break;
            }
            continue;
        }

        session.history.push(Message::user(trimmed));
        let (sender, receiver) = mpsc::unbounded_channel();
        let rendering = tokio::spawn(run::render_events(
            receiver,
            cli.json,
            run::colour_enabled(cli),
        ));

        let cancel = interrupt.lock().await.clone();
        let agent = session.agent();
        let outcome = agent.run(&mut session.history, &sender, cancel).await;
        usage.absorb(outcome.usage);

        drop(sender);
        let _ = rendering.await;

        if outcome.stop != StopReason::Completed {
            eprintln!("… turn ended: {}", outcome.stop.as_str());
        }
    }

    Ok(())
}

/// Handle a `/command`. Returns `true` when the REPL should exit.
fn handle_slash(command: &str, session: &mut Session, usage: &Usage) -> Result<bool> {
    let mut parts = command.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let argument = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    match name {
        "quit" | "exit" | "q" => return Ok(true),
        "help" | "?" => print_help(),
        "tools" => {
            for (tool, risk) in session.tools.risks() {
                println!("  {tool:<20} {}", risk.as_str());
            }
        }
        "clear" => {
            session.history.truncate(1);
            println!("history cleared");
        }
        "model" => match argument {
            Some(model) => {
                session.options.model = model.to_string();
                println!("model set to {}", session.options.model);
            }
            None => println!("{}", session.options.model),
        },
        "cost" => println!(
            "prompt {} · completion {} · total {} tokens this session",
            usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
        ),
        "session" => {
            println!("{}", session.session_id);
            let note = "This is the conversation id substituted into any ${session} header.";
            println!("{note}");
        }
        other => eprintln!("unknown command `/{other}` — try /help"),
    }
    Ok(false)
}

fn print_help() {
    println!("  /help              this message");
    println!("  /model [name]      show or change the model for this session");
    println!("  /tools             list enabled tools and their risk class");
    println!("  /cost              token usage for this session");
    println!("  /session           show the conversation id sent to the provider");
    println!("  /clear             forget the conversation, keep the system prompt");
    println!("  /quit              exit");
    println!("  !<command>         run a shell command directly, bypassing the model");
}

/// Run a local shell command, bypassing the model entirely.
async fn shell_escape(command: &str, root: &Path) {
    if command.trim().is_empty() {
        return;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let status = tokio::process::Command::new(shell)
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .status()
        .await;

    match status {
        Ok(status) if !status.success() => {
            eprintln!("(exit {})", status.code().unwrap_or(-1));
        }
        Ok(_) => {}
        Err(err) => eprintln!("minion: cannot run `{command}`: {err}"),
    }
}

/// Replace the cancellation token after each interrupt so turns are independent.
fn spawn_interrupt_swapper(slot: Arc<Mutex<CancellationToken>>) {
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            let mut guard = slot.lock().await;
            guard.cancel();
            *guard = CancellationToken::new();
        }
    });
}

fn decision_name(decision: minion_core::config::Decision) -> &'static str {
    match decision {
        minion_core::config::Decision::Auto => "auto",
        minion_core::config::Decision::Ask => "ask",
        minion_core::config::Decision::Deny => "deny",
    }
}

/// First few characters of a session id, for a compact banner.
fn short_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}
