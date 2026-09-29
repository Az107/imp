//! Tool abstraction, risk classification, and the registry.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::provider::ToolSchema;

/// How dangerous a tool is.
///
/// Drives the approval policy and the default MCP exposure surface: `Execute`
/// and `Write` are the classes that must never become implicitly reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Inspects state without changing it.
    ReadOnly,
    /// Mutates files or persisted state.
    Write,
    /// Runs arbitrary processes.
    Execute,
    /// Reaches the network.
    Network,
}

impl Risk {
    /// Stable lowercase name, used in logs and audit records.
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::ReadOnly => "read_only",
            Risk::Write => "write",
            Risk::Execute => "execute",
            Risk::Network => "network",
        }
    }

    /// Whether this risk class requires consent by default.
    pub fn requires_consent(self) -> bool {
        matches!(self, Risk::Write | Risk::Execute)
    }
}

/// Text handed back to the model, plus metadata for the UI and audit trail.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Content placed in the `tool` message.
    pub content: String,
    /// Whether [`content`](Self::content) was cut to fit the output cap.
    pub truncated: bool,
    /// Structured detail (exit codes, byte counts, ...). Never sent to the model.
    pub metadata: serde_json::Value,
}

impl ToolOutput {
    /// A plain-text result.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            truncated: false,
            metadata: serde_json::Value::Null,
        }
    }

    /// A JSON result, rendered as a compact string for the model.
    pub fn json(value: serde_json::Value) -> Self {
        Self {
            content: value.to_string(),
            truncated: false,
            metadata: serde_json::Value::Null,
        }
    }

    /// Attach structured metadata.
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Per-invocation context handed to tools.
///
/// The workspace root is the single boundary every path-taking tool must honour.
#[derive(Debug, Clone)]
pub struct ToolCtx {
    /// Canonical workspace root. Paths outside it are rejected.
    pub workspace_root: PathBuf,
    /// Cancelled when the turn is interrupted; tools should stop promptly.
    pub cancel: CancellationToken,
}

impl ToolCtx {
    /// Resolve `raw` against the workspace root, rejecting anything that escapes.
    ///
    /// Accepts relative paths (joined to the root) and absolute paths that are
    /// still inside it. The result is canonicalized so that `..` segments and
    /// symlinks cannot be used to break out, even for paths that do not exist yet.
    pub fn resolve(&self, raw: &str) -> Result<PathBuf> {
        let root = normalize(&self.workspace_root)?;
        let candidate = {
            let path = Path::new(raw);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            }
        };
        let resolved = normalize(&candidate)?;
        if !resolved.starts_with(&root) {
            return Err(Error::Denied(format!(
                "path `{raw}` escapes the workspace root `{}`",
                root.display()
            )));
        }
        Ok(resolved)
    }
}

/// Canonicalize `path`, tolerating components that do not exist yet.
///
/// `Path::canonicalize` requires the whole path to exist, which is useless for
/// a file about to be written. This walks up to the deepest existing ancestor,
/// canonicalizes that, and re-appends the missing tail.
fn normalize(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let mut tail: Vec<OsString> = Vec::new();
    let mut current = path.to_path_buf();
    while let Some(parent) = current.parent().map(Path::to_path_buf) {
        let Some(name) = current.file_name().map(OsString::from) else {
            break;
        };
        tail.push(name);
        if parent.exists() {
            let mut base = parent.canonicalize()?;
            for part in tail.iter().rev() {
                base.push(part);
            }
            return Ok(base);
        }
        current = parent;
    }
    Err(Error::Config(format!(
        "cannot resolve path `{}`",
        path.display()
    )))
}

/// A capability the model can invoke.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Registered name, matching the schema and the model's tool call.
    fn name(&self) -> &'static str;
    /// One-line description used to prompt the model.
    fn description(&self) -> &'static str;
    /// JSON Schema for the arguments object.
    fn schema(&self) -> serde_json::Value;
    /// How dangerous this tool is.
    fn risk(&self) -> Risk;
    /// What allow and deny rules are matched against, usually the path or
    /// command. `None` falls back to a value derived from the arguments.
    ///
    /// This is the only place a tool influences policy, and it chooses the
    /// *subject* of a match rather than the outcome.
    fn approval_subject(&self, _args: &serde_json::Value) -> Option<String> {
        None
    }
    /// Wall-clock budget for a single invocation.
    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }
    /// Run the tool.
    async fn invoke(&self, ctx: ToolCtx, args: serde_json::Value) -> Result<ToolOutput>;
}

/// The set of tools currently offered to the model.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool. Registration order is the order schemas are advertised in.
    pub fn register(&mut self, tool: impl Tool + 'static) -> &mut Self {
        self.tools.push(Box::new(tool));
        self
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(Box::as_ref)
    }

    /// Schemas for every registered tool.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .iter()
            .map(|tool| ToolSchema {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.schema(),
            })
            .collect()
    }

    /// Names and risk classes, for `--verbose` output and `/tools`.
    pub fn risks(&self) -> Vec<(&'static str, Risk)> {
        self.tools
            .iter()
            .map(|tool| (tool.name(), tool.risk()))
            .collect()
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
