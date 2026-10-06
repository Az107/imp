//! Persisting cron jobs and their runs (SDD §5.7, §5.8).
//!
//! The `jobs` and `job_runs` tables have existed since the baseline migration
//! with nothing writing to them; this is what finally does. The decisions — when
//! a job is due, whether a run may start, what `next_run_at` becomes — belong to
//! `minion-cron`. This module only records them, and it records a fire as one
//! transaction: the run row and the job's new `next_run_at` either both land or
//! neither does, so a crash can never leave a job due with no trace of the run.

use std::sync::Arc;

use minion_core::error::{Error, Result};
use minion_core::job::{
    Job, JobRun, JobStatus, JobStore, JobUpdate, NewJob, NewJobRun, SessionMode, stamp,
};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row};

use crate::Store;

/// Turn a domain parse failure into a column error.
///
/// A status the schema's `CHECK` should have prevented is corruption, not a
/// default: silently reading it as `failed` would hide it.
fn unreadable(index: usize, name: &str, raw: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        Type::Text,
        Box::new(Error::Store(format!("unreadable {name} `{raw}`"))),
    )
}

impl Store {
    /// Store implementation of [`JobStore`], for the scheduler and the cron tools.
    pub fn jobs(self: &Arc<Self>) -> Arc<dyn JobStore> {
        Arc::new(StoreJobs(self.clone()))
    }
}

/// The store's side of the job boundary.
struct StoreJobs(Arc<Store>);

/// Columns every job query selects, in the order [`read_job`] expects.
const JOB_COLUMNS: &str = "id, name, schedule, timezone, prompt, cwd, session_id, session_mode, \
                            enabled, allow_overlap, max_runs, runs_count, created_at, last_run_at, \
                            next_run_at, last_status";

fn read_job(row: &Row<'_>) -> rusqlite::Result<Job> {
    let raw_mode: String = row.get(7)?;
    let session_mode =
        SessionMode::parse(&raw_mode).map_err(|_| unreadable(7, "session_mode", &raw_mode))?;
    Ok(Job {
        id: row.get(0)?,
        name: row.get(1)?,
        schedule: row.get(2)?,
        timezone: row.get(3)?,
        prompt: row.get(4)?,
        cwd: row.get(5)?,
        session_id: row.get(6)?,
        session_mode,
        enabled: row.get::<_, i64>(8)? != 0,
        allow_overlap: row.get::<_, i64>(9)? != 0,
        max_runs: row.get(10)?,
        runs_count: row.get(11)?,
        created_at: row.get(12)?,
        last_run_at: row.get(13)?,
        next_run_at: row.get(14)?,
        last_status: row.get(15)?,
    })
}

/// Apply a [`JobUpdate`] to `job_id` inside an open transaction.
fn apply_update(
    transaction: &rusqlite::Transaction<'_>,
    job_id: &str,
    update: &JobUpdate,
) -> Result<()> {
    if let Some(next) = &update.next_run_at {
        transaction
            .execute(
                "UPDATE jobs SET next_run_at = ?2 WHERE id = ?1",
                rusqlite::params![job_id, next],
            )
            .map_err(|err| Error::Store(format!("cannot set next_run_at: {err}")))?;
    }
    if let Some(last) = &update.last_run_at {
        transaction
            .execute(
                "UPDATE jobs SET last_run_at = ?2 WHERE id = ?1",
                rusqlite::params![job_id, last],
            )
            .map_err(|err| Error::Store(format!("cannot set last_run_at: {err}")))?;
    }
    if let Some(status) = &update.last_status {
        transaction
            .execute(
                "UPDATE jobs SET last_status = ?2 WHERE id = ?1",
                rusqlite::params![job_id, status],
            )
            .map_err(|err| Error::Store(format!("cannot set last_status: {err}")))?;
    }
    if let Some(enabled) = update.enabled {
        transaction
            .execute(
                "UPDATE jobs SET enabled = ?2 WHERE id = ?1",
                rusqlite::params![job_id, if enabled { 1 } else { 0 }],
            )
            .map_err(|err| Error::Store(format!("cannot set enabled: {err}")))?;
    }
    if let Some(session) = &update.session_id {
        transaction
            .execute(
                "UPDATE jobs SET session_id = ?2 WHERE id = ?1",
                rusqlite::params![job_id, session],
            )
            .map_err(|err| Error::Store(format!("cannot adopt the job session: {err}")))?;
    }
    if update.increment_runs {
        transaction
            .execute(
                "UPDATE jobs SET runs_count = runs_count + 1 WHERE id = ?1",
                rusqlite::params![job_id],
            )
            .map_err(|err| Error::Store(format!("cannot count the run: {err}")))?;
    }
    Ok(())
}

fn read_run(row: &Row<'_>) -> rusqlite::Result<JobRun> {
    let raw_status: String = row.get(5)?;
    let status =
        JobStatus::parse(&raw_status).map_err(|_| unreadable(5, "run status", &raw_status))?;
    Ok(JobRun {
        id: row.get(0)?,
        job_id: row.get(1)?,
        session_id: row.get(2)?,
        started_at: row.get(3)?,
        finished_at: row.get(4)?,
        status,
        exit_summary: row.get(6)?,
        output_ref: row.get(7)?,
    })
}

#[async_trait::async_trait]
impl JobStore for StoreJobs {
    async fn create_job(&self, new: NewJob) -> Result<Job> {
        let created = new.clone();
        self.0
            .blocking(move |conn: &Connection| {
                conn.execute(
                    "INSERT INTO jobs
                       (id, name, schedule, timezone, prompt, cwd, session_id, session_mode,
                        enabled, allow_overlap, max_runs, runs_count, created_at, next_run_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, 1, ?8, ?9, 0, ?10, ?11)",
                    rusqlite::params![
                        new.id,
                        new.name,
                        new.schedule,
                        new.timezone,
                        new.prompt,
                        new.cwd,
                        new.session_mode.as_str(),
                        if new.allow_overlap { 1 } else { 0 },
                        new.max_runs,
                        new.created_at,
                        new.next_run_at,
                    ],
                )
                .map_err(|err| Error::Store(format!("cannot create the job: {err}")))?;
                Ok(())
            })
            .await?;

        Ok(Job {
            id: created.id,
            name: created.name,
            schedule: created.schedule,
            timezone: created.timezone,
            prompt: created.prompt,
            cwd: created.cwd,
            session_id: None,
            session_mode: created.session_mode,
            enabled: true,
            allow_overlap: created.allow_overlap,
            max_runs: created.max_runs,
            runs_count: 0,
            created_at: created.created_at,
            last_run_at: None,
            next_run_at: created.next_run_at,
            last_status: None,
        })
    }

    async fn job(&self, key: &str) -> Result<Option<Job>> {
        let key = key.to_string();
        self.0
            .blocking(move |conn| {
                conn.query_row(
                    &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1 OR name = ?1"),
                    rusqlite::params![key],
                    read_job,
                )
                .optional()
                .map_err(|err| Error::Store(format!("cannot read the job: {err}")))
            })
            .await
    }

    async fn list_jobs(&self) -> Result<Vec<Job>> {
        self.0
            .blocking(move |conn| {
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT {JOB_COLUMNS} FROM jobs ORDER BY created_at ASC, id ASC"
                    ))
                    .map_err(|err| Error::Store(format!("cannot list jobs: {err}")))?;
                let rows = stmt
                    .query_map([], read_job)
                    .map_err(|err| Error::Store(format!("cannot list jobs: {err}")))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|err| Error::Store(format!("cannot list jobs: {err}")))
            })
            .await
    }

    async fn remove_job(&self, key: &str) -> Result<bool> {
        let key = key.to_string();
        self.0
            .blocking(move |conn| {
                let changed = conn
                    .execute(
                        "DELETE FROM jobs WHERE id = ?1 OR name = ?1",
                        rusqlite::params![key],
                    )
                    .map_err(|err| Error::Store(format!("cannot remove the job: {err}")))?;
                Ok(changed > 0)
            })
            .await
    }

    async fn due_jobs(&self, now: chrono::DateTime<chrono::Utc>) -> Result<Vec<Job>> {
        let now = stamp(now);
        self.0
            .blocking(move |conn| {
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT {JOB_COLUMNS} FROM jobs
                         WHERE enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= ?1
                         ORDER BY next_run_at ASC, id ASC"
                    ))
                    .map_err(|err| Error::Store(format!("cannot query due jobs: {err}")))?;
                let rows = stmt
                    .query_map(rusqlite::params![now], read_job)
                    .map_err(|err| Error::Store(format!("cannot query due jobs: {err}")))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|err| Error::Store(format!("cannot query due jobs: {err}")))
            })
            .await
    }

    async fn start_run(&self, run: NewJobRun, update: JobUpdate) -> Result<()> {
        self.0
            .blocking(move |conn| {
                let transaction = conn
                    .unchecked_transaction()
                    .map_err(|err| Error::Store(format!("cannot begin a transaction: {err}")))?;
                transaction
                    .execute(
                        "INSERT INTO job_runs
                           (id, job_id, session_id, started_at, status, exit_summary)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![
                            run.id,
                            run.job_id,
                            run.session_id,
                            run.started_at,
                            run.status.as_str(),
                            run.exit_summary,
                        ],
                    )
                    .map_err(|err| Error::Store(format!("cannot record the run: {err}")))?;
                apply_update(&transaction, &run.job_id, &update)?;
                transaction
                    .commit()
                    .map_err(|err| Error::Store(format!("cannot commit the run: {err}")))?;
                Ok(())
            })
            .await
    }

    async fn finish_run(
        &self,
        run_id: &str,
        status: JobStatus,
        finished_at: chrono::DateTime<chrono::Utc>,
        exit_summary: Option<String>,
        output_ref: Option<String>,
        update: JobUpdate,
    ) -> Result<()> {
        let run_id = run_id.to_string();
        let finished = stamp(finished_at);
        self.0
            .blocking(move |conn| {
                let transaction = conn
                    .unchecked_transaction()
                    .map_err(|err| Error::Store(format!("cannot begin a transaction: {err}")))?;
                let job_id: String = transaction
                    .query_row(
                        "SELECT job_id FROM job_runs WHERE id = ?1",
                        rusqlite::params![run_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|err| Error::Store(format!("cannot find the run: {err}")))?
                    .ok_or_else(|| Error::Store(format!("no run `{run_id}` to finish")))?;

                transaction
                    .execute(
                        "UPDATE job_runs
                         SET finished_at = ?2, status = ?3, exit_summary = ?4, output_ref = ?5
                         WHERE id = ?1",
                        rusqlite::params![
                            run_id,
                            finished,
                            status.as_str(),
                            exit_summary,
                            output_ref,
                        ],
                    )
                    .map_err(|err| Error::Store(format!("cannot finish the run: {err}")))?;
                apply_update(&transaction, &job_id, &update)?;
                transaction
                    .commit()
                    .map_err(|err| Error::Store(format!("cannot commit the run: {err}")))?;
                Ok(())
            })
            .await
    }

    async fn update_job(&self, job_id: &str, update: JobUpdate) -> Result<()> {
        let job_id = job_id.to_string();
        self.0
            .blocking(move |conn| {
                let transaction = conn
                    .unchecked_transaction()
                    .map_err(|err| Error::Store(format!("cannot begin a transaction: {err}")))?;
                apply_update(&transaction, &job_id, &update)?;
                transaction
                    .commit()
                    .map_err(|err| Error::Store(format!("cannot commit the job: {err}")))?;
                Ok(())
            })
            .await
    }

    async fn mark_run_running(&self, run_id: &str) -> Result<()> {
        let run_id = run_id.to_string();
        self.0
            .blocking(move |conn| {
                conn.execute(
                    "UPDATE job_runs SET status = 'running' WHERE id = ?1 AND status = 'queued'",
                    rusqlite::params![run_id],
                )
                .map_err(|err| Error::Store(format!("cannot promote the run: {err}")))?;
                Ok(())
            })
            .await
    }

    async fn list_runs(&self, job_id: &str, limit: usize) -> Result<Vec<JobRun>> {
        let job_id = job_id.to_string();
        self.0
            .blocking(move |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, job_id, session_id, started_at, finished_at, status,
                                exit_summary, output_ref
                         FROM job_runs WHERE job_id = ?1
                         ORDER BY started_at DESC, rowid DESC LIMIT ?2",
                    )
                    .map_err(|err| Error::Store(format!("cannot list runs: {err}")))?;
                let rows = stmt
                    .query_map(rusqlite::params![job_id, limit as i64], read_run)
                    .map_err(|err| Error::Store(format!("cannot list runs: {err}")))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|err| Error::Store(format!("cannot list runs: {err}")))
            })
            .await
    }

    async fn abandon_open_runs(&self, now: chrono::DateTime<chrono::Utc>) -> Result<usize> {
        let finished = stamp(now);
        self.0
            .blocking(move |conn| {
                let transaction = conn
                    .unchecked_transaction()
                    .map_err(|err| Error::Store(format!("cannot begin a transaction: {err}")))?;
                let changed = transaction
                    .execute(
                        "UPDATE job_runs
                         SET status = 'failed', finished_at = ?1,
                             exit_summary = 'interrupted: minion stopped while this run was in flight'
                         WHERE status IN ('running', 'queued')",
                        rusqlite::params![finished],
                    )
                    .map_err(|err| Error::Store(format!("cannot close open runs: {err}")))?;
                // The job's own summary is a copy of its latest run's status and
                // would otherwise keep claiming a run that no longer exists.
                transaction
                    .execute(
                        "UPDATE jobs SET last_status = 'failed'
                         WHERE last_status IN ('running', 'queued')",
                        [],
                    )
                    .map_err(|err| Error::Store(format!("cannot correct the job: {err}")))?;
                transaction
                    .commit()
                    .map_err(|err| Error::Store(format!("cannot close open runs: {err}")))?;
                Ok(changed)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use minion_core::job::stamp;

    async fn store() -> Arc<Store> {
        Arc::new(Store::open_in_memory().await.unwrap())
    }

    fn start() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 3, 2, 9, 0, 0).unwrap()
    }

    fn new_job(name: Option<&str>, next: chrono::DateTime<chrono::Utc>) -> NewJob {
        NewJob {
            id: minion_core::new_session_id(),
            name: name.map(str::to_string),
            schedule: "0 9 * * 1".to_string(),
            timezone: "UTC".to_string(),
            prompt: "publish the digest".to_string(),
            cwd: "/tmp".to_string(),
            session_mode: SessionMode::New,
            allow_overlap: false,
            max_runs: None,
            next_run_at: Some(stamp(next)),
            created_at: stamp(start()),
        }
    }

    #[tokio::test]
    async fn a_job_round_trips_and_is_readable_by_name() {
        let store = store().await;
        let jobs = store.jobs();
        let created = jobs
            .create_job(new_job(Some("weekly"), start() + Duration::days(7)))
            .await
            .unwrap();

        let by_id = jobs.job(&created.id).await.unwrap().unwrap();
        let by_name = jobs.job("weekly").await.unwrap().unwrap();

        assert_eq!(by_id, by_name);
        assert_eq!(by_name.prompt, "publish the digest");
        assert!(by_name.enabled);
        assert!(!by_name.allow_overlap);
        assert_eq!(by_name.runs_count, 0);
        assert_eq!(by_name.session_mode, SessionMode::New);
        assert_eq!(jobs.list_jobs().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_name_is_unique() {
        let store = store().await;
        let jobs = store.jobs();
        jobs.create_job(new_job(Some("weekly"), start()))
            .await
            .unwrap();

        let err = jobs
            .create_job(new_job(Some("weekly"), start()))
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("cannot create the job"),
            "was: {err}"
        );
    }

    #[tokio::test]
    async fn due_jobs_are_the_enabled_ones_whose_time_has_come() {
        let store = store().await;
        let jobs = store.jobs();
        let due = jobs
            .create_job(new_job(Some("due"), start() - Duration::minutes(5)))
            .await
            .unwrap();
        jobs.create_job(new_job(Some("later"), start() + Duration::days(1)))
            .await
            .unwrap();
        let disabled = jobs
            .create_job(new_job(Some("off"), start() - Duration::days(1)))
            .await
            .unwrap();
        jobs.update_job(
            &disabled.id,
            JobUpdate {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let found = jobs.due_jobs(start()).await.unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, due.id);
    }

    #[tokio::test]
    async fn a_fire_records_the_run_and_advances_the_job_together() {
        let store = store().await;
        let jobs = store.jobs();
        let job = jobs.create_job(new_job(None, start())).await.unwrap();
        let run_id = minion_core::new_session_id();
        let next = start() + Duration::days(7);

        jobs.start_run(
            NewJobRun {
                id: run_id.clone(),
                job_id: job.id.clone(),
                session_id: None,
                started_at: stamp(start()),
                status: JobStatus::Running,
                exit_summary: None,
            },
            JobUpdate {
                next_run_at: Some(stamp(next)),
                last_run_at: Some(stamp(start())),
                last_status: Some(JobStatus::Running.as_str().to_string()),
                increment_runs: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let runs = jobs.list_runs(&job.id, 10).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, JobStatus::Running);
        assert!(runs[0].finished_at.is_none());

        let reloaded = jobs.job(&job.id).await.unwrap().unwrap();
        assert_eq!(reloaded.next_run_at.as_deref(), Some(stamp(next).as_str()));
        assert_eq!(reloaded.runs_count, 1);
        assert_eq!(reloaded.last_status.as_deref(), Some("running"));
        // The occurrence was consumed at fire time, so the job is no longer due
        // even though the run has not finished.
        assert!(jobs.due_jobs(start()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn finishing_a_run_writes_the_status_and_the_next_occurrence_together() {
        let store = store().await;
        let jobs = store.jobs();
        let job = jobs.create_job(new_job(None, start())).await.unwrap();
        let run_id = minion_core::new_session_id();
        jobs.start_run(
            NewJobRun {
                id: run_id.clone(),
                job_id: job.id.clone(),
                session_id: None,
                started_at: stamp(start()),
                status: JobStatus::Running,
                exit_summary: None,
            },
            JobUpdate::default(),
        )
        .await
        .unwrap();

        let next = start() + Duration::days(7);
        jobs.finish_run(
            &run_id,
            JobStatus::Ok,
            start() + Duration::seconds(2),
            Some("completed".to_string()),
            Some("session-1".to_string()),
            JobUpdate {
                next_run_at: Some(stamp(next)),
                last_status: Some(JobStatus::Ok.as_str().to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let runs = jobs.list_runs(&job.id, 10).await.unwrap();
        assert_eq!(runs[0].status, JobStatus::Ok);
        assert!(runs[0].finished_at.is_some());
        assert_eq!(runs[0].output_ref.as_deref(), Some("session-1"));
        let reloaded = jobs.job(&job.id).await.unwrap().unwrap();
        assert_eq!(reloaded.next_run_at.as_deref(), Some(stamp(next).as_str()));
        assert_eq!(reloaded.last_status.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn removing_a_job_takes_its_runs_with_it() {
        let store = store().await;
        let jobs = store.jobs();
        let job = jobs
            .create_job(new_job(Some("weekly"), start()))
            .await
            .unwrap();
        jobs.start_run(
            NewJobRun {
                id: minion_core::new_session_id(),
                job_id: job.id.clone(),
                session_id: None,
                started_at: stamp(start()),
                status: JobStatus::Skipped,
                exit_summary: Some("missed".to_string()),
            },
            JobUpdate::default(),
        )
        .await
        .unwrap();

        assert!(jobs.remove_job("weekly").await.unwrap());
        assert!(jobs.job(&job.id).await.unwrap().is_none());
        assert!(jobs.list_runs(&job.id, 10).await.unwrap().is_empty());
        assert!(!jobs.remove_job("weekly").await.unwrap());
    }

    #[tokio::test]
    async fn a_run_left_in_flight_by_a_stopped_process_is_closed() {
        let store = store().await;
        let jobs = store.jobs();
        let job = jobs.create_job(new_job(None, start())).await.unwrap();
        jobs.start_run(
            NewJobRun {
                id: minion_core::new_session_id(),
                job_id: job.id.clone(),
                session_id: None,
                started_at: stamp(start()),
                status: JobStatus::Running,
                exit_summary: None,
            },
            JobUpdate::default(),
        )
        .await
        .unwrap();

        let closed = jobs
            .abandon_open_runs(start() + Duration::minutes(1))
            .await
            .unwrap();

        assert_eq!(closed, 1);
        let runs = jobs.list_runs(&job.id, 10).await.unwrap();
        assert_eq!(runs[0].status, JobStatus::Failed);
        assert!(
            runs[0]
                .exit_summary
                .as_deref()
                .unwrap_or_default()
                .contains("interrupted"),
            "the row must say why it was closed"
        );
    }
}
