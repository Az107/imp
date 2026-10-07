//! `minion session` — inspect and manage stored conversations.

use std::process::ExitCode;

use minion_core::config::Config;
use minion_core::error::{Error, Result};
use minion_core::message::Role;
use minion_store::Store;

use crate::cli::{Cli, SessionAction, SessionArgs};
use crate::repl;

/// Exit code when a requested session does not exist.
const EXIT_NOT_FOUND: u8 = 4;

/// Run a `minion session` subcommand.
pub async fn run(cli: &Cli, config: &Config, args: SessionArgs) -> Result<ExitCode> {
    let cwd = cli
        .cwd
        .clone()
        .map(|dir| dir.canonicalize().unwrap_or(dir))
        .unwrap_or(std::env::current_dir()?);
    let database = cli.db.clone().unwrap_or_else(|| config.database_path());
    let store = Store::open(&database).await?;

    match &args.action {
        SessionAction::List { limit } => list(cli, &store, *limit).await,
        SessionAction::Show { id } => show(cli, &store, id).await,
        SessionAction::Rm { id } => remove(cli, &store, id).await,
        SessionAction::Resume { id } => {
            // Resolution first, so a bad id fails before the REPL takes the
            // terminal and the user is left staring at a prompt.
            let resolved = resolve(&store, id).await?;
            drop(store);
            repl::interactive(cli, config, &cwd, Some(&resolved)).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn list(cli: &Cli, store: &Store, limit: usize) -> Result<ExitCode> {
    let rows = store.list_sessions(limit).await?;

    if cli.json {
        let items: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "type": "session",
                    "id": row.id,
                    "title": row.title,
                    "cwd": row.cwd,
                    "model": row.model,
                    "provider": row.provider,
                    "created_at": row.created_at,
                    "updated_at": row.updated_at,
                })
            })
            .collect();
        for item in items {
            println!("{item}");
        }
        return Ok(ExitCode::SUCCESS);
    }

    if rows.is_empty() {
        println!("No conversations yet.");
        return Ok(ExitCode::SUCCESS);
    }
    let header = format!("{:<10}  {:<25}  TITLE", "ID", "UPDATED");
    println!("{header}");
    for row in &rows {
        let title = row
            .title
            .clone()
            .unwrap_or_else(|| "(untitled)".to_string());
        println!(
            "{:<10}  {:<25}  {}",
            &row.id[..8.min(row.id.len())],
            row.updated_at,
            title
        );
    }
    Ok(ExitCode::SUCCESS)
}

async fn show(cli: &Cli, store: &Store, id: &str) -> Result<ExitCode> {
    let id = resolve(store, id).await?;
    let row = store.session(&id).await?;
    let messages = store.load_messages(&id).await?;

    if cli.json {
        for message in &messages {
            println!(
                "{}",
                serde_json::json!({
                    "type": "message",
                    "session": id,
                    "role": message.role,
                    "content": message.content,
                    "tool_call_id": message.tool_call_id,
                    "tool_calls": message.tool_calls,
                })
            );
        }
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(row) = row {
        println!(
            "{}  {}",
            &row.id[..8.min(row.id.len())],
            row.title.clone().unwrap_or_default()
        );
        println!(
            "workspace {}  model {}",
            row.cwd,
            row.model.clone().unwrap_or_default()
        );
        println!();
    }
    for message in &messages {
        if message.role == Role::System {
            continue;
        }
        let tag = match message.role {
            Role::User => "you",
            Role::Assistant => "minion",
            Role::Tool => "tool",
            Role::System => continue,
        };
        if let Some(text) = &message.content {
            println!("{tag}: {text}");
        }
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                println!("  ▸ {} {}", call.function.name, call.function.arguments);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn remove(cli: &Cli, store: &Store, id: &str) -> Result<ExitCode> {
    let id = resolve(store, id).await?;
    let deleted = store.delete_session(&id).await?;

    if cli.json {
        println!(
            "{}",
            serde_json::json!({ "type": "session_deleted", "id": id, "deleted": deleted })
        );
    } else if deleted {
        println!("deleted {}", &id[..8.min(id.len())]);
    } else {
        println!("no session matches `{id}`");
    }
    Ok(if deleted {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_NOT_FOUND)
    })
}

/// Accept a full id, a unique prefix, or a 1-based position from `list`.
///
/// Typing a 32-character uuid is a usability trap, so the short forms the list
/// prints are enough to act on.
pub async fn resolve(store: &Store, needle: &str) -> Result<String> {
    let needle = needle.trim();

    if let Some(row) = store.session(needle).await? {
        return Ok(row.id);
    }
    if let Ok(position) = needle.parse::<usize>()
        && position >= 1
    {
        let rows = store.list_sessions(position).await?;
        if let Some(row) = rows.get(position - 1) {
            return Ok(row.id.clone());
        }
    }

    let rows = store.list_sessions(200).await?;
    let matches: Vec<&str> = rows
        .iter()
        .filter(|row| row.id.starts_with(needle))
        .map(|row| row.id.as_str())
        .collect();
    match matches.as_slice() {
        [only] => Ok((*only).to_string()),
        [] => Err(Error::Config(format!("no session matches `{needle}`"))),
        many => Err(Error::Config(format!(
            "`{needle}` is ambiguous: {} conversations match",
            many.len()
        ))),
    }
}
