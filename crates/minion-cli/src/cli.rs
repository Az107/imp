//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// A minimal, Unix-native AI agent harness.
#[derive(Debug, Parser)]
#[command(name = "minion", version, about, long_about = None)]
pub struct Cli {
    /// Model to use, overriding the config file.
    #[arg(long, global = true)]
    pub model: Option<String>,

    /// Base URL of an OpenAI-compatible endpoint (without `/chat/completions`).
    #[arg(long = "base-url", global = true)]
    pub base_url: Option<String>,

    /// Environment variable holding the API key.
    #[arg(long = "api-key-env", global = true)]
    pub api_key_env: Option<String>,

    /// Configuration file loaded last, overriding the project `minion.toml`.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Directory to treat as the workspace root.
    #[arg(long, global = true)]
    pub cwd: Option<PathBuf>,

    /// Emit newline-delimited JSON events instead of prose.
    #[arg(long, global = true)]
    pub json: bool,

    /// Disable ANSI colour.
    #[arg(long = "no-color", global = true)]
    pub no_color: bool,

    /// Increase log verbosity (`-v`, `-vv`).
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Subcommand. With none, minion starts an interactive session.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one prompt and exit.
    Run {
        /// The prompt. Use `-` to read it from stdin.
        prompt: String,
    },

    /// Configure the model backend and write a config file (§5.1.1).
    Init(InitArgs),
}

/// Arguments for `minion init`.
#[derive(Debug, Clone, clap::Args)]
pub struct InitArgs {
    /// Backend preset: openai, openrouter, opencode-go, opencode-zen, ollama, vllm, custom.
    #[arg(long)]
    pub preset: Option<String>,

    /// Base URL of the OpenAI-compatible endpoint.
    #[arg(long = "base-url")]
    pub base_url: Option<String>,

    /// Model identifier.
    #[arg(long)]
    pub model: Option<String>,

    /// Environment variable holding the API key.
    #[arg(long = "api-key-env")]
    pub api_key_env: Option<String>,

    /// The backend needs no API key (local inference).
    #[arg(long = "no-api-key")]
    pub no_api_key: bool,

    /// Do not store the API key, even though it was asked for and used to check
    /// the backend. Leaves `provider.api_key_env` as the only credential source.
    #[arg(long = "no-store-token")]
    pub no_store_token: bool,

    /// Where to store the API key. Defaults to `credentials` beside the user config.
    #[arg(long = "credentials-file")]
    pub credentials_file: Option<PathBuf>,

    /// Extra request header as `Name: value`. Repeatable.
    /// `${session}` expands to the stable id of the current conversation.
    #[arg(long = "header", value_name = "NAME: VALUE")]
    pub headers: Vec<String>,

    /// Workspace root to record in the config.
    #[arg(long)]
    pub workspace: Option<String>,

    /// Write ./minion.toml instead of the user config file.
    #[arg(long)]
    pub project: bool,

    /// Replace an existing config file.
    #[arg(long)]
    pub force: bool,

    /// Never prompt; every answer must come from a flag.
    #[arg(long = "non-interactive")]
    pub non_interactive: bool,

    /// Probe the backend first, and write nothing if it is unreachable.
    #[arg(long)]
    pub check: bool,
}
