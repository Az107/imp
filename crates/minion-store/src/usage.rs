//! Token usage per conversation, so `/cost` aggregates a session rather than
//! only the process that happens to be running (§7).

use minion_core::error::{Error, Result};
use minion_core::provider::Usage;
use rusqlite::OptionalExtension;

use crate::{Store, migrate};

/// Accumulated token usage for one conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionUsage {
    /// Tokens sent in prompts.
    pub prompt_tokens: u64,
    /// Tokens generated.
    pub completion_tokens: u64,
    /// Provider-reported total.
    pub total_tokens: u64,
    /// How many turns contributed.
    pub turns: u64,
}

impl Store {
    /// Add one turn's usage to a conversation's running total.
    ///
    /// `usage` is advisory (§R8): a provider that omits it contributes zeros
    /// rather than failing the write, and the turn counter still advances.
    pub async fn record_usage(&self, session_id: &str, usage: Usage) -> Result<()> {
        let session_id = session_id.to_string();
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO session_usage
                   (session_id, prompt_tokens, completion_tokens, total_tokens, turns, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 1, ?5)
                 ON CONFLICT(session_id) DO UPDATE SET
                   prompt_tokens = prompt_tokens + excluded.prompt_tokens,
                   completion_tokens = completion_tokens + excluded.completion_tokens,
                   total_tokens = total_tokens + excluded.total_tokens,
                   turns = turns + 1,
                   updated_at = excluded.updated_at",
                rusqlite::params![
                    session_id,
                    usage.prompt_tokens as i64,
                    usage.completion_tokens as i64,
                    usage.total_tokens as i64,
                    migrate::timestamp(),
                ],
            )
            .map_err(|err| Error::Store(format!("cannot record usage: {err}")))?;
            Ok(())
        })
        .await
    }

    /// The running total for one conversation, or zeros when it has none.
    pub async fn session_usage(&self, session_id: &str) -> Result<SessionUsage> {
        let session_id = session_id.to_string();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT prompt_tokens, completion_tokens, total_tokens, turns
                 FROM session_usage WHERE session_id = ?1",
                rusqlite::params![session_id],
                |row| {
                    Ok(SessionUsage {
                        prompt_tokens: row.get::<_, i64>(0)? as u64,
                        completion_tokens: row.get::<_, i64>(1)? as u64,
                        total_tokens: row.get::<_, i64>(2)? as u64,
                        turns: row.get::<_, i64>(3)? as u64,
                    })
                },
            )
            .optional()
            .map(|row| row.unwrap_or_default())
            .map_err(|err| Error::Store(format!("cannot read usage: {err}")))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewSession;

    async fn store_with_session() -> (Store, String) {
        let store = Store::open_in_memory().await.unwrap();
        let id = minion_core::new_session_id();
        store
            .create_session(NewSession {
                id: id.clone(),
                cwd: "/tmp".to_string(),
                model: None,
                provider: None,
            })
            .await
            .unwrap();
        (store, id)
    }

    fn usage(prompt: u32, completion: u32, total: u32) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total,
        }
    }

    #[tokio::test]
    async fn usage_accumulates_across_turns() {
        let (store, id) = store_with_session().await;

        store.record_usage(&id, usage(10, 5, 15)).await.unwrap();
        store.record_usage(&id, usage(3, 2, 5)).await.unwrap();

        let total = store.session_usage(&id).await.unwrap();
        assert_eq!(total.prompt_tokens, 13);
        assert_eq!(total.completion_tokens, 7);
        assert_eq!(total.total_tokens, 20);
        assert_eq!(total.turns, 2);
    }

    #[tokio::test]
    async fn an_unknown_session_reports_zero() {
        let (store, _id) = store_with_session().await;

        let total = store.session_usage("nobody").await.unwrap();

        assert_eq!(total, SessionUsage::default());
    }
}
