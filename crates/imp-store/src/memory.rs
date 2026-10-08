//! Keyword memory: the `memory` table and the `memory_fts` index beside it.
//!
//! Memory is deliberately not a vector store (§2 non-goals). It is a keyed
//! upsert plus a full-text query, which is deterministic, inspectable with any
//! SQLite shell, and cheap enough to run on every recall.
//!
//! `recall` searches the FTS index and then filters by namespace. FTS5's
//! external-content index carries no `namespace` column, so the filter is a
//! join back to `memory` rather than part of the match expression. That costs a
//! little precision — matches from other workspaces are ranked and then
//! discarded — but it keeps the index definition to the three columns the SDD
//! specifies.

use imp_core::error::{Error, Result};

use crate::{Store, migrate};

/// A remembered fact, as `recall` returns it.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEntry {
    /// Lookup key, unique within a namespace.
    pub key: String,
    /// The stored text, untruncated. Callers cap it for display.
    pub value: String,
    /// Labels attached at write time.
    pub tags: Vec<String>,
    /// RFC 3339.
    pub updated_at: String,
    /// FTS5 `bm25` score. Lower is a better match; only meaningful per query.
    pub score: f64,
}

/// What a write to `memory` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Written {
    /// The key was new in this namespace.
    Created,
    /// The key already existed and its value and tags were replaced.
    Replaced,
}

impl Store {
    /// Insert or replace one fact, scoped to `namespace`.
    ///
    /// Upserts on `UNIQUE (namespace, key)`, so the same key is overwritten
    /// rather than duplicated, and `created_at` is preserved across the update
    /// while `updated_at` advances.
    pub async fn remember(
        &self,
        namespace: &str,
        key: &str,
        value: &str,
        tags: &[String],
        session_id: Option<&str>,
    ) -> Result<Written> {
        let (namespace, key, value, tags, session_id) = (
            namespace.to_string(),
            key.to_string(),
            value.to_string(),
            tags.to_vec(),
            session_id.map(str::to_string),
        );

        self.blocking(move |conn| {
            let encoded = serde_json::to_string(&tags)
                .map_err(|err| Error::Store(format!("cannot encode tags: {err}")))?;

            // The whole write is one transaction, and the existence check
            // rides inside it: a separate query would leave a window where a
            // concurrent writer flips the answer.
            let transaction = conn
                .unchecked_transaction()
                .map_err(|err| Error::Store(format!("cannot begin a transaction: {err}")))?;

            let existed: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM memory WHERE namespace = ?1 AND key = ?2)",
                    rusqlite::params![namespace, key],
                    |row| row.get(0),
                )
                .map_err(|err| Error::Store(format!("cannot read memory: {err}")))?;

            transaction
                .execute(
                    "INSERT INTO memory
                       (id, namespace, key, value, tags, session_id, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                     ON CONFLICT (namespace, key) DO UPDATE SET
                       value      = excluded.value,
                       tags       = excluded.tags,
                       session_id = excluded.session_id,
                       updated_at = excluded.updated_at",
                    rusqlite::params![
                        imp_core::new_session_id(),
                        namespace,
                        key,
                        value,
                        encoded,
                        session_id,
                        migrate::timestamp(),
                    ],
                )
                .map_err(|err| Error::Store(format!("cannot store memory: {err}")))?;

            // A `Transaction` rolls back when dropped, so the write has to be
            // committed explicitly. Without this the row silently vanishes and
            // `recall` matches nothing.
            transaction
                .commit()
                .map_err(|err| Error::Store(format!("cannot commit memory: {err}")))?;

            Ok(if existed {
                Written::Replaced
            } else {
                Written::Created
            })
        })
        .await
    }

    /// Full-text search over `key`, `value` and `tags`, best match first.
    ///
    /// A query that contains nothing searchable — only punctuation, say —
    /// returns an empty list rather than an error: a model asking a vague
    /// question should not see a tool failure for it.
    pub async fn recall(
        &self,
        namespace: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        let Some(terms) = search_terms(query) else {
            return Ok(Vec::new());
        };
        let namespace = namespace.to_string();

        self.blocking(move |conn| {
            // bm25 returns a more-negative score for a better match, so the
            // ascending sort is the relevance order. The `query_map` closure
            // stays inside `rusqlite::Result` and the tag JSON is decoded after
            // the rows are collected.
            let mut statement = conn
                .prepare(
                    "SELECT m.key, m.value, m.tags, m.updated_at, bm25(memory_fts) AS score
                     FROM memory_fts
                     JOIN memory m ON m.rowid = memory_fts.rowid
                     WHERE memory_fts MATCH ?1 AND m.namespace = ?2
                     ORDER BY score
                     LIMIT ?3",
                )
                .map_err(|err| Error::Store(format!("cannot prepare the recall query: {err}")))?;

            let rows = statement
                .query_map(rusqlite::params![terms, namespace, limit as i64], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, f64>(4)?,
                    ))
                })
                .map_err(|err| Error::Store(format!("cannot run the recall query: {err}")))?;

            let collected = rows
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|err| Error::Store(format!("cannot read matches: {err}")))?;

            collected
                .into_iter()
                .map(|(key, value, tags, updated_at, score)| {
                    let tags = serde_json::from_str(&tags).unwrap_or_default();
                    Ok(MemoryEntry {
                        key,
                        value,
                        tags,
                        updated_at,
                        score,
                    })
                })
                .collect()
        })
        .await
    }
}

/// Reduce free text to a valid, non-operator FTS5 match expression.
///
/// The model writes these, so they arrive with punctuation, quotes and the
/// occasional stray `*`. Passing that straight to `MATCH` makes FTS5 raise a
/// syntax error on input nobody ever saw as code, so each word is reduced to
/// its alphanumeric characters and wrapped in double quotes.
///
/// The quoting is load-bearing, not tidiness. FTS5 reads `OR`, `AND` and `NOT`
/// as operators when they stand alone, so an unquoted query like
/// `alpha OR beta` would silently *widen* from "both words" to "either word"
/// and hand back a row the caller never asked for. Inside a quoted phrase they
/// are just text. The phrases are then implicitly `AND`ed together, which is
/// the reading a recall query is meant to have.
///
/// Returns `None` when nothing searchable survives, so an empty or
/// punctuation-only query yields no matches instead of an error.
fn search_terms(query: &str) -> Option<String> {
    let phrases: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{term}\""))
        .collect();

    if phrases.is_empty() {
        None
    } else {
        Some(phrases.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> Store {
        Store::open_in_memory().await.expect("in-memory store")
    }

    fn tags(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_string()).collect()
    }

    #[tokio::test]
    async fn a_fact_can_be_written_and_found_again() {
        let store = store().await;
        store
            .remember(
                "ns",
                "deploy",
                "run cargo build --release",
                &tags(&["ops"]),
                None,
            )
            .await
            .unwrap();

        let hits = store.recall("ns", "deploy", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "deploy");
        assert_eq!(hits[0].value, "run cargo build --release");
        assert_eq!(hits[0].tags, tags(&["ops"]));
    }

    #[tokio::test]
    async fn a_remembered_value_is_searchable_before_the_key_is_known() {
        // The point of FTS over key/value/tags: the model recalls by content,
        // not by having remembered the exact key.
        let store = store().await;
        store
            .remember(
                "ns",
                "build",
                "the release profile uses thin lto",
                &[],
                None,
            )
            .await
            .unwrap();

        let hits = store.recall("ns", "release lto", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn tags_are_searchable() {
        let store = store().await;
        store
            .remember("ns", "a", "something", &tags(&["python", "lint"]), None)
            .await
            .unwrap();
        store
            .remember("ns", "b", "other", &tags(&["rust"]), None)
            .await
            .unwrap();

        let hits = store.recall("ns", "python", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "a");
    }

    #[tokio::test]
    async fn every_term_must_match() {
        let store = store().await;
        store
            .remember("ns", "a", "the sky is blue", &[], None)
            .await
            .unwrap();

        assert_eq!(store.recall("ns", "sky", 5).await.unwrap().len(), 1);
        assert!(
            store
                .recall("ns", "sky purple", 5)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_key_is_replaced_rather_than_duplicated() {
        let store = store().await;
        assert_eq!(
            store.remember("ns", "k", "first", &[], None).await.unwrap(),
            Written::Created
        );
        assert_eq!(
            store
                .remember("ns", "k", "second", &tags(&["v2"]), None)
                .await
                .unwrap(),
            Written::Replaced
        );

        assert_eq!(store.recall("ns", "first", 5).await.unwrap().len(), 0);
        let hits = store.recall("ns", "second", 5).await.unwrap();
        assert_eq!(hits.len(), 1, "the old row must not linger");
        assert_eq!(hits[0].tags, tags(&["v2"]));
    }

    #[tokio::test]
    async fn the_index_follows_an_update() {
        // The FTS table is external-content, so the update trigger is the only
        // thing keeping it in step. If it regressed, recall would keep matching
        // the value the row used to hold.
        let store = store().await;
        store
            .remember("ns", "k", "original", &[], None)
            .await
            .unwrap();
        store
            .remember("ns", "k", "replacement", &[], None)
            .await
            .unwrap();

        assert!(store.recall("ns", "original", 5).await.unwrap().is_empty());
        assert_eq!(store.recall("ns", "replacement", 5).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn memory_never_leaks_into_another_namespace() {
        let store = store().await;
        store
            .remember(
                "ns-one",
                "secret",
                "client credentials live here",
                &[],
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            store
                .recall("ns-one", "credentials", 5)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .recall("ns-two", "credentials", 5)
                .await
                .unwrap()
                .is_empty(),
            "another workspace must not see this"
        );
    }

    #[tokio::test]
    async fn the_same_key_can_exist_in_two_namespaces() {
        let store = store().await;
        store
            .remember("one", "k", "value one", &[], None)
            .await
            .unwrap();
        store
            .remember("two", "k", "value two", &[], None)
            .await
            .unwrap();

        assert_eq!(
            store.recall("one", "value", 5).await.unwrap()[0].value,
            "value one"
        );
        assert_eq!(
            store.recall("two", "value", 5).await.unwrap()[0].value,
            "value two"
        );
    }

    #[tokio::test]
    async fn the_limit_is_honoured_and_best_matches_come_first() {
        let store = store().await;
        for index in 0..5 {
            store
                .remember("ns", &format!("key{index}"), "shared token here", &[], None)
                .await
                .unwrap();
        }

        let hits = store.recall("ns", "shared", 2).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert!(
            hits[0].score <= hits[1].score,
            "bm25 sorts ascending, so the first hit is the better match"
        );
    }

    #[tokio::test]
    async fn a_punctuation_only_query_is_empty_rather_than_an_error() {
        let store = store().await;
        store.remember("ns", "k", "v", &[], None).await.unwrap();

        for query in ["", "   ", "!!!", "* \" ( )"] {
            assert!(
                store.recall("ns", query, 5).await.unwrap().is_empty(),
                "`{query}` should yield nothing, not fail"
            );
        }
    }

    #[tokio::test]
    async fn fts_operators_in_a_query_cannot_widen_the_search() {
        // Model text reaches `MATCH` unfiltered if nothing sanitises it, and
        // FTS5 would then read the bareword `OR` as an operator and hand back
        // rows the caller never asked for. Terms are quoted and ANDed instead.
        let store = store().await;
        store.remember("ns", "a", "alpha", &[], None).await.unwrap();
        store.remember("ns", "b", "beta", &[], None).await.unwrap();

        // Neither row contains all three terms, so nothing matches — whereas
        // an unquoted `alpha OR beta` would have returned both.
        let hits = store.recall("ns", "\"alpha OR beta*", 5).await.unwrap();
        assert!(
            hits.is_empty(),
            "`OR` must not widen the query to either row, got {:?}",
            hits.iter().map(|hit| &hit.key).collect::<Vec<_>>()
        );

        // The stray `*` is a prefix operator in FTS5; it is stripped rather
        // than raising a syntax error.
        assert_eq!(store.recall("ns", "alpha*", 5).await.unwrap().len(), 1);
    }

    #[test]
    fn search_terms_keeps_words_and_neutralises_operators() {
        assert_eq!(
            search_terms("todo conventions"),
            Some("\"todo\" \"conventions\"".into())
        );
        assert_eq!(search_terms("a-b_c"), Some("\"a\" \"b\" \"c\"".into()));
        assert_eq!(
            search_terms("  spaced   out  "),
            Some("\"spaced\" \"out\"".into())
        );
        // A bare operator word becomes a literal phrase, not a clause.
        assert_eq!(
            search_terms("alpha OR beta"),
            Some("\"alpha\" \"OR\" \"beta\"".into())
        );
        assert_eq!(search_terms("**"), None);
        assert_eq!(search_terms(""), None);
    }
}
