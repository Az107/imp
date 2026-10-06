//! Cron jobs, run records, and the persistence boundary the scheduler writes through.
//!
//! The tables live in `minion-store` (SDD §5.8) and were created in the baseline
//! migration with nothing writing to them; M4 is what finally does. The types
//! and the trait live here, next to [`crate::policy::ApprovalStore`], so the
//! scheduler in `minion-cron` depends on `minion-core` alone and can be driven
//! against an in-memory fake while a real SQLite store is exercised separately.

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Render an instant the one way this crate ever writes or compares one.
///
/// Fixed-width, millisecond precision, `Z` suffix: SQLite compares these as
/// text, and only a single canonical format makes `next_run_at <= now` a
/// correct comparison rather than a lexicographic guess.
pub fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Parse a value written by [`stamp`], tolerating anything else RFC 3339.
pub fn parse_stamp(raw: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|err| Error::Config(format!("`{raw}` is not a timestamp: {err}")))
}

/// How a job's prompt reaches a conversation (SDD §5.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    /// A fresh session per run. The default: one runaway run cannot poison the next.
    #[default]
    New,
    /// Append to the job's own session, so the job accumulates context.
    Reuse,
}

impl SessionMode {
    /// Stable lowercase name, matching the `jobs.session_mode` column.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionMode::New => "new",
            SessionMode::Reuse => "reuse",
        }
    }

    /// Read a session mode from user or model input.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "new" => Ok(SessionMode::New),
            "reuse" => Ok(SessionMode::Reuse),
            other => Err(Error::Config(format!(
                "session mode must be `new` or `reuse`, was `{other}`"
            ))),
        }
    }
}

/// What happened to one run. Matches the enum SDD §5.8 leaves in the schema comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// Accepted but waiting for a free concurrency slot.
    Queued,
    /// Dispatched and in flight.
    Running,
    /// Finished successfully.
    Ok,
    /// Finished with a provider or tool failure.
    Failed,
    /// A missed occurrence the catch-up policy chose not to run.
    Skipped,
    /// The previous run was still active when this occurrence came due.
    Overlap,
}

impl JobStatus {
    /// Stable lowercase name, matching `job_runs.status`.
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Ok => "ok",
            JobStatus::Failed => "failed",
            JobStatus::Skipped => "skipped",
            JobStatus::Overlap => "overlap",
        }
    }

    /// Read a status back out of the database.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "queued" => Ok(JobStatus::Queued),
            "running" => Ok(JobStatus::Running),
            "ok" => Ok(JobStatus::Ok),
            "failed" => Ok(JobStatus::Failed),
            "skipped" => Ok(JobStatus::Skipped),
            "overlap" => Ok(JobStatus::Overlap),
            other => Err(Error::Store(format!("unknown run status `{other}`"))),
        }
    }

    /// Whether no further transition is expected.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatus::Ok | JobStatus::Failed | JobStatus::Skipped | JobStatus::Overlap
        )
    }
}

/// A scheduled prompt as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// uuid v7.
    pub id: String,
    /// Human label, unique when present.
    pub name: Option<String>,
    /// Five-field cron expression.
    pub schedule: String,
    /// IANA timezone the expression is read in.
    pub timezone: String,
    /// The prompt dispatched on each fire.
    pub prompt: String,
    /// Workspace the run executes in.
    pub cwd: String,
    /// The session a `reuse` job appends to.
    pub session_id: Option<String>,
    /// `new` or `reuse`.
    pub session_mode: SessionMode,
    /// Whether the scheduler may fire it.
    pub enabled: bool,
    /// Whether a fire may start while the previous run is still active.
    pub allow_overlap: bool,
    /// Stop after this many runs, when set.
    pub max_runs: Option<i64>,
    /// How many runs have been started.
    pub runs_count: i64,
    /// RFC 3339, when the job was created.
    pub created_at: String,
    /// RFC 3339, when the most recent run started.
    pub last_run_at: Option<String>,
    /// RFC 3339, the next occurrence. `None` for a job that never fires.
    pub next_run_at: Option<String>,
    /// The most recent run's status.
    pub last_status: Option<String>,
}

impl Job {
    /// The label to print and to tag a synthetic message with.
    pub fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.id.chars().take(8).collect())
    }
}

/// Values needed to create a job.
#[derive(Debug, Clone)]
pub struct NewJob {
    /// Pre-generated id, so the caller can report it if the write fails.
    pub id: String,
    /// Optional unique label.
    pub name: Option<String>,
    /// Five-field cron expression, already validated.
    pub schedule: String,
    /// IANA timezone, already validated.
    pub timezone: String,
    /// Prompt dispatched on each fire.
    pub prompt: String,
    /// Workspace for the run.
    pub cwd: String,
    /// Session behaviour.
    pub session_mode: SessionMode,
    /// Whether overlapping runs are allowed.
    pub allow_overlap: bool,
    /// Optional run budget.
    pub max_runs: Option<i64>,
    /// First occurrence, computed before the insert.
    pub next_run_at: Option<String>,
    /// Creation time.
    pub created_at: String,
}

/// A run row about to be inserted. Written before dispatch (NFR-6).
#[derive(Debug, Clone)]
pub struct NewJobRun {
    /// Run id.
    pub id: String,
    /// Owning job.
    pub job_id: String,
    /// Session the run uses, once known.
    pub session_id: Option<String>,
    /// RFC 3339.
    pub started_at: String,
    /// `running` or `queued`.
    pub status: JobStatus,
    /// Human line describing the outcome, when known at insert time.
    pub exit_summary: Option<String>,
}

/// The fields a fire or a completion changes on the job row.
///
/// Applied in the same transaction as the run row, which is what §5.7 asks for:
/// a run and the job's new `next_run_at` either both land or neither does.
#[derive(Debug, Clone, Default)]
pub struct JobUpdate {
    /// New next occurrence, when it changed.
    pub next_run_at: Option<String>,
    /// Run start instant to record.
    pub last_run_at: Option<String>,
    /// Status to record on the job.
    pub last_status: Option<String>,
    /// Enable or disable the job.
    pub enabled: Option<bool>,
    /// Adopt a session for a `reuse` job that did not have one yet.
    pub session_id: Option<String>,
    /// Increment `runs_count`.
    pub increment_runs: bool,
}

/// A run as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRun {
    /// Run id.
    pub id: String,
    /// Owning job.
    pub job_id: String,
    /// Session the run used.
    pub session_id: Option<String>,
    /// RFC 3339.
    pub started_at: String,
    /// RFC 3339, absent while the run is in flight.
    pub finished_at: Option<String>,
    /// Current status.
    pub status: JobStatus,
    /// One-line outcome.
    pub exit_summary: Option<String>,
    /// Where the run's output lives.
    pub output_ref: Option<String>,
}

/// Persistence the scheduler and the cron tools need.
///
/// Implemented by `minion-store` over the `jobs` and `job_runs` tables. Kept
/// deliberately narrow: everything the scheduler decides is decided in
/// `minion-cron`, and the store only records it.
#[async_trait]
pub trait JobStore: Send + Sync {
    /// Insert a job.
    async fn create_job(&self, new: NewJob) -> Result<Job>;

    /// Fetch a job by id, or by name.
    async fn job(&self, key: &str) -> Result<Option<Job>>;

    /// Every job, oldest first.
    async fn list_jobs(&self) -> Result<Vec<Job>>;

    /// Delete a job by id or name; its runs cascade away.
    async fn remove_job(&self, key: &str) -> Result<bool>;

    /// Enabled jobs whose `next_run_at` is at or before `now`, earliest first.
    async fn due_jobs(&self, now: DateTime<Utc>) -> Result<Vec<Job>>;

    /// Insert a run and apply the matching job update, in one transaction.
    async fn start_run(&self, run: NewJobRun, update: JobUpdate) -> Result<()>;

    /// Write a run's terminal status and the job update, in one transaction.
    async fn finish_run(
        &self,
        run_id: &str,
        status: JobStatus,
        finished_at: DateTime<Utc>,
        exit_summary: Option<String>,
        output_ref: Option<String>,
        update: JobUpdate,
    ) -> Result<()>;

    /// Apply a job update with no run row, for changes that are not a run.
    async fn update_job(&self, job_id: &str, update: JobUpdate) -> Result<()>;

    /// Promote a `queued` run to `running` when a concurrency slot frees.
    async fn mark_run_running(&self, run_id: &str) -> Result<()>;

    /// A job's runs, newest first.
    async fn list_runs(&self, job_id: &str, limit: usize) -> Result<Vec<JobRun>>;

    /// Mark runs left non-terminal by a previous process as failed.
    ///
    /// Without this a crash mid-run leaves a row claiming to be `running`
    /// forever, and `/cron` would report a job as active that nothing is
    /// running. Returns how many rows were closed.
    async fn abandon_open_runs(&self, now: DateTime<Utc>) -> Result<usize>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_round_trip_through_their_names() {
        for status in [
            JobStatus::Queued,
            JobStatus::Running,
            JobStatus::Ok,
            JobStatus::Failed,
            JobStatus::Skipped,
            JobStatus::Overlap,
        ] {
            assert_eq!(JobStatus::parse(status.as_str()).unwrap(), status);
        }
        assert!(JobStatus::parse("melted").is_err());
    }

    #[test]
    fn only_the_finished_states_are_terminal() {
        assert!(JobStatus::Ok.is_terminal());
        assert!(JobStatus::Failed.is_terminal());
        assert!(JobStatus::Skipped.is_terminal());
        assert!(JobStatus::Overlap.is_terminal());
        assert!(!JobStatus::Running.is_terminal());
        assert!(!JobStatus::Queued.is_terminal());
    }

    #[test]
    fn session_modes_round_trip_and_reject_nonsense() {
        assert_eq!(SessionMode::parse("reuse").unwrap(), SessionMode::Reuse);
        assert_eq!(SessionMode::parse(" NEW ").unwrap(), SessionMode::New);
        assert!(SessionMode::parse("shared").is_err());
        assert_eq!(SessionMode::default(), SessionMode::New);
    }

    /// The fixed-width stamp is load-bearing: SQLite compares these as text, so
    /// two instants must order the same way as strings and as times.
    #[test]
    fn timestamps_are_fixed_width_and_sort_as_text() {
        let base = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let earlier = stamp(base);
        let later = stamp(base + chrono::Duration::milliseconds(1));
        let much_later = stamp(base + chrono::Duration::days(30));

        assert_eq!(earlier.len(), later.len());
        assert_eq!(earlier.len(), much_later.len());
        assert!(earlier < later && later < much_later);
        assert_eq!(parse_stamp(&earlier).unwrap(), base);
        assert!(parse_stamp("yesterday").is_err());
    }
}
