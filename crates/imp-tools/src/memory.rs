//! `remember` and `recall`: durable, workspace-scoped keyword memory.
//!
//! Memory is the one tool family that needs the store, so these two hold an
//! `Arc<Store>` rather than reaching for a global. The alternative — putting
//! the database in `ToolCtx` — would force every other tool to carry a handle
//! it never uses, and would put a database dependency into `imp-core`,
//! which has none on purpose.
//!
//! The namespace is derived from `ctx.workspace_root` on every call rather than
//! captured at construction. The registry is built once per process but the
//! tools stay correct if a future caller builds one registry across two
//! workspaces, and it keeps the two tools honest about the scope they promise.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use imp_core::error::{Error, Result};
use imp_core::memory::namespace_for;
use imp_core::tool::{Risk, Tool, ToolCtx, ToolOutput};
use imp_store::{MemoryEntry, Store, Written};

/// Matches returned when the caller does not say.
const DEFAULT_LIMIT: usize = 5;
/// Ceiling on `limit`, so one recall cannot flood the context.
const MAX_LIMIT: usize = 25;
/// Longest stored value, to keep a runaway write out of the database.
const MAX_VALUE_BYTES: usize = 16 * 1024;
/// Longest single key, and a cap on a single line of what recall returns.
const MAX_KEY_CHARS: usize = 200;
/// Longest value included in a recall result. The model usually needs the gist;
/// the full text is one `recall` away in the database.
const MAX_SNIPPET_CHARS: usize = 400;

/// Arguments accepted by [`Remember`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RememberArgs {
    /// Short stable label, unique within this workspace. Re-using a key
    /// overwrites the previous value.
    pub key: String,
    /// The fact to keep.
    pub value: String,
    /// Optional labels, searchable alongside the key and value.
    pub tags: Option<Vec<String>>,
}

/// Arguments accepted by [`Recall`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallArgs {
    /// Words to search for. Every word must appear somewhere in a match.
    pub query: String,
    /// Maximum results, best first. Defaults to 5.
    pub limit: Option<usize>,
}

/// Stores a fact for later recall.
pub struct Remember {
    store: Arc<Store>,
}

impl Remember {
    /// Build the tool over a shared store.
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for Remember {
    fn name(&self) -> &'static str {
        "remember"
    }

    fn description(&self) -> &'static str {
        "Store a fact for later in this workspace. Re-using a key overwrites it. \
         Use for durable preferences, conventions, and decisions; not for transient state."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(RememberArgs)).unwrap_or_default()
    }

    fn risk(&self) -> Risk {
        // Mutates persisted state, so it is a `Write` and goes through the gate
        // like any other. Read-only in effect — it cannot touch the workspace —
        // but the class describes what changes, not how much that matters.
        Risk::Write
    }

    fn approval_subject(&self, args: &serde_json::Value) -> Option<String> {
        // Match the key, so a persistent allowlist entry can name the fact
        // being written rather than blessing every `remember` for the session.
        args.get("key")
            .and_then(|key| key.as_str())
            .map(str::to_string)
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    async fn invoke(&self, ctx: ToolCtx, args: serde_json::Value) -> Result<ToolOutput> {
        let args: RememberArgs = serde_json::from_value(args).map_err(|err| Error::ToolArgs {
            tool: self.name().to_string(),
            message: err.to_string(),
        })?;

        let key = args.key.trim();
        if key.is_empty() {
            return Err(Error::ToolArgs {
                tool: self.name().to_string(),
                message: "`key` must not be empty".to_string(),
            });
        }
        if key.chars().count() > MAX_KEY_CHARS {
            return Err(Error::ToolArgs {
                tool: self.name().to_string(),
                message: format!("`key` is longer than {MAX_KEY_CHARS} characters"),
            });
        }
        if args.value.trim().is_empty() {
            return Err(Error::ToolArgs {
                tool: self.name().to_string(),
                message: "`value` must not be empty".to_string(),
            });
        }
        if args.value.len() > MAX_VALUE_BYTES {
            return Err(Error::ToolArgs {
                tool: self.name().to_string(),
                message: format!(
                    "`value` is {} bytes, over the {MAX_VALUE_BYTES} byte cap",
                    args.value.len()
                ),
            });
        }

        let outcome = self
            .store
            .remember(
                &namespace_for(&ctx.workspace_root),
                key,
                &args.value,
                &args.tags.unwrap_or_default(),
                None,
            )
            .await?;

        let verb = match outcome {
            Written::Created => "stored",
            Written::Replaced => "replaced",
        };
        Ok(ToolOutput::text(format!("{verb} `{key}`"))
            .with_metadata(serde_json::json!({ "key": key, "outcome": verb })))
    }
}

/// Searches previously stored facts.
pub struct Recall {
    store: Arc<Store>,
}

impl Recall {
    /// Build the tool over a shared store.
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for Recall {
    fn name(&self) -> &'static str {
        "recall"
    }

    fn description(&self) -> &'static str {
        "Search facts stored earlier in this workspace. Every word must appear in a match; \
         best matches first. Returns nothing when there is no match."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(RecallArgs)).unwrap_or_default()
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    async fn invoke(&self, ctx: ToolCtx, args: serde_json::Value) -> Result<ToolOutput> {
        let args: RecallArgs = serde_json::from_value(args).map_err(|err| Error::ToolArgs {
            tool: self.name().to_string(),
            message: err.to_string(),
        })?;

        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let entries = self
            .store
            .recall(&namespace_for(&ctx.workspace_root), &args.query, limit)
            .await?;

        if entries.is_empty() {
            // An empty result is a legitimate answer, not a failure, so it is
            // stated plainly instead of being dressed as a tool error.
            return Ok(ToolOutput::text(format!(
                "no memory in this workspace matches `{}`",
                args.query.trim()
            ))
            .with_metadata(serde_json::json!({ "matches": 0 })));
        }

        let count = entries.len();
        let body = entries.iter().map(render).collect::<Vec<_>>().join("\n\n");

        Ok(ToolOutput::text(body).with_metadata(serde_json::json!({ "matches": count })))
    }
}

/// One match as the model sees it: key, value, and any tags.
fn render(entry: &MemoryEntry) -> String {
    let mut line = format!("- {}: {}", entry.key, snippet(&entry.value));
    if !entry.tags.is_empty() {
        line.push_str(&format!("  [{}]", entry.tags.join(", ")));
    }
    line
}

/// Clip a value to something readable, on a word boundary where possible.
fn snippet(value: &str) -> String {
    if value.chars().count() <= MAX_SNIPPET_CHARS {
        return value.to_string();
    }
    let clipped: String = value.chars().take(MAX_SNIPPET_CHARS).collect();
    match clipped.rfind(char::is_whitespace) {
        Some(cut) if cut > MAX_SNIPPET_CHARS / 2 => format!("{}…", clipped[..cut].trim_end()),
        _ => format!("{clipped}…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use imp_core::tool::ToolRegistry;
    use tokio_util::sync::CancellationToken;

    async fn registry() -> ToolRegistry {
        let store = Arc::new(Store::open_in_memory().await.expect("in-memory store"));
        let mut registry = ToolRegistry::new();
        registry.register(Remember::new(store.clone()));
        registry.register(Recall::new(store));
        registry
    }

    fn ctx(root: &str) -> ToolCtx {
        ToolCtx {
            workspace_root: std::path::PathBuf::from(root),
            cancel: CancellationToken::new(),
        }
    }

    async fn remember(registry: &ToolRegistry, root: &str, args: serde_json::Value) -> ToolOutput {
        registry
            .get("remember")
            .expect("remember is registered")
            .invoke(ctx(root), args)
            .await
            .expect("remember succeeds")
    }

    async fn recall(registry: &ToolRegistry, root: &str, args: serde_json::Value) -> ToolOutput {
        registry
            .get("recall")
            .expect("recall is registered")
            .invoke(ctx(root), args)
            .await
            .expect("recall succeeds")
    }

    /// The arguments object as the model sends it.
    fn args(value: serde_json::Value) -> serde_json::Value {
        value
    }

    #[tokio::test]
    async fn a_fact_survives_a_write_and_a_read() {
        let registry = registry().await;
        remember(
            &registry,
            "/ws",
            args(serde_json::json!({ "key": "editor", "value": "the project uses tabs" })),
        )
        .await;

        let found = recall(
            &registry,
            "/ws",
            args(serde_json::json!({ "query": "editor" })),
        )
        .await;
        assert!(
            found.content.contains("the project uses tabs"),
            "{}",
            found.content
        );
    }

    #[tokio::test]
    async fn a_rewrite_is_reported_as_a_replacement() {
        let registry = registry().await;
        let first = remember(
            &registry,
            "/ws",
            args(serde_json::json!({ "key": "k", "value": "old" })),
        )
        .await;
        assert!(first.content.contains("stored"));

        let second = remember(
            &registry,
            "/ws",
            args(serde_json::json!({ "key": "k", "value": "new" })),
        )
        .await;
        assert!(second.content.contains("replaced"), "{}", second.content);

        let found = recall(&registry, "/ws", args(serde_json::json!({ "query": "k" }))).await;
        assert!(found.content.contains("new"));
        assert!(!found.content.contains("old"));
    }

    #[tokio::test]
    async fn a_fact_does_not_cross_into_another_workspace() {
        let registry = registry().await;
        remember(
            &registry,
            "/ws-one",
            args(serde_json::json!({ "key": "deploy", "value": "ask before deploying" })),
        )
        .await;

        let elsewhere = recall(
            &registry,
            "/ws-two",
            args(serde_json::json!({ "query": "deploy" })),
        )
        .await;
        assert!(
            elsewhere.content.contains("no memory"),
            "{}",
            elsewhere.content
        );
        assert_eq!(elsewhere.metadata["matches"], 0);
    }

    #[tokio::test]
    async fn a_miss_is_a_plain_answer_not_an_error() {
        let registry = registry().await;
        let found = recall(
            &registry,
            "/ws",
            args(serde_json::json!({ "query": "nothing stored yet" })),
        )
        .await;
        assert!(found.content.contains("no memory"), "{}", found.content);
        assert!(!found.truncated);
    }

    #[tokio::test]
    async fn tags_come_back_with_the_match() {
        let registry = registry().await;
        remember(
            &registry,
            "/ws",
            args(serde_json::json!({
                "key": "test runner",
                "value": "cargo nextest",
                "tags": ["build", "ci"]
            })),
        )
        .await;

        let found = recall(
            &registry,
            "/ws",
            args(serde_json::json!({ "query": "nextest" })),
        )
        .await;
        assert!(found.content.contains("build, ci"), "{}", found.content);
    }

    #[tokio::test]
    async fn remember_is_a_write_and_recall_is_read_only() {
        // The risk classes are what put `remember` behind the approval gate.
        let registry = registry().await;
        assert_eq!(registry.get("remember").unwrap().risk(), Risk::Write);
        assert_eq!(registry.get("recall").unwrap().risk(), Risk::ReadOnly);
    }

    #[tokio::test]
    async fn the_approval_subject_is_the_key() {
        let registry = registry().await;
        let subject = registry
            .get("remember")
            .unwrap()
            .approval_subject(&args(serde_json::json!({ "key": "deploy" })))
            .expect("a subject");
        assert_eq!(subject, "deploy");
    }

    #[tokio::test]
    async fn an_empty_key_is_refused() {
        let registry = registry().await;
        let outcome = registry
            .get("remember")
            .unwrap()
            .invoke(
                ctx("/ws"),
                serde_json::json!({ "key": "   ", "value": "something" }),
            )
            .await;
        assert!(outcome.is_err(), "a blank key must not be storable");
    }

    #[tokio::test]
    async fn an_empty_value_is_refused() {
        let registry = registry().await;
        let outcome = registry
            .get("remember")
            .unwrap()
            .invoke(ctx("/ws"), serde_json::json!({ "key": "k", "value": "  " }))
            .await;
        assert!(outcome.is_err(), "a blank value must not be storable");
    }

    #[tokio::test]
    async fn an_oversized_value_is_refused_rather_than_stored() {
        let registry = registry().await;
        let huge = "x".repeat(MAX_VALUE_BYTES + 1);
        let outcome = registry
            .get("remember")
            .unwrap()
            .invoke(ctx("/ws"), serde_json::json!({ "key": "k", "value": huge }))
            .await;
        assert!(outcome.is_err());
    }

    #[tokio::test]
    async fn a_long_value_comes_back_as_a_snippet() {
        let registry = registry().await;
        let long = format!("{} end", "a".repeat(MAX_SNIPPET_CHARS * 2));
        remember(
            &registry,
            "/ws",
            serde_json::json!({ "key": "long", "value": long }),
        )
        .await;

        let found = recall(
            &registry,
            "/ws",
            args(serde_json::json!({ "query": "long" })),
        )
        .await;
        assert!(found.content.contains('…'), "{}", found.content);
        assert!(found.content.chars().count() < MAX_SNIPPET_CHARS * 2);
    }

    #[test]
    fn a_snippet_clips_on_a_word_boundary_when_it_can() {
        let value = format!("{} tail", "word ".repeat(200));
        let clipped = snippet(&value);
        assert!(clipped.ends_with('…'));
        assert!(
            !clipped.contains("wor…"),
            "clipping should not split a word"
        );
    }

    #[test]
    fn a_short_value_is_returned_untouched() {
        assert_eq!(snippet("short and sweet"), "short and sweet");
    }

    #[test]
    fn the_schema_advertises_the_documented_parameters() {
        let schema = schemars::schema_for!(RememberArgs);
        let json = serde_json::to_value(&schema).unwrap();
        let properties = &json["properties"];
        assert!(properties.get("key").is_some());
        assert!(properties.get("value").is_some());
        assert!(properties.get("tags").is_some());
        // `tags` is optional, so it must not appear in `required`.
        let required = json["required"]
            .as_array()
            .map(|list| list.len())
            .unwrap_or(0);
        assert_eq!(required, 2, "only key and value are required");
    }
}
