//! Reading the audit trail.
//!
//! Writing is `ApprovalStore::audit` (see [`crate::approvals`]); this is the
//! read side, so `imp doctor` and tests can see what the gate recorded.

use imp_core::error::{Error, Result};

use crate::Store;

/// One row of `audit_log`, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    /// RFC 3339.
    pub ts: String,
    /// Conversation the decision belonged to.
    pub session_id: Option<String>,
    /// Provider round-trip the decision belonged to.
    pub turn_id: Option<String>,
    /// Tool name.
    pub tool: Option<String>,
    /// Risk class.
    pub risk: Option<String>,
    /// `allow`, `deny`, or a guard verdict.
    pub decision: Option<String>,
    /// What the rules saw.
    pub subject: Option<String>,
    /// `ok`, `error`, `denied`, or `None` for a row the gate never finished.
    pub outcome: Option<String>,
    /// Wall-clock milliseconds the tool spent.
    pub duration_ms: Option<i64>,
}

impl Store {
    /// The most recent audit rows, newest first.
    pub async fn recent_audit(&self, limit: usize) -> Result<Vec<AuditRow>> {
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT ts, session_id, turn_id, tool, risk, decision, args_digest, outcome,
                            duration_ms
                     FROM audit_log ORDER BY id DESC LIMIT ?1",
                )
                .map_err(|err| Error::Store(format!("cannot read the audit log: {err}")))?;
            let rows = stmt
                .query_map(rusqlite::params![limit as i64], |row| {
                    Ok(AuditRow {
                        ts: row.get(0)?,
                        session_id: row.get(1)?,
                        turn_id: row.get(2)?,
                        tool: row.get(3)?,
                        risk: row.get(4)?,
                        decision: row.get(5)?,
                        subject: row.get(6)?,
                        outcome: row.get(7)?,
                        duration_ms: row.get(8)?,
                    })
                })
                .map_err(|err| Error::Store(format!("cannot read the audit log: {err}")))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|err| Error::Store(format!("cannot read the audit log: {err}")))
        })
        .await
    }
}
