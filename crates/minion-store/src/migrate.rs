//! Schema definition and forward-only migrations.
//!
//! The baseline matches SDD §5.8. Tables that later milestones own (`jobs`,
//! `job_runs`, `memory`, `audit_log`) are created here so the shape is fixed and
//! inspectable from day one, but nothing writes to them until M3/M4.

use rusqlite::Connection;

use minion_core::error::{Error, Result};

/// The schema version this build expects. A database newer than this is refused
/// rather than silently misread.
pub const SCHEMA_VERSION: i64 = 2;

/// Applied in order; never edited once released.
const MIGRATIONS: &[&str] = &[BASELINE, SESSION_USAGE];

/// Tables, indices, and the FTS triggers for `memory`.
const BASELINE: &str = r#"
CREATE TABLE sessions (
  id          TEXT PRIMARY KEY,          -- uuid v7
  title       TEXT,
  cwd         TEXT NOT NULL,
  model       TEXT,
  provider    TEXT,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  meta        TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE messages (
  id           TEXT PRIMARY KEY,
  session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq          INTEGER NOT NULL,
  role         TEXT NOT NULL CHECK (role IN ('system','user','assistant','tool')),
  content      TEXT,
  tool_calls   TEXT,                     -- JSON array, assistant only
  tool_call_id TEXT,                     -- tool role only
  tool_name    TEXT,
  is_summary   INTEGER NOT NULL DEFAULT 0,
  tokens_in    INTEGER,
  tokens_out   INTEGER,
  created_at   TEXT NOT NULL,
  UNIQUE (session_id, seq)
);
CREATE INDEX idx_messages_session ON messages(session_id, seq);

CREATE TABLE jobs (
  id               TEXT PRIMARY KEY,
  name             TEXT UNIQUE,
  schedule         TEXT NOT NULL,        -- 5-field cron
  timezone         TEXT NOT NULL,
  prompt           TEXT NOT NULL,
  cwd              TEXT NOT NULL,
  session_id       TEXT REFERENCES sessions(id) ON DELETE SET NULL,
  session_mode     TEXT NOT NULL DEFAULT 'new',
  enabled          INTEGER NOT NULL DEFAULT 1,
  allow_overlap    INTEGER NOT NULL DEFAULT 0,
  max_runs         INTEGER,
  runs_count       INTEGER NOT NULL DEFAULT 0,
  created_at       TEXT NOT NULL,
  last_run_at      TEXT,
  next_run_at      TEXT,
  last_status      TEXT
);
CREATE INDEX idx_jobs_due ON jobs(enabled, next_run_at);

CREATE TABLE job_runs (
  id           TEXT PRIMARY KEY,
  job_id       TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  session_id   TEXT REFERENCES sessions(id) ON DELETE SET NULL,
  started_at   TEXT NOT NULL,
  finished_at  TEXT,
  status       TEXT NOT NULL,            -- queued|running|ok|failed|skipped|overlap
  exit_summary TEXT,
  output_ref   TEXT
);
CREATE INDEX idx_job_runs_job ON job_runs(job_id, started_at DESC);

CREATE TABLE approvals (
  id          TEXT PRIMARY KEY,
  tool        TEXT NOT NULL,
  pattern     TEXT NOT NULL,
  scope       TEXT NOT NULL,             -- workspace path
  decision    TEXT NOT NULL CHECK (decision IN ('allow','deny')),
  created_at  TEXT NOT NULL,
  expires_at  TEXT,
  UNIQUE (tool, pattern, scope, decision)
);

CREATE TABLE memory (
  id         TEXT PRIMARY KEY,
  namespace  TEXT NOT NULL,              -- workspace hash
  key        TEXT NOT NULL,
  value      TEXT NOT NULL,
  tags       TEXT NOT NULL DEFAULT '[]',
  session_id TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (namespace, key)
);

-- External-content FTS: the triggers keep the index in step with `memory`.
-- Without them `recall` would silently match nothing in M3.
CREATE VIRTUAL TABLE memory_fts USING fts5(
  key, value, tags, content='memory', content_rowid='rowid'
);
CREATE TRIGGER memory_ai AFTER INSERT ON memory BEGIN
  INSERT INTO memory_fts(rowid, key, value, tags)
  VALUES (new.rowid, new.key, new.value, new.tags);
END;
CREATE TRIGGER memory_ad AFTER DELETE ON memory BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, key, value, tags)
  VALUES ('delete', old.rowid, old.key, old.value, old.tags);
END;
CREATE TRIGGER memory_au AFTER UPDATE ON memory BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, key, value, tags)
  VALUES ('delete', old.rowid, old.key, old.value, old.tags);
  INSERT INTO memory_fts(rowid, key, value, tags)
  VALUES (new.rowid, new.key, new.value, new.tags);
END;

CREATE TABLE audit_log (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts          TEXT NOT NULL,
  session_id  TEXT,
  turn_id     TEXT,
  tool        TEXT,
  risk        TEXT,
  decision    TEXT,                      -- auto|allow_once|allow_session|allow_always|deny
  args_digest TEXT,                      -- hash, not raw args, for commands
  outcome     TEXT,
  duration_ms INTEGER
);
CREATE INDEX idx_audit_session ON audit_log(session_id, ts);
"#;

/// v2: token usage per conversation, so `/cost` can aggregate a session rather
/// than only the process that is currently running (§7). Added in M7.
const SESSION_USAGE: &str = r#"
CREATE TABLE session_usage (
  session_id        TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  prompt_tokens     INTEGER NOT NULL DEFAULT 0,
  completion_tokens INTEGER NOT NULL DEFAULT 0,
  total_tokens      INTEGER NOT NULL DEFAULT 0,
  turns             INTEGER NOT NULL DEFAULT 0,
  updated_at        TEXT NOT NULL
);
"#;

/// Bring `conn` up to [`SCHEMA_VERSION`], creating the database if needed.
///
/// Fails loudly on an unreadable file rather than starting with a half-applied
/// schema, which is the failure mode a silent "create if missing" would hide.
pub fn migrate(conn: &Connection) -> Result<()> {
    let integrity: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|err| Error::Store(format!("integrity check failed: {err}")))?;
    if integrity != "ok" {
        return Err(Error::Store(format!(
            "the database failed its integrity check: {integrity}"
        )));
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
           version    INTEGER PRIMARY KEY,
           applied_at TEXT NOT NULL
         )",
    )
    .map_err(|err| Error::Store(format!("cannot create the migration table: {err}")))?;

    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .map_err(|err| Error::Store(format!("cannot read the schema version: {err}")))?;

    if current > SCHEMA_VERSION {
        return Err(Error::Store(format!(
            "the database is at schema {current}, but this build only understands {SCHEMA_VERSION}. \
             Upgrade minion, or point --db at a different file."
        )));
    }

    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if version <= current {
            continue;
        }
        // Run *this* migration's SQL, not the baseline: once a second migration
        // exists, always applying the first would never advance the schema.
        conn.execute_batch(sql)
            .map_err(|err| Error::Store(format!("migration {version} failed: {err}")))?;
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![version, timestamp()],
        )
        .map_err(|err| Error::Store(format!("cannot record migration {version}: {err}")))?;
    }

    Ok(())
}

/// UTC timestamp in RFC 3339, matching what the CLI prints.
pub fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_database_reaches_the_current_version() {
        let conn = Connection::open_in_memory().unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let conn = Connection::open_in_memory().unwrap();

        migrate(&conn).unwrap();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        migrate(&conn).unwrap();
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();

        assert_eq!(before, after, "migrations must be idempotent");
    }

    /// An existing v1 database is brought up to v2 in place: the M7 migration
    /// adds `session_usage` without disturbing what was already there.
    #[test]
    fn a_v1_database_gains_the_usage_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
        )
        .unwrap();
        conn.execute_batch(BASELINE).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (1, '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        let has_usage: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'session_usage'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, 2);
        assert_eq!(has_usage, 1);
    }

    #[test]
    fn a_newer_database_is_refused_rather_than_misread() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![SCHEMA_VERSION + 5, timestamp()],
        )
        .unwrap();

        let err = migrate(&conn).unwrap_err();

        assert!(
            err.to_string().contains("Upgrade minion"),
            "message was: {err}"
        );
    }

    #[test]
    fn the_baseline_defines_every_table_the_sdd_lists() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        for table in [
            "sessions",
            "messages",
            "jobs",
            "job_runs",
            "approvals",
            "memory",
            "memory_fts",
            "audit_log",
            "session_usage",
        ] {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
                    rusqlite::params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "{table} is missing from the schema");
        }
    }
}
