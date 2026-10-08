//! The interactive REPL.
//!
//! Deliberately not a full-screen TUI: input is line-oriented, output is
//! appended, and scrollback is preserved. Tool activity and approvals go to
//! stderr so that piping stdout yields only the conversation.

use std::path::Path;
use std::sync::Arc;

use imp_core::agent::StopReason;
use imp_core::config::{Config, Decision};
use imp_core::error::{Error, Result};
use imp_core::message::Message;
use imp_core::provider::Usage;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cli::Cli;
use crate::run;
use crate::session;
use crate::setup::{self, Session};

/// Run the interactive loop until the user quits or stdin closes.
///
/// `resume` continues a stored conversation; without it a new one is started.
pub async fn interactive(
    cli: &Cli,
    config: &Config,
    cwd: &Path,
    resume: Option<&str>,
) -> Result<()> {
    let mut state = setup::build(cli, config, cwd, resume).await?;
    let mut editor = DefaultEditor::new()
        .map_err(|err| Error::Config(format!("cannot start the line editor: {err}")))?;

    // The scheduler lives and dies with the session: a job fires while imp is
    // running, and nothing is left behind when it exits (D5).
    let _cron = start_cron(config, &state).await;

    // Ctrl-C mid-turn cancels the turn; the token is replaced so the next turn
    // starts fresh.
    let interrupt = Arc::new(Mutex::new(CancellationToken::new()));
    spawn_interrupt_swapper(interrupt.clone());

    if !cli.quiet {
        println!(
            "imp {} · {} · {} · policy: {}",
            env!("CARGO_PKG_VERSION"),
            state.options.model,
            state.workspace_root.display(),
            decision_name(config.policy.default)
        );
        println!(
            "session {} · /help for commands, /quit to exit",
            short_id(&state.session_id)
        );
    }

    let mut usage = Usage::default();
    // Ctrl-C at the prompt asks once before it exits; the first press is easy to
    // hit by accident. A successful read re-arms the first press.
    let mut interrupts = InterruptGuard::default();

    loop {
        let line = match editor.readline("› ") {
            Ok(line) => {
                interrupts.on_line();
                line
            }
            Err(ReadlineError::Interrupted) => {
                if interrupts.on_interrupt() {
                    break;
                }
                eprintln!("press again to exit");
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(err) => {
                eprintln!("imp: input error: {err}");
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(trimmed);

        if let Some(command) = trimmed.strip_prefix('!') {
            shell_escape(command, &state.workspace_root).await;
            continue;
        }
        if let Some(command) = trimmed.strip_prefix('/') {
            match handle_slash(command, &mut state, &usage, cli).await {
                Ok(true) => break,
                Ok(false) => continue,
                Err(err) => {
                    eprintln!("imp: {err}");
                    continue;
                }
            }
        }

        let turn_start = state.history.len();
        // Retry the external servers before the turn they would serve, and give
        // a `system` message to anything that changed (§5.10).
        for notice in state.refresh_mcp().await {
            eprintln!("… {notice}");
            state.history.push(Message::system(notice));
        }
        state.history.push(Message::user(trimmed));
        // Where the reply will begin. Taken from the length rather than assumed
        // to be `turn_start + 1`, because a notice may have been pushed too.
        let reply_start = state.history.len();
        if let Err(err) = state.persist_since(turn_start).await {
            // Losing the turn is bad, but so is losing the user's prompt; say so
            // and keep going rather than exiting.
            eprintln!("imp: could not save this turn: {err}");
        }

        let (sender, receiver) = mpsc::unbounded_channel();
        let rendering = tokio::spawn(run::render_events(
            receiver,
            cli.json,
            run::style_for(cli),
            run::colour_enabled(cli),
        ));

        let cancel = interrupt.lock().await.clone();
        let agent = state.agent();
        let outcome = agent.run(&mut state.history, &sender, cancel).await;
        usage.absorb(outcome.usage);
        // Persist this turn's usage against the conversation, so `/cost` can
        // report the whole session rather than just this process (§7).
        if let Err(err) = state
            .store
            .record_usage(&state.session_id, outcome.usage)
            .await
        {
            eprintln!("imp: could not record token usage: {err}");
        }

        drop(sender);
        let _ = rendering.await;

        // Persist what the turn added, including a partial turn after Ctrl-C.
        if let Err(err) = state.persist_since(reply_start).await {
            eprintln!("imp: could not save the assistant reply: {err}");
        }

        if outcome.stop != StopReason::Completed {
            eprintln!("… turn ended: {}", outcome.stop.as_str());
        }
    }

    // The external servers belong to the session, so they go with it.
    state.shutdown().await;
    Ok(())
}

/// Start the in-process scheduler for this session, reporting what it did.
///
/// Failure is not fatal: a scheduler that cannot start must not take the REPL
/// with it, since the user is still there to work. It is loud on stderr instead.
async fn start_cron(config: &Config, state: &Session) -> Option<crate::cron::CronService> {
    match crate::cron::start(config, &state.store, state.cron_runner.clone()).await {
        Ok(Some((service, startup))) => {
            if startup.reconciled > 0 {
                eprintln!(
                    "… cron: closed {} run(s) left in flight by a previous process",
                    startup.reconciled
                );
            }
            let caught = startup.caught_up;
            if caught != imp_cron::TickReport::default() {
                eprintln!(
                    "… cron catch-up: {} fired, {} queued, {} skipped, {} overlapped",
                    caught.fired, caught.queued, caught.skipped, caught.overlapped
                );
            }
            Some(service)
        }
        Ok(None) => None,
        Err(err) => {
            eprintln!("imp: the cron scheduler did not start: {err}");
            None
        }
    }
}

/// Handle a `/command`. Returns `true` when the REPL should exit.
async fn handle_slash(
    command: &str,
    state: &mut Session,
    usage: &Usage,
    cli: &Cli,
) -> Result<bool> {
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
            for (tool, risk) in state.tools.risks() {
                println!("  {tool:<20} {}", risk.as_str());
            }
        }
        "new" => {
            state.reset().await?;
            println!("new session {}", short_id(&state.session_id));
        }
        "sessions" => {
            let rows = state.store.list_sessions(20).await?;
            if rows.is_empty() {
                println!("No conversations yet.");
            }
            for row in rows {
                println!("  {}", setup::describe_session(&row));
            }
        }
        "resume" => {
            let needle = argument
                .ok_or_else(|| Error::Config("usage: /resume <id> — see /sessions".to_string()))?;
            let id = session::resolve(&state.store, needle).await?;
            let history = state.store.load_messages(&id).await?;
            if history.is_empty() {
                return Err(Error::Config(format!("session `{needle}` has no messages")));
            }
            let mut history = history;
            setup::apply_history_window(&mut history, state.history_window);
            // Re-adopt the prompt so a resumed session behaves like a fresh one.
            let needs_system = history
                .first()
                .map(|message| message.role != imp_core::message::Role::System)
                .unwrap_or(true);
            if needs_system && let Some(system) = state.history.first().cloned() {
                history.insert(0, system);
            }
            state.adopt(&id);
            state.history = history;
            println!("resumed {id} ({} messages)", state.history.len());
        }
        "rename" => {
            let title =
                argument.ok_or_else(|| Error::Config("usage: /rename <title>".to_string()))?;
            if !state.store.rename_session(&state.session_id, title).await? {
                return Err(Error::Config(
                    "this session is no longer in the database".to_string(),
                ));
            }
            println!("renamed to {title}");
        }
        "clear" => {
            // Forget the conversation, keep the system prompt and the session id
            // so the transcript stays continuous and resumable.
            let system = state.history.first().cloned();
            state.history = system.into_iter().collect();
            println!("history cleared");
        }
        "model" => match argument {
            Some(model) => {
                state.options.model = model.to_string();
                println!("model set to {}", state.options.model);
            }
            None => {
                println!("current model: {}", state.options.model);
                match state.list_models().await {
                    Ok(models) if models.is_empty() => {
                        println!("the provider advertised no models");
                    }
                    Ok(models) => {
                        println!("available models ({}):", models.len());
                        for model in &models {
                            let marker = if *model == state.options.model {
                                "›"
                            } else {
                                " "
                            };
                            println!("  {marker} {model}");
                        }
                    }
                    Err(err) => {
                        println!("could not list models from the provider: {err}");
                    }
                }
            }
        },
        "cost" => {
            let total = state.store.session_usage(&state.session_id).await?;
            println!(
                "session: prompt {} · completion {} · total {} tokens over {} turn(s)",
                total.prompt_tokens, total.completion_tokens, total.total_tokens, total.turns
            );
            println!(
                "this process: prompt {} · completion {} · total {} tokens",
                usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
            );
        }
        "session" => {
            println!("{}", state.session_id);
            let note = "This is the conversation id substituted into any ${session} header.";
            println!("{note}");
        }
        "where" => println!("{}", state.store.path().display()),
        "cron" | "jobs" => {
            let jobs = state.store.jobs().list_jobs().await?;
            if jobs.is_empty() {
                println!("No jobs scheduled. Add one with `imp cron add`.");
            }
            for job in jobs {
                println!("  {}", imp_cron::describe(&job));
            }
        }
        other => {
            let _ = cli;
            eprintln!("unknown command `/{other}` — try /help");
        }
    }
    Ok(false)
}

fn print_help() {
    println!("  /help              this message");
    println!("  /new               start a fresh conversation");
    println!("  /sessions          list stored conversations");
    println!("  /resume <id>       continue a stored conversation");
    println!("  /rename <title>    set this conversation's title");
    println!("  /clear             forget the messages, keep the session");
    println!("  /model [name]      list the provider's models, or switch to <name>");
    println!("  /tools             list enabled tools and their risk class");
    println!("  /cron              list scheduled jobs (alias /jobs)");
    println!("  /cost              token usage for this session");
    println!("  /session           show the conversation id sent to the provider");
    println!("  /where             show the database path");
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
        Err(err) => eprintln!("imp: cannot run `{command}`: {err}"),
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

fn decision_name(decision: Decision) -> &'static str {
    match decision {
        Decision::Auto => "auto",
        Decision::Ask => "ask",
        Decision::Deny => "deny",
    }
}

/// First few characters of a session id, for a compact banner.
fn short_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

/// Tracks the "press Ctrl-C again to exit" gesture.
///
/// The first interrupt arms the exit and prints a hint; a second, consecutive
/// interrupt exits. Any line the user actually submits re-arms the first press,
/// so the gesture is "twice in a row" and not a sticky session-wide state. The
/// decision is a pure function so it can be tested without a pty.
#[derive(Debug, Default)]
struct InterruptGuard {
    armed: bool,
}

impl InterruptGuard {
    /// A line was read: the next interrupt is a fresh first press.
    fn on_line(&mut self) {
        self.armed = false;
    }

    /// A Ctrl-C arrived. Returns `true` when the REPL should exit.
    fn on_interrupt(&mut self) -> bool {
        if self.armed {
            return true;
        }
        self.armed = true;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::InterruptGuard;

    #[test]
    fn one_interrupt_arms_and_the_next_exits() {
        let mut guard = InterruptGuard::default();

        assert!(!guard.on_interrupt(), "the first press only warns");
        assert!(guard.on_interrupt(), "the second consecutive press exits");
    }

    #[test]
    fn reading_a_line_re_arms_the_first_press() {
        let mut guard = InterruptGuard::default();

        assert!(!guard.on_interrupt());
        guard.on_line();
        assert!(
            !guard.on_interrupt(),
            "after a submitted line, the next press warns again"
        );
        assert!(guard.on_interrupt());
    }
}
