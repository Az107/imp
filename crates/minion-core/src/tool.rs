//! Tool abstraction, risk classification, and the registry.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::provider::{ToolSchema, Usage};

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
        matches!(self, Risk::Write | Risk::Execute | Risk::Network)
    }

    /// Whether the class only observes, leaving the machine unchanged.
    ///
    /// This is the line the non-interactive rule draws: a missing terminal
    /// governs calls that change something, not calls that only read. `Network`
    /// is a change — the request leaves the machine and can be induced by
    /// untrusted content — so it sits with `Write` and `Execute`. See D15.
    pub fn is_observation(self) -> bool {
        matches!(self, Risk::ReadOnly)
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

    /// Token usage a tool reports having spent *elsewhere*, or `None`.
    ///
    /// A delegating tool (a peer call, M10.2) spends tokens on a backend that is
    /// not this turn's provider, so the loop cannot see them in the stream. The
    /// tool puts them in its `metadata` under `usage` —
    /// `{"prompt_tokens":…, "completion_tokens":…, "total_tokens":…}` — and the
    /// loop folds them into the turn's usage, which is what `/cost` reads.
    ///
    /// Advisory only (R8): a missing, non-object or non-numeric value is
    /// `None`, never an error, so a tool that forgets to report simply does not
    /// count.
    pub fn reported_usage(&self) -> Option<Usage> {
        let usage = self.metadata.get("usage")?.as_object()?;
        let field = |name: &str| -> u32 {
            usage
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32
        };
        Some(Usage {
            prompt_tokens: field("prompt_tokens"),
            completion_tokens: field("completion_tokens"),
            total_tokens: field("total_tokens"),
        })
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

/// A source of tools that is consulted on every catalogue read.
///
/// A registry built at session assembly cannot describe a capability that comes
/// and goes, and the MCP client is exactly that (SDD §5.10): an external server
/// is spawned, may fall over, and is retried on the next turn. Implementing this
/// trait lets that set change under a *frozen* registry, so the catalog the model
/// sees and the tool a call resolves to are always the same live answer.
///
/// Ordering is the shadowing rule: registered tools come first and a name taken
/// there is never handed to a catalog, so `mcp__…` tools cannot displace a
/// built-in no matter what a server calls itself. Between catalogs, the first to
/// offer a name keeps it.
pub trait ToolCatalog: Send + Sync {
    /// The tools currently on offer, in the order they should be advertised.
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

/// The set of tools currently offered to the model.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    catalogs: Vec<Arc<dyn ToolCatalog>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool. Registration order is the order schemas are advertised in.
    pub fn register(&mut self, tool: impl Tool + 'static) -> &mut Self {
        self.tools.push(Arc::new(tool));
        self
    }

    /// Add a tool that is already shared.
    ///
    /// The same ordering rule as [`register`](Self::register), for a tool the
    /// caller keeps a handle on — a peer delegation tool, whose connection the
    /// session has to close on the way out (M10.2).
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.tools.push(tool);
        self
    }

    /// Build a registry from an explicit set of tools, in the given order.
    ///
    /// This is how a *subset* of the registered tools is assembled — the MCP
    /// server's read-only inner surface is one (§5.9) — without a filter pass
    /// that would have to re-register by name. The caller owns the ordering, so
    /// it is the same ordering rule as [`register`](Self::register).
    pub fn from_tools(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            tools,
            catalogs: Vec::new(),
        }
    }

    /// Attach a runtime source of tools, consulted after every registered one.
    pub fn attach_catalog(&mut self, catalog: Arc<dyn ToolCatalog>) -> &mut Self {
        self.catalogs.push(catalog);
        self
    }

    /// Every tool, registered ones first, one entry per name.
    ///
    /// This is where "never shadow" is enforced rather than documented: a name
    /// already taken is skipped, so a registered tool always wins and two
    /// catalogs cannot overwrite each other.
    fn all(&self) -> Vec<Arc<dyn Tool>> {
        let mut seen: Vec<&'static str> = self.tools.iter().map(|tool| tool.name()).collect();
        let mut all = self.tools.clone();
        for catalog in &self.catalogs {
            for tool in catalog.tools() {
                if seen.contains(&tool.name()) {
                    continue;
                }
                seen.push(tool.name());
                all.push(tool);
            }
        }
        all
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.all().into_iter().find(|tool| tool.name() == name)
    }

    /// Schemas for every tool on offer.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.all()
            .into_iter()
            .map(|tool| ToolSchema {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.schema(),
            })
            .collect()
    }

    /// Names and risk classes, for `--verbose` output and `/tools`.
    pub fn risks(&self) -> Vec<(&'static str, Risk)> {
        self.all()
            .into_iter()
            .map(|tool| (tool.name(), tool.risk()))
            .collect()
    }

    /// Number of tools on offer.
    pub fn len(&self) -> usize {
        self.all().len()
    }

    /// Whether no tool is on offer.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.catalogs.iter().all(|c| c.tools().is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// A tool that answers to `name` and declares `risk`.
    struct Named {
        name: &'static str,
        risk: Risk,
    }

    #[async_trait]
    impl Tool for Named {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "test tool"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        fn risk(&self) -> Risk {
            self.risk
        }
        async fn invoke(&self, _ctx: ToolCtx, _args: serde_json::Value) -> Result<ToolOutput> {
            Ok(ToolOutput::text(self.name))
        }
    }

    fn named(name: &'static str) -> Arc<dyn Tool> {
        Arc::new(Named {
            name,
            risk: Risk::Network,
        })
    }

    /// A catalog whose contents can be swapped, standing in for a server that
    /// comes up after the registry was built.
    #[derive(Default)]
    struct Swappable(std::sync::RwLock<Vec<Arc<dyn Tool>>>);

    impl Swappable {
        fn set(&self, tools: Vec<Arc<dyn Tool>>) {
            *self.0.write().unwrap() = tools;
        }
    }

    impl ToolCatalog for Swappable {
        fn tools(&self) -> Vec<Arc<dyn Tool>> {
            self.0.read().unwrap().clone()
        }
    }

    fn schema_names(registry: &ToolRegistry) -> Vec<String> {
        registry
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect()
    }

    #[test]
    fn a_catalog_tool_is_offered_and_callable() {
        let mut registry = ToolRegistry::new();
        registry.register(Named {
            name: "read_file",
            risk: Risk::ReadOnly,
        });
        let catalog = Arc::new(Swappable::default());
        catalog.set(vec![named("mcp__files__read")]);
        registry.attach_catalog(catalog);

        assert_eq!(
            schema_names(&registry),
            vec!["read_file", "mcp__files__read"]
        );
        assert!(registry.get("mcp__files__read").is_some());
        assert_eq!(registry.len(), 2);
    }

    /// A registered tool always wins. This is the property that keeps a server
    /// from publishing a name the harness already owns.
    #[test]
    fn a_catalog_never_shadows_a_registered_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(Named {
            name: "read_file",
            risk: Risk::ReadOnly,
        });
        let catalog = Arc::new(Swappable::default());
        catalog.set(vec![named("read_file")]);
        registry.attach_catalog(catalog);

        assert_eq!(
            schema_names(&registry),
            vec!["read_file"],
            "the name must be advertised once"
        );
        assert_eq!(
            registry.get("read_file").unwrap().risk(),
            Risk::ReadOnly,
            "the call must resolve to the registered tool, not the catalog one"
        );
    }

    #[test]
    fn the_first_catalog_to_offer_a_name_keeps_it() {
        let mut registry = ToolRegistry::new();
        let first = Arc::new(Swappable::default());
        first.set(vec![named("mcp__a__b")]);
        let second = Arc::new(Swappable::default());
        second.set(vec![named("mcp__a__b")]);
        registry.attach_catalog(first);
        registry.attach_catalog(second);

        assert_eq!(schema_names(&registry), vec!["mcp__a__b"]);
    }

    /// The point of the trait: a frozen registry follows a catalog that changes.
    #[test]
    fn a_catalog_that_changes_is_seen_immediately() {
        let mut registry = ToolRegistry::new();
        let catalog = Arc::new(Swappable::default());
        registry.attach_catalog(catalog.clone());

        assert!(registry.is_empty(), "a server that is down offers nothing");

        catalog.set(vec![named("mcp__files__read")]);

        assert!(!registry.is_empty());
        assert_eq!(schema_names(&registry), vec!["mcp__files__read"]);

        catalog.set(Vec::new());
        assert!(registry.get("mcp__files__read").is_none());
        assert!(registry.is_empty());
    }
}
