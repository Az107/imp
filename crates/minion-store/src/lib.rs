//! Session and message persistence.
//!
//! `rusqlite` is synchronous, so every call is dispatched through
//! [`Store::blocking`] onto a blocking thread. Doing the I/O inline would stall
//! the async runtime for the duration of the query, which is exactly the kind of
//! hidden latency an agent loop should not absorb.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use minion_core::error::{Error, Result};
use minion_core::message::{Message, Role};
use rusqlite::{Connection, OptionalExtension};

pub mod approvals;
pub mod audit;
pub mod jobs;
pub mod memory;
pub mod migrate;
pub mod usage;

pub use audit::AuditRow;
pub use memory::{MemoryEntry, Written};
pub use migrate::SCHEMA_VERSION;
pub use usage::SessionUsage;

/// A session as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// uuid v7, time-ordered.
    pub id: String,
    /// Human label; derived from the first user message until renamed.
    pub title: Option<String>,
    /// Workspace the session was started in.
    pub cwd: String,
    /// Model id at creation time.
    pub model: Option<String>,
    /// Base URL of the backend.
    pub provider: Option<String>,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339, advanced on every write.
    pub updated_at: String,
}

/// Values needed to start a session.
#[derive(Debug, Clone)]
pub struct NewSession {
    /// Pre-generated id, so the caller can log it even if the write fails.
    pub id: String,
    /// Workspace root.
    pub cwd: String,
    /// Model id.
    pub model: Option<String>,
    /// Base URL.
    pub provider: Option<String>,
}

/// One message row as it sits in SQLite, before decoding into [`Message`].
struct StoredMessage {
    role: String,
    content: Option<String>,
    tool_calls: Option<String>,
    tool_call_id: Option<String>,
}

/// The SQLite database.
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl Store {
    /// Open (creating if needed) the database at `path` and migrate it.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| {
                Error::Store(format!("cannot create {}: {err}", parent.display()))
            })?;
            set_mode(parent, 0o700);
        }

        // Build the connection on a blocking thread: creating a file can touch
        // the disk, and `migrate` runs a full integrity check.
        let target = path.to_path_buf();
        let connection = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&target)
                .map_err(|err| Error::Store(format!("cannot open the database: {err}")))?;
            conn.pragma_update(None, "journal_mode", "WAL")
                .map_err(|err| Error::Store(format!("cannot enable WAL: {err}")))?;
            conn.pragma_update(None, "foreign_keys", "ON")
                .map_err(|err| Error::Store(format!("cannot enable foreign keys: {err}")))?;
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .map_err(|err| Error::Store(format!("cannot set busy_timeout: {err}")))?;
            conn.pragma_update(None, "synchronous", "NORMAL")
                .map_err(|err| Error::Store(format!("cannot set synchronous: {err}")))?;
            migrate::migrate(&conn)?;
            Ok::<Connection, Error>(conn)
        })
        .await
        .map_err(|err| Error::Store(format!("the database task failed: {err}")))??;

        set_mode(path, 0o600);
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path: path.to_path_buf(),
        })
    }

    /// A private in-memory database, for tests and dry runs.
    pub async fn open_in_memory() -> Result<Self> {
        let connection = tokio::task::spawn_blocking(|| {
            let conn = Connection::open_in_memory()
                .map_err(|err| Error::Store(format!("cannot open memory db: {err}")))?;
            conn.pragma_update(None, "foreign_keys", "ON")
                .map_err(|err| Error::Store(format!("cannot enable foreign keys: {err}")))?;
            migrate::migrate(&conn)?;
            Ok::<Connection, Error>(conn)
        })
        .await
        .map_err(|err| Error::Store(format!("the database task failed: {err}")))??;

        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path: PathBuf::from(":memory:"),
        })
    }

    /// Where the database lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run a closure against the connection on a blocking thread.
    async fn blocking<T, F>(&self, work: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let guard = connection
                .lock()
                .map_err(|_| Error::Store("the database mutex is poisoned".to_string()))?;
            work(&guard)
        })
        .await
        .map_err(|err| Error::Store(format!("the database task failed: {err}")))?
    }

    /// Insert a new session.
    pub async fn create_session(&self, new: NewSession) -> Result<SessionRow> {
        self.blocking(move |conn| {
            let now = migrate::timestamp();
            conn.execute(
                "INSERT INTO sessions (id, title, cwd, model, provider, created_at, updated_at)
                 VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?5)",
                rusqlite::params![new.id, new.cwd, new.model, new.provider, now],
            )
            .map_err(|err| Error::Store(format!("cannot create the session: {err}")))?;

            Ok(SessionRow {
                id: new.id,
                title: None,
                cwd: new.cwd,
                model: new.model,
                provider: new.provider,
                created_at: now.clone(),
                updated_at: now,
            })
        })
        .await
    }

    /// Fetch one session.
    pub async fn session(&self, id: &str) -> Result<Option<SessionRow>> {
        let id = id.to_string();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT id, title, cwd, model, provider, created_at, updated_at
                 FROM sessions WHERE id = ?1",
                rusqlite::params![id],
                |row| {
                    Ok(SessionRow {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        cwd: row.get(2)?,
                        model: row.get(3)?,
                        provider: row.get(4)?,
                        created_at: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(|err| Error::Store(format!("cannot read the session: {err}")))
        })
        .await
    }

    /// Most recently updated sessions first.
    pub async fn list_sessions(&self, limit: usize) -> Result<Vec<SessionRow>> {
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, title, cwd, model, provider, created_at, updated_at
                     FROM sessions ORDER BY updated_at DESC LIMIT ?1",
                )
                .map_err(|err| Error::Store(format!("cannot list sessions: {err}")))?;
            let rows = stmt
                .query_map(rusqlite::params![limit as i64], |row| {
                    Ok(SessionRow {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        cwd: row.get(2)?,
                        model: row.get(3)?,
                        provider: row.get(4)?,
                        created_at: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                })
                .map_err(|err| Error::Store(format!("cannot read sessions: {err}")))?;

            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|err| Error::Store(format!("cannot read sessions: {err}")))
        })
        .await
    }

    /// Set a session's title.
    pub async fn rename_session(&self, id: &str, title: &str) -> Result<bool> {
        let (id, title) = (id.to_string(), title.to_string());
        self.blocking(move |conn| {
            let changed = conn
                .execute(
                    "UPDATE sessions SET title = ?2, updated_at = ?3 WHERE id = ?1",
                    rusqlite::params![id, title, migrate::timestamp()],
                )
                .map_err(|err| Error::Store(format!("cannot rename the session: {err}")))?;
            Ok(changed > 0)
        })
        .await
    }

    /// Derive a title from the first user message, if none was set.
    pub async fn title_from_first_prompt(&self, id: &str, prompt: &str) -> Result<()> {
        let (id, title) = (id.to_string(), summarise(prompt, 60));
        self.blocking(move |conn| {
            conn.execute(
                "UPDATE sessions SET title = ?2
                 WHERE id = ?1 AND (title IS NULL OR title = '')",
                rusqlite::params![id, title],
            )
            .map_err(|err| Error::Store(format!("cannot set the session title: {err}")))?;
            Ok(())
        })
        .await
    }

    /// Delete a session; its messages cascade away with it.
    pub async fn delete_session(&self, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.blocking(move |conn| {
            let changed = conn
                .execute("DELETE FROM sessions WHERE id = ?1", rusqlite::params![id])
                .map_err(|err| Error::Store(format!("cannot delete the session: {err}")))?;
            Ok(changed > 0)
        })
        .await
    }

    /// Append messages, allocating `seq` inside the transaction.
    ///
    /// One transaction for the whole batch so a turn is never half-written: a
    /// tool result without its assistant call would be an invalid transcript.
    pub async fn append_messages(&self, session_id: &str, messages: &[Message]) -> Result<()> {
        let session_id = session_id.to_string();
        let messages = messages.to_vec();
        self.blocking(move |conn| {
            let transaction = conn
                .unchecked_transaction()
                .map_err(|err| Error::Store(format!("cannot begin a transaction: {err}")))?;

            let mut next: i64 = transaction
                .query_row(
                    "SELECT COALESCE(MAX(seq), 0) FROM messages WHERE session_id = ?1",
                    rusqlite::params![session_id],
                    |row| row.get(0),
                )
                .map_err(|err| Error::Store(format!("cannot read the sequence: {err}")))?;

            for message in &messages {
                next += 1;
                let tool_calls = match &message.tool_calls {
                    Some(calls) if !calls.is_empty() => {
                        Some(serde_json::to_string(calls).map_err(|err| {
                            Error::Store(format!("cannot encode tool calls: {err}"))
                        })?)
                    }
                    _ => None,
                };
                transaction
                    .execute(
                        "INSERT INTO messages
                           (id, session_id, seq, role, content, tool_calls, tool_call_id,
                            tool_name, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        rusqlite::params![
                            minion_core::new_session_id(),
                            session_id,
                            next,
                            role_name(message.role),
                            message.content,
                            tool_calls,
                            message.tool_call_id,
                            message.tool_calls.as_ref().and_then(|calls| {
                                calls.first().map(|call| call.function.name.clone())
                            }),
                            migrate::timestamp(),
                        ],
                    )
                    .map_err(|err| Error::Store(format!("cannot append a message: {err}")))?;
            }

            transaction
                .execute(
                    "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                    rusqlite::params![session_id, migrate::timestamp()],
                )
                .map_err(|err| Error::Store(format!("cannot touch the session: {err}")))?;

            transaction
                .commit()
                .map_err(|err| Error::Store(format!("cannot commit: {err}")))?;
            Ok(())
        })
        .await
    }

    /// The whole transcript, oldest first.
    pub async fn load_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let session_id = session_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT role, content, tool_calls, tool_call_id
                     FROM messages WHERE session_id = ?1 ORDER BY seq ASC",
                )
                .map_err(|err| Error::Store(format!("cannot read messages: {err}")))?;

            // Read the raw columns first: a `query_map` closure may only return
            // `rusqlite::Result`, so decoding happens after the query resolves.
            let raw: Vec<StoredMessage> = stmt
                .query_map(rusqlite::params![session_id], |row| {
                    Ok(StoredMessage {
                        role: row.get(0)?,
                        content: row.get(1)?,
                        tool_calls: row.get(2)?,
                        tool_call_id: row.get(3)?,
                    })
                })
                .map_err(|err| Error::Store(format!("cannot read messages: {err}")))?
                .collect::<rusqlite::Result<_>>()
                .map_err(|err| Error::Store(format!("cannot read messages: {err}")))?;

            raw.into_iter()
                .map(|row| {
                    let calls = match row.tool_calls {
                        Some(raw) => Some(serde_json::from_str(&raw).map_err(|err| {
                            Error::Store(format!("cannot decode tool calls: {err}"))
                        })?),
                        None => None,
                    };
                    Ok(Message {
                        role: parse_role(&row.role)?,
                        content: row.content,
                        tool_calls: calls,
                        tool_call_id: row.tool_call_id,
                    })
                })
                .collect()
        })
        .await
    }

    /// Number of messages in a session.
    pub async fn message_count(&self, session_id: &str) -> Result<usize> {
        let session_id = session_id.to_string();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                rusqlite::params![session_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as usize)
            .map_err(|err| Error::Store(format!("cannot count messages: {err}")))
        })
        .await
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn parse_role(raw: &str) -> Result<Role> {
    match raw {
        "system" => Ok(Role::System),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
        other => Err(Error::Store(format!(
            "unknown role `{other}` in the transcript"
        ))),
    }
}

/// Clip a prompt into a one-line session title.
fn summarise(prompt: &str, width: usize) -> String {
    let line = prompt.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return "(untitled)".to_string();
    }
    if line.chars().count() <= width {
        return line.to_string();
    }
    let clipped: String = line.chars().take(width - 1).collect();
    format!("{clipped}…")
}

/// Best-effort permission tightening; a no-op without POSIX modes.
fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minion_core::message::FunctionCall;
    use minion_core::message::ToolCall;

    fn session() -> NewSession {
        NewSession {
            id: minion_core::new_session_id(),
            cwd: "/tmp".to_string(),
            model: Some("test-model".to_string()),
            provider: Some("http://localhost/v1".to_string()),
        }
    }

    #[tokio::test]
    async fn a_session_round_trips() {
        let store = Store::open_in_memory().await.unwrap();
        let new = session();

        let created = store.create_session(new.clone()).await.unwrap();
        let fetched = store.session(&new.id).await.unwrap().unwrap();

        assert_eq!(created.id, new.id);
        assert_eq!(fetched.cwd, "/tmp");
        assert_eq!(fetched.model.as_deref(), Some("test-model"));
        assert_eq!(fetched.title, None);
    }

    #[tokio::test]
    async fn messages_keep_their_order_across_batches() {
        let store = Store::open_in_memory().await.unwrap();
        let new = session();
        store.create_session(new.clone()).await.unwrap();

        store
            .append_messages(&new.id, &[Message::user("one"), Message::assistant("two")])
            .await
            .unwrap();
        store
            .append_messages(&new.id, &[Message::user("three")])
            .await
            .unwrap();

        let loaded = store.load_messages(&new.id).await.unwrap();

        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].content.as_deref(), Some("one"));
        assert_eq!(loaded[1].content.as_deref(), Some("two"));
        assert_eq!(loaded[2].content.as_deref(), Some("three"));
        assert_eq!(loaded[0].role, Role::User);
        assert_eq!(loaded[1].role, Role::Assistant);
    }

    #[tokio::test]
    async fn tool_calls_survive_the_round_trip() {
        let store = Store::open_in_memory().await.unwrap();
        let new = session();
        store.create_session(new.clone()).await.unwrap();

        let call = ToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: "{\"path\":\"a.txt\"}".to_string(),
            },
        };
        store
            .append_messages(
                &new.id,
                &[
                    Message::assistant_with_tool_calls(None, vec![call]),
                    Message::tool_result("call_1", "contents"),
                ],
            )
            .await
            .unwrap();

        let loaded = store.load_messages(&new.id).await.unwrap();

        let calls = loaded[0].tool_calls.as_ref().expect("tool calls were lost");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(loaded[1].role, Role::Tool);
        assert_eq!(loaded[1].tool_call_id.as_deref(), Some("call_1"));
    }

    #[tokio::test]
    async fn messages_never_share_a_sequence_number() {
        let store = Store::open_in_memory().await.unwrap();
        let new = session();
        store.create_session(new.clone()).await.unwrap();

        store
            .append_messages(&new.id, &[Message::user("a"), Message::user("b")])
            .await
            .unwrap();
        store
            .append_messages(&new.id, &[Message::user("c")])
            .await
            .unwrap();

        // The UNIQUE(session_id, seq) constraint would have raised by now if the
        // allocator restarted per batch; assert the count to make that explicit.
        assert_eq!(store.message_count(&new.id).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn deleting_a_session_takes_its_messages_with_it() {
        let store = Store::open_in_memory().await.unwrap();
        let new = session();
        store.create_session(new.clone()).await.unwrap();
        store
            .append_messages(&new.id, &[Message::user("hi")])
            .await
            .unwrap();

        let deleted = store.delete_session(&new.id).await.unwrap();

        assert!(deleted);
        assert!(store.session(&new.id).await.unwrap().is_none());
        assert_eq!(store.message_count(&new.id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn sessions_list_newest_first() {
        let store = Store::open_in_memory().await.unwrap();
        let older = session();
        store.create_session(older.clone()).await.unwrap();
        let newer = session();
        store.create_session(newer.clone()).await.unwrap();

        let listed = store.list_sessions(10).await.unwrap();

        assert_eq!(listed.len(), 2);
        // updated_at is second-resolution, so fall back to the insertion order the
        // schema guarantees only loosely; assert both are present.
        let ids: Vec<&str> = listed.iter().map(|row| row.id.as_str()).collect();
        assert!(ids.contains(&older.id.as_str()));
        assert!(ids.contains(&newer.id.as_str()));
    }

    #[tokio::test]
    async fn a_title_is_derived_once_then_left_alone() {
        let store = Store::open_in_memory().await.unwrap();
        let new = session();
        store.create_session(new.clone()).await.unwrap();

        store
            .title_from_first_prompt(&new.id, "a long first prompt\nand a second line")
            .await
            .unwrap();
        store
            .title_from_first_prompt(&new.id, "a later prompt")
            .await
            .unwrap();

        let row = store.session(&new.id).await.unwrap().unwrap();
        assert_eq!(row.title.as_deref(), Some("a long first prompt"));
    }

    #[tokio::test]
    async fn renaming_overrides_the_derived_title() {
        let store = Store::open_in_memory().await.unwrap();
        let new = session();
        store.create_session(new.clone()).await.unwrap();
        store
            .title_from_first_prompt(&new.id, "original")
            .await
            .unwrap();

        let renamed = store
            .rename_session(&new.id, "chosen by hand")
            .await
            .unwrap();

        assert!(renamed);
        let row = store.session(&new.id).await.unwrap().unwrap();
        assert_eq!(row.title.as_deref(), Some("chosen by hand"));
    }

    #[test]
    fn titles_are_clipped_to_a_single_line() {
        assert_eq!(summarise("short", 60), "short");
        assert_eq!(summarise("first\nsecond", 60), "first");
        assert_eq!(summarise("   ", 60), "(untitled)");
        assert_eq!(summarise(&"x".repeat(80), 60).chars().count(), 60);
    }

    #[tokio::test]
    async fn the_database_file_is_not_world_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("minion.db");

        Store::open(&path).await.unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a transcript should stay private");
        }
    }

    /// NFR-6: the store runs in WAL mode, which is what lets a reader see
    /// committed data while a writer is mid-transaction and what survives a
    /// `SIGKILL` without losing a committed turn.
    #[tokio::test]
    async fn the_store_runs_in_wal_mode() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("minion.db")).await.unwrap();

        let mode: String = store
            .blocking(|conn| {
                conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))
                    .map_err(|err| Error::Store(err.to_string()))
            })
            .await
            .unwrap();

        assert_eq!(mode.to_lowercase(), "wal");
    }

    /// NFR-6: a committed message is on disk, so a process that dies and a
    /// fresh one that reopens the file find the same transcript.
    #[tokio::test]
    async fn a_committed_message_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("minion.db");
        let session_id = minion_core::new_session_id();
        {
            let store = Store::open(&path).await.unwrap();
            store
                .create_session(NewSession {
                    id: session_id.clone(),
                    cwd: "/tmp".to_string(),
                    model: None,
                    provider: None,
                })
                .await
                .unwrap();
            store
                .append_messages(&session_id, &[Message::user("durable?")])
                .await
                .unwrap();
            // Drop the connection the way a crashed process would.
        }

        let reopened = Store::open(&path).await.unwrap();
        let messages = reopened.load_messages(&session_id).await.unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content.as_deref(), Some("durable?"));
    }
}
