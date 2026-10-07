//! Persisting approval decisions.

use minion_core::error::{Error, Result};
use minion_core::policy::{ApprovalStore, AuditEntry};
use minion_core::session::new_session_id;
use rusqlite::OptionalExtension;

use crate::{Store, migrate};

impl Store {
    /// Store implementation of [`ApprovalStore`], for the policy engine.
    pub fn approvals(self: &std::sync::Arc<Self>) -> std::sync::Arc<dyn ApprovalStore> {
        std::sync::Arc::new(StoreApprovals(self.clone()))
    }
}

/// The store's side of the approval boundary.
struct StoreApprovals(std::sync::Arc<Store>);

#[async_trait::async_trait]
impl ApprovalStore for StoreApprovals {
    async fn is_allowed(&self, tool: &str, pattern: &str, scope: &str) -> Result<bool> {
        let (tool, pattern, scope) = (tool.to_string(), pattern.to_string(), scope.to_string());
        self.0
            .blocking(move |conn| {
                conn.query_row(
                    "SELECT 1 FROM approvals
                     WHERE tool = ?1 AND pattern = ?2 AND scope = ?3 AND decision = 'allow'",
                    rusqlite::params![tool, pattern, scope],
                    |_| Ok(()),
                )
                .optional()
                .map(|found| found.is_some())
                .map_err(|err| Error::Store(format!("cannot read approvals: {err}")))
            })
            .await
    }

    async fn remember_allow(&self, tool: &str, pattern: &str, scope: &str) -> Result<()> {
        let (tool, pattern, scope) = (tool.to_string(), pattern.to_string(), scope.to_string());
        self.0
            .blocking(move |conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO approvals (id, tool, pattern, scope, decision, created_at)
                     VALUES (?1, ?2, ?3, ?4, 'allow', ?5)",
                    rusqlite::params![new_session_id(), tool, pattern, scope, migrate::timestamp()],
                )
                .map_err(|err| Error::Store(format!("cannot store the approval: {err}")))?;
                Ok(())
            })
            .await
    }

    /// Best effort: an audit failure must never fail the turn it describes.
    async fn audit(&self, entry: AuditEntry) {
        let outcome = self
            .0
            .blocking(move |conn| {
                conn.execute(
                    "INSERT INTO audit_log
                       (ts, session_id, turn_id, tool, risk, decision, args_digest, outcome,
                        duration_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        migrate::timestamp(),
                        entry.session_id,
                        entry.turn_id,
                        entry.tool,
                        entry.risk,
                        entry.decision,
                        entry.subject,
                        entry.outcome,
                        entry.duration_ms.map(|ms| ms as i64),
                    ],
                )
                .map(|_| ())
                .map_err(|err| Error::Store(format!("cannot write the audit entry: {err}")))
            })
            .await;

        if let Err(err) = outcome {
            tracing::warn!(error = %err, "could not write the audit entry");
        }
    }
}
