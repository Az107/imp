//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// A minimal, Unix-native AI agent harness.
#[derive(Debug, Parser)]
#[command(name = "minion", version = env!("MINION_VERSION"), about, long_about = None)]
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

    /// Session database to use instead of the default state directory.
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,

    /// Directory to treat as the workspace root.
    #[arg(long, global = true)]
    pub cwd: Option<PathBuf>,

    /// Assume consent for anything policy would otherwise ask about, and never
    /// prompt. Dangerous: it also means a non-interactive run may execute.
    #[arg(long, global = true)]
    pub yes: bool,

    /// Refuse anything not explicitly allowlisted, prompting never.
    #[arg(long, global = true, conflicts_with = "yes")]
    pub deny: bool,

    /// Emit newline-delimited JSON events instead of prose.
    #[arg(long, global = true)]
    pub json: bool,

    /// Disable ANSI colour. Layout such as table borders is kept.
    #[arg(long = "no-color", global = true)]
    pub no_color: bool,

    /// Render markdown in the terminal. On by default when stdout is a
    /// terminal; ignored when output is piped, where raw markdown is kept.
    #[arg(long = "markdown", global = true, overrides_with = "no_markdown")]
    pub markdown: bool,

    /// Never render markdown, even on a terminal.
    #[arg(long = "no-markdown", global = true)]
    pub no_markdown: bool,

    /// Wrap width in columns. Defaults to $COLUMNS, then 80.
    #[arg(long, global = true)]
    pub width: Option<usize>,

    /// Increase log verbosity (`-v`, `-vv`).
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Reduce output to results and errors: no banner, and warnings only.
    #[arg(long, short, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Resume a stored conversation, by id, unique prefix, or list position.
    #[arg(long, global = true, value_name = "SESSION")]
    pub resume: Option<String>,

    /// Maximum provider round-trips per turn, overriding the config.
    #[arg(long = "max-iterations", global = true, value_name = "N")]
    pub max_iterations: Option<u32>,

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

    /// Inspect the effective configuration (§5.12).
    Config(ConfigArgs),

    /// Check the environment, config, database and provider reachability.
    Doctor,

    /// Inspect and manage stored conversations.
    Session(SessionArgs),

    /// Manage scheduled jobs (§5.12).
    Cron(CronArgs),

    /// Inspect the external MCP servers minion consumes (§5.10, §5.12).
    Mcp(McpArgs),
}

/// Arguments for `minion mcp`.
#[derive(Debug, Clone, clap::Args)]
pub struct McpArgs {
    /// The action to perform.
    #[command(subcommand)]
    pub action: McpAction,
}

/// MCP client actions.
///
/// `mcp serve` — the other direction — is M6: it publishes minion to an MCP
/// host over stdio. The two are mutually exclusive by construction (R5): the
/// server owns stdin/stdout, so it never starts the REPL.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum McpAction {
    /// List configured servers, whether they came up, and the tools they offer.
    List,

    /// Inspect one server: everything it lists, and what minion publishes of it.
    Tools {
        /// The server's name, as it appears under `[mcp.client.servers]`.
        server: String,
    },

    /// Run as an MCP server over stdio, exposing the agent to another model.
    Serve {
        /// Use stdio. The only transport there is; `[mcp.server].transport`
        /// must also say `stdio`.
        #[arg(long)]
        stdio: bool,
    },
}

/// Arguments for `minion cron`.
#[derive(Debug, Clone, clap::Args)]
pub struct CronArgs {
    /// The action to perform.
    #[command(subcommand)]
    pub action: CronAction,
}

/// Cron actions.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum CronAction {
    /// Create a job.
    Add {
        /// Five-field cron expression: minute hour day-of-month month day-of-week.
        #[arg(long)]
        schedule: String,
        /// The prompt to run on each fire.
        #[arg(long)]
        prompt: String,
        /// Optional unique label.
        #[arg(long)]
        name: Option<String>,
        /// Workspace the run executes in. Defaults to the current directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// IANA timezone. Defaults to `cron.timezone`.
        #[arg(long)]
        timezone: Option<String>,
        /// `new` (default) or `reuse`.
        #[arg(long = "session-mode")]
        session_mode: Option<String>,
        /// Stop after this many runs.
        #[arg(long = "max-runs")]
        max_runs: Option<i64>,
        /// Allow a run to start while the previous one is still active.
        #[arg(long = "allow-overlap")]
        allow_overlap: bool,
    },

    /// List scheduled jobs.
    List,

    /// Delete a job by id or name.
    Remove {
        /// Job id, or its name.
        key: String,
    },
}

/// Arguments for `minion session`.
#[derive(Debug, Clone, clap::Args)]
pub struct SessionArgs {
    /// The action to perform.
    #[command(subcommand)]
    pub action: SessionAction,
}

/// Session actions.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum SessionAction {
    /// List stored conversations, newest first.
    List {
        /// How many to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Print a conversation's transcript.
    Show {
        /// Session id, or a numeric position from `list`.
        id: String,
    },
    /// Delete a conversation and its messages.
    Rm {
        /// Session id.
        id: String,
    },
    /// Continue a conversation in the REPL.
    Resume {
        /// Session id.
        id: String,
    },
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

/// Arguments for `minion config`.
#[derive(Debug, Clone, clap::Args)]
pub struct ConfigArgs {
    /// The action to perform.
    #[command(subcommand)]
    pub action: ConfigAction,
}

/// Read-only configuration views (§5.12). `config init` aliases `minion init`.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum ConfigAction {
    /// Print the effective configuration, secrets redacted.
    Show,

    /// Print the paths minion reads and writes.
    Path,

    /// Alias for `minion init`.
    Init(InitArgs),
}
