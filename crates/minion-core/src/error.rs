//! Error types shared across minion crates.

use thiserror::Error;

/// Library-wide result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors produced by the agent core and its collaborators.
#[derive(Debug, Error)]
pub enum Error {
    /// Configuration was missing, malformed, or contradictory.
    #[error("configuration error: {0}")]
    Config(String),

    /// The model provider returned an error or an unusable response.
    #[error("provider error: {0}")]
    Provider(String),

    /// The provider rejected the request itself (bad model name, context
    /// overflow, malformed tools). Retrying will not help.
    #[error("provider rejected the request ({status}): {message}")]
    BadRequest {
        /// HTTP status returned.
        status: u16,
        /// Response body, which usually carries the provider's explanation.
        message: String,
    },

    /// Authentication with the provider failed.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// The provider rate limited us; the caller may retry after the delay.
    #[error("rate limited (retry after {retry_after:?})")]
    RateLimit {
        /// Delay requested by the provider, if it sent one.
        retry_after: Option<std::time::Duration>,
    },

    /// The model requested a tool that is not registered.
    ///
    /// The names that *would* have resolved are carried so a small model that
    /// hallucinated an adjacent name can correct itself on the next iteration.
    #[error("unknown tool `{name}`; available: {available:?}")]
    UnknownTool {
        /// The name the model asked for.
        name: String,
        /// Tool names that would have resolved.
        available: Vec<String>,
    },

    /// Tool arguments failed validation.
    #[error("invalid arguments for tool `{tool}`: {message}")]
    ToolArgs {
        /// Tool that rejected the arguments.
        tool: String,
        /// Human-readable reason.
        message: String,
    },

    /// A tool ran but failed.
    #[error("tool `{tool}` failed: {message}")]
    Tool {
        /// Tool that failed.
        tool: String,
        /// Human-readable reason.
        message: String,
    },

    /// Policy refused the action.
    #[error("denied by policy: {0}")]
    Denied(String),

    /// The turn was cancelled by the user or a shutdown signal.
    #[error("operation cancelled")]
    Cancelled,

    /// The SQLite store could not be read or written.
    #[error("store error: {0}")]
    Store(String),

    /// Filesystem error.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// JSON (de)serialization error.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
