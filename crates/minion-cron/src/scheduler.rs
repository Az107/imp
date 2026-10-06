//! The in-process scheduler (SDD §5.7, D5, FR-18–FR-21).
//!
//! One task ticks once a second and asks the store which jobs are due; each due
//! occurrence becomes a `job_runs` row **before** anything is dispatched, so an
//! acknowledged run always has a durable trace (NFR-6). Everything the scheduler
//! decides — whether a run may start, what `next_run_at` becomes, what a missed
//! occurrence costs — is decided here and only recorded by the store, which
//! keeps this crate free of SQLite and of HTTP.
//!
//! Time comes from a [`Clock`], never from `Utc::now()`, so a test can place the
//! process on a virtual clock and drive a weekly job without sleeping (§8).
//!
//! The prompt is executed by a [`JobRunner`]. In the shipped binary that is an
//! agent turn; in tests it is a recorder. That seam is what lets the firing
//! rules be tested without a provider, a key, or a socket.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use minion_core::clock::Clock;
use minion_core::config::{CronConfig, MissedRunPolicy};
use minion_core::error::Result;
use minion_core::job::{
    Job, JobStatus, JobStore, JobUpdate, NewJobRun, SessionMode, parse_stamp, stamp,
};
use minion_core::new_session_id;

use crate::schedule;

/// Knobs the scheduler reads, resolved from `[cron]`.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Whether the tick loop may fire anything at all.
    pub enabled: bool,
    /// What to do with occurrences missed while the process was not running.
    pub missed_run_policy: MissedRunPolicy,
    /// Runs allowed in flight at once; the excess is queued.
    pub max_concurrent_jobs: usize,
    /// Ceiling on the runs one `run_all` catch-up may start.
    pub missed_run_cap: usize,
}

impl From<&CronConfig> for SchedulerConfig {
    fn from(config: &CronConfig) -> Self {
        Self {
            enabled: config.enabled,
            missed_run_policy: config.missed_run_policy,
            max_concurrent_jobs: config.max_concurrent_jobs.max(1),
            missed_run_cap: config.missed_run_cap.max(1),
        }
    }
}

/// What one run of a job produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// Whether the turn completed.
    pub ok: bool,
    /// One-line outcome, stored as `exit_summary`.
    pub summary: String,
    /// Where the run's output lives, stored as `output_ref`.
    pub output_ref: Option<String>,
    /// The session the run used, adopted by a `reuse` job.
    pub session_id: Option<String>,
}

impl RunReport {
    /// A run that finished.
    pub fn ok(summary: impl Into<String>) -> Self {
        Self {
            ok: true,
            summary: summary.into(),
            output_ref: None,
            session_id: None,
        }
    }

    /// A run that did not.
    pub fn failed(summary: impl Into<String>) -> Self {
        Self {
            ok: false,
            summary: summary.into(),
            output_ref: None,
            session_id: None,
        }
    }

    /// Record where the output went.
    pub fn with_output_ref(mut self, reference: Option<String>) -> Self {
        self.output_ref = reference;
        self
    }

    /// Record which session the run used.
    pub fn with_session(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }
}

/// Puts a job's prompt in front of the agent. Implemented by `minion-cli`.
///
/// The runner is handed the job, not a rendered prompt, because the session
/// bookkeeping a `reuse` job needs is the caller's: only the CLI knows how to
/// create a session, load a transcript, and build the non-interactive gate.
#[async_trait]
pub trait JobRunner: Send + Sync {
    /// Run `job`'s prompt for the run row `run_id`.
    async fn run(&self, job: &Job, run_id: &str) -> RunReport;
}

/// What one tick or one catch-up did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Runs dispatched immediately.
    pub fired: usize,
    /// Runs accepted but waiting for a free slot.
    pub queued: usize,
    /// Occurrences dropped because the previous run was still active.
    pub overlapped: usize,
    /// Capacity or schedule decisions that started nothing.
    pub skipped: usize,
}

/// A run the scheduler believes is in flight.
struct Inflight {
    queued: bool,
}

/// A run accepted but not yet dispatched.
struct QueuedRun {
    job: Job,
    run_id: String,
}

#[derive(Default)]
struct State {
    /// Job id → the run in flight for it.
    inflight: HashMap<String, Inflight>,
    /// Runs waiting for a concurrency slot, oldest first.
    queue: VecDeque<QueuedRun>,
    /// Spawned run tasks, kept so a test can wait for them.
    handles: Vec<JoinHandle<()>>,
}

/// The scheduler. Cheap to clone; every clone shares one position.
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<dyn JobStore>,
    runner: Arc<dyn JobRunner>,
    clock: Arc<dyn Clock>,
    config: SchedulerConfig,
    state: Mutex<State>,
}

impl Scheduler {
    /// Build a scheduler over a store, a runner, and a clock.
    pub fn new(
        store: Arc<dyn JobStore>,
        runner: Arc<dyn JobRunner>,
        clock: Arc<dyn Clock>,
        config: SchedulerConfig,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                runner,
                clock,
                config,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Close runs a previous process left in flight.
    ///
    /// Call once at startup, before [`catch_up`](Self::catch_up): a row claiming
    /// to be `running` that nothing is running would otherwise be read as a live
    /// run and reported by `/cron` forever.
    pub async fn reconcile(&self) -> Result<usize> {
        self.inner.store.abandon_open_runs(self.now()).await
    }

    /// Apply `missed_run_policy` to every job that came due while minion was closed.
    pub async fn catch_up(&self) -> Result<TickReport> {
        let mut report = TickReport::default();
        if !self.inner.config.enabled {
            return Ok(report);
        }
        let now = self.now();
        for job in self.inner.store.due_jobs(now).await? {
            let Some(first) = job.next_run_at.as_deref().map(parse_stamp).transpose()? else {
                continue;
            };
            let zone = schedule::timezone(&job.timezone)?;
            let expression = schedule::expression(&job.schedule)?;

            match self.inner.config.missed_run_policy {
                MissedRunPolicy::Skip => {
                    let next = schedule::next_after(&expression, zone, now);
                    self.record_skip(
                        &job,
                        now,
                        next,
                        "missed while minion was not running, and skipped by cron.missed_run_policy",
                    )
                    .await?;
                    report.skipped += 1;
                }
                MissedRunPolicy::RunOnce => {
                    let next = schedule::next_after(&expression, zone, now);
                    match self.fire_one(&job, first, next, now, true, false).await? {
                        FireOutcome::Fired => report.fired += 1,
                        FireOutcome::Queued => report.queued += 1,
                        FireOutcome::Retired => report.skipped += 1,
                        FireOutcome::Overlapped => {}
                    }
                }
                MissedRunPolicy::RunAll => {
                    let missed = schedule::occurrences(
                        &expression,
                        zone,
                        first,
                        now,
                        self.inner.config.missed_run_cap,
                    );
                    let tail = schedule::next_after(&expression, zone, now);
                    for (index, occurrence) in missed.iter().enumerate() {
                        let next = missed.get(index + 1).copied().or(tail);
                        // No overlap check: these occurrences never ran, so there
                        // is no live run to collide with. The concurrency cap
                        // still applies, which is what bounds the burst.
                        self.fire_one(&job, *occurrence, next, now, true, false)
                            .await?;
                    }
                    report.fired += missed.len();
                }
            }
        }
        Ok(report)
    }

    /// One tick: pump the queue, then fire everything that is due.
    pub async fn tick(&self) -> Result<TickReport> {
        self.pump().await?;
        let mut report = TickReport::default();
        if !self.inner.config.enabled {
            return Ok(report);
        }
        let now = self.now();
        for job in self.inner.store.due_jobs(now).await? {
            let Some(occurrence) = job.next_run_at.as_deref().map(parse_stamp).transpose()? else {
                continue;
            };
            let zone = schedule::timezone(&job.timezone)?;
            let expression = schedule::expression(&job.schedule)?;
            let next = schedule::next_after(&expression, zone, occurrence);

            match self
                .fire_one(&job, occurrence, next, now, true, true)
                .await?
            {
                FireOutcome::Fired => report.fired += 1,
                FireOutcome::Queued => report.queued += 1,
                FireOutcome::Overlapped => report.overlapped += 1,
                FireOutcome::Retired => report.skipped += 1,
            }
        }
        Ok(report)
    }

    /// Wait for every in-flight run to finish, including ones a completion set going.
    pub async fn drain(&self) {
        loop {
            let handles: Vec<JoinHandle<()>> = match self.inner.state.lock() {
                Ok(mut state) => std::mem::take(&mut state.handles),
                Err(_) => return,
            };
            if handles.is_empty() {
                return;
            }
            for handle in handles {
                let _ = handle.await;
            }
        }
    }

    /// How many runs are dispatched right now.
    pub fn running_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .map(|state| state.inflight.values().filter(|run| !run.queued).count())
            .unwrap_or(0)
    }

    fn now(&self) -> DateTime<Utc> {
        self.inner.clock.now()
    }

    /// Whether `job_id` already has a run in flight or waiting.
    fn is_busy(&self, job_id: &str) -> bool {
        self.inner
            .state
            .lock()
            .map(|state| state.inflight.contains_key(job_id))
            .unwrap_or(false)
    }

    /// Insert a run row and update the job, in one transaction, then dispatch.
    ///
    /// `next` is what `next_run_at` becomes, `count_run` whether the job's run
    /// counter advances, and `check_overlap` whether a live run blocks this one.
    async fn fire_one(
        &self,
        job: &Job,
        occurrence: DateTime<Utc>,
        next: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
        count_run: bool,
        check_overlap: bool,
    ) -> Result<FireOutcome> {
        // A job that has spent its run budget is retired rather than fired, so
        // `cron_add`'s `max_runs` cannot turn into an unbounded loop.
        if let Some(budget) = job.max_runs
            && job.runs_count >= budget
        {
            self.inner
                .store
                .update_job(
                    &job.id,
                    JobUpdate {
                        enabled: Some(false),
                        last_status: Some(JobStatus::Skipped.as_str().to_string()),
                        ..JobUpdate::default()
                    },
                )
                .await?;
            return Ok(FireOutcome::Retired);
        }

        if check_overlap && self.is_busy(&job.id) && !job.allow_overlap {
            self.inner
                .store
                .start_run(
                    NewJobRun {
                        id: new_session_id(),
                        job_id: job.id.clone(),
                        session_id: job.session_id.clone(),
                        started_at: stamp(now),
                        status: JobStatus::Overlap,
                        exit_summary: Some(
                            "skipped (overlap): the previous run was still active".to_string(),
                        ),
                    },
                    JobUpdate {
                        next_run_at: next.map(stamp),
                        last_run_at: Some(stamp(occurrence)),
                        last_status: Some(JobStatus::Overlap.as_str().to_string()),
                        ..JobUpdate::default()
                    },
                )
                .await?;
            return Ok(FireOutcome::Overlapped);
        }

        let slot = self.running_count() < self.inner.config.max_concurrent_jobs;
        let status = if slot {
            JobStatus::Running
        } else {
            JobStatus::Queued
        };
        let run_id = new_session_id();
        self.inner
            .store
            .start_run(
                NewJobRun {
                    id: run_id.clone(),
                    job_id: job.id.clone(),
                    session_id: job.session_id.clone(),
                    started_at: stamp(now),
                    status,
                    exit_summary: None,
                },
                JobUpdate {
                    next_run_at: next.map(stamp),
                    last_run_at: Some(stamp(occurrence)),
                    last_status: Some(status.as_str().to_string()),
                    increment_runs: count_run,
                    ..JobUpdate::default()
                },
            )
            .await?;

        // The row exists before the prompt is dispatched (NFR-6), so a crash
        // between here and the runner's first action still leaves a trace.
        if let Ok(mut state) = self.inner.state.lock() {
            state
                .inflight
                .insert(job.id.clone(), Inflight { queued: !slot });
        }

        if slot {
            self.spawn(job.clone(), run_id);
            Ok(FireOutcome::Fired)
        } else {
            if let Ok(mut state) = self.inner.state.lock() {
                state.queue.push_back(QueuedRun {
                    job: job.clone(),
                    run_id,
                });
            }
            Ok(FireOutcome::Queued)
        }
    }

    /// Record a run that is deliberately not run, and move the schedule on.
    async fn record_skip(
        &self,
        job: &Job,
        now: DateTime<Utc>,
        next: Option<DateTime<Utc>>,
        reason: &str,
    ) -> Result<()> {
        self.inner
            .store
            .start_run(
                NewJobRun {
                    id: new_session_id(),
                    job_id: job.id.clone(),
                    session_id: job.session_id.clone(),
                    started_at: stamp(now),
                    status: JobStatus::Skipped,
                    exit_summary: Some(reason.to_string()),
                },
                JobUpdate {
                    next_run_at: next.map(stamp),
                    last_status: Some(JobStatus::Skipped.as_str().to_string()),
                    ..JobUpdate::default()
                },
            )
            .await
    }

    /// Start queued runs while there is a free slot.
    async fn pump(&self) -> Result<()> {
        loop {
            if self.running_count() >= self.inner.config.max_concurrent_jobs {
                return Ok(());
            }
            let queued = match self.inner.state.lock() {
                Ok(mut state) => state.queue.pop_front(),
                Err(_) => None,
            };
            let Some(queued) = queued else {
                return Ok(());
            };

            self.inner.store.mark_run_running(&queued.run_id).await?;
            self.inner
                .store
                .update_job(
                    &queued.job.id,
                    JobUpdate {
                        last_status: Some(JobStatus::Running.as_str().to_string()),
                        ..JobUpdate::default()
                    },
                )
                .await?;
            if let Ok(mut state) = self.inner.state.lock()
                && let Some(run) = state.inflight.get_mut(&queued.job.id)
            {
                run.queued = false;
            }
            self.spawn(queued.job, queued.run_id);
        }
    }

    /// Dispatch a run on its own task, so one slow job cannot stall the tick.
    fn spawn(&self, job: Job, run_id: String) {
        let me = self.clone();
        let handle = tokio::spawn(async move {
            let report = me.inner.runner.run(&job, &run_id).await;
            me.finish(&job, &run_id, report).await;
        });
        if let Ok(mut state) = self.inner.state.lock() {
            state.handles.push(handle);
        }
    }

    /// Write a run's terminal status and the job's new `next_run_at`, then pump.
    async fn finish(&self, job: &Job, run_id: &str, report: RunReport) {
        let now = self.now();
        let next = match (
            schedule::expression(&job.schedule),
            schedule::timezone(&job.timezone),
        ) {
            (Ok(expression), Ok(zone)) => schedule::next_after(&expression, zone, now),
            _ => None,
        };
        let status = if report.ok {
            JobStatus::Ok
        } else {
            JobStatus::Failed
        };
        let update = JobUpdate {
            next_run_at: next.map(stamp),
            last_status: Some(status.as_str().to_string()),
            ..JobUpdate::default()
        };

        if let Err(err) = self
            .inner
            .store
            .finish_run(
                run_id,
                status,
                now,
                Some(report.summary),
                report.output_ref,
                update,
            )
            .await
        {
            tracing::warn!(error = %err, run = %run_id, "cannot record the job run");
        }

        // Adopting a session is a separate write on purpose. A `reuse` job's
        // session is created by the runner, and `jobs.session_id` is a foreign
        // key: if the runner reported a session the store does not have, the
        // adoption fails — and it must not take the run's terminal status down
        // with it, or the job would be left claiming to be running forever.
        if job.session_mode == SessionMode::Reuse
            && let Some(session) = report.session_id
            && let Err(err) = self
                .inner
                .store
                .update_job(
                    &job.id,
                    JobUpdate {
                        session_id: Some(session),
                        ..JobUpdate::default()
                    },
                )
                .await
        {
            tracing::warn!(error = %err, job = %job.id, "cannot adopt the job session");
        }
        if let Ok(mut state) = self.inner.state.lock() {
            state.inflight.remove(&job.id);
        }
        if let Err(err) = self.pump().await {
            tracing::warn!(error = %err, "cannot start a queued job run");
        }
    }
}

/// The outcome of one fire attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FireOutcome {
    Fired,
    Queued,
    Overlapped,
    Retired,
}

/// Tick once a second, aligned to the second boundary, until cancelled.
///
/// The alignment is what makes `* * * * *` fire at the top of the minute rather
/// than a second after the process happened to start.
pub fn spawn_loop(scheduler: Scheduler, cancel: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let sub_millis = scheduler.now().timestamp_subsec_millis();
            let wait = StdDuration::from_millis(u64::from(1000 - sub_millis.min(999)));
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(wait) => {}
            }
            if let Err(err) = scheduler.tick().await {
                tracing::warn!(error = %err, "cron tick failed");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use chrono::{Duration, TimeZone};
    use minion_core::clock::ManualClock;
    use minion_core::job::{JobRun, NewJob};

    // ------------------------------------------------------------- the fakes

    /// An in-memory [`JobStore`] good enough to drive the scheduler.
    ///
    /// The real SQLite implementation is exercised in `minion-store` and in
    /// `tests/cron_end_to_end.rs`; this one exists so the *decisions* can be
    /// tested without a database in the way.
    #[derive(Default)]
    struct FakeStore {
        jobs: Mutex<Vec<Job>>,
        runs: Mutex<Vec<JobRun>>,
    }

    impl FakeStore {
        fn runs(&self) -> Vec<JobRun> {
            self.runs.lock().unwrap().clone()
        }

        fn job(&self, id: &str) -> Job {
            self.jobs
                .lock()
                .unwrap()
                .iter()
                .find(|job| job.id == id)
                .expect("job")
                .clone()
        }

        fn statuses(&self) -> Vec<JobStatus> {
            self.runs().iter().map(|run| run.status).collect()
        }
    }

    fn apply(jobs: &Mutex<Vec<Job>>, job_id: &str, update: &JobUpdate) {
        let mut jobs = jobs.lock().unwrap();
        let Some(job) = jobs.iter_mut().find(|job| job.id == job_id) else {
            return;
        };
        if let Some(next) = &update.next_run_at {
            job.next_run_at = Some(next.clone());
        }
        if let Some(last) = &update.last_run_at {
            job.last_run_at = Some(last.clone());
        }
        if let Some(status) = &update.last_status {
            job.last_status = Some(status.clone());
        }
        if let Some(enabled) = update.enabled {
            job.enabled = enabled;
        }
        if let Some(session) = &update.session_id {
            job.session_id = Some(session.clone());
        }
        if update.increment_runs {
            job.runs_count += 1;
        }
    }

    #[async_trait]
    impl JobStore for FakeStore {
        async fn create_job(&self, new: NewJob) -> Result<Job> {
            let job = Job {
                id: new.id,
                name: new.name,
                schedule: new.schedule,
                timezone: new.timezone,
                prompt: new.prompt,
                cwd: new.cwd,
                session_id: None,
                session_mode: new.session_mode,
                enabled: true,
                allow_overlap: new.allow_overlap,
                max_runs: new.max_runs,
                runs_count: 0,
                created_at: new.created_at,
                last_run_at: None,
                next_run_at: new.next_run_at,
                last_status: None,
            };
            self.jobs.lock().unwrap().push(job.clone());
            Ok(job)
        }

        async fn job(&self, key: &str) -> Result<Option<Job>> {
            Ok(self
                .jobs
                .lock()
                .unwrap()
                .iter()
                .find(|job| job.id == key || job.name.as_deref() == Some(key))
                .cloned())
        }

        async fn list_jobs(&self) -> Result<Vec<Job>> {
            Ok(self.jobs.lock().unwrap().clone())
        }

        async fn remove_job(&self, key: &str) -> Result<bool> {
            let mut jobs = self.jobs.lock().unwrap();
            let before = jobs.len();
            jobs.retain(|job| job.id != key && job.name.as_deref() != Some(key));
            Ok(jobs.len() != before)
        }

        async fn due_jobs(&self, now: DateTime<Utc>) -> Result<Vec<Job>> {
            let now = stamp(now);
            let mut due: Vec<Job> = self
                .jobs
                .lock()
                .unwrap()
                .iter()
                .filter(|job| {
                    job.enabled
                        && job
                            .next_run_at
                            .as_deref()
                            .is_some_and(|next| next <= now.as_str())
                })
                .cloned()
                .collect();
            due.sort_by(|a, b| a.next_run_at.cmp(&b.next_run_at));
            Ok(due)
        }

        async fn start_run(&self, run: NewJobRun, update: JobUpdate) -> Result<()> {
            self.runs.lock().unwrap().push(JobRun {
                id: run.id,
                job_id: run.job_id.clone(),
                session_id: run.session_id,
                started_at: run.started_at,
                finished_at: None,
                status: run.status,
                exit_summary: run.exit_summary,
                output_ref: None,
            });
            apply(&self.jobs, &run.job_id, &update);
            Ok(())
        }

        async fn finish_run(
            &self,
            run_id: &str,
            status: JobStatus,
            finished_at: DateTime<Utc>,
            exit_summary: Option<String>,
            output_ref: Option<String>,
            update: JobUpdate,
        ) -> Result<()> {
            let mut runs = self.runs.lock().unwrap();
            let run = runs
                .iter_mut()
                .find(|run| run.id == run_id)
                .expect("run to finish");
            run.status = status;
            run.finished_at = Some(stamp(finished_at));
            run.exit_summary = exit_summary;
            run.output_ref = output_ref;
            let job_id = run.job_id.clone();
            drop(runs);
            apply(&self.jobs, &job_id, &update);
            Ok(())
        }

        async fn update_job(&self, job_id: &str, update: JobUpdate) -> Result<()> {
            apply(&self.jobs, job_id, &update);
            Ok(())
        }

        async fn mark_run_running(&self, run_id: &str) -> Result<()> {
            for run in self.runs.lock().unwrap().iter_mut() {
                if run.id == run_id {
                    run.status = JobStatus::Running;
                }
            }
            Ok(())
        }

        async fn list_runs(&self, job_id: &str, limit: usize) -> Result<Vec<JobRun>> {
            let mut runs: Vec<JobRun> = self
                .runs
                .lock()
                .unwrap()
                .iter()
                .filter(|run| run.job_id == job_id)
                .cloned()
                .collect();
            runs.reverse();
            runs.truncate(limit);
            Ok(runs)
        }

        async fn abandon_open_runs(&self, now: DateTime<Utc>) -> Result<usize> {
            let mut closed = 0;
            for run in self.runs.lock().unwrap().iter_mut() {
                if run.status == JobStatus::Running || run.status == JobStatus::Queued {
                    run.status = JobStatus::Failed;
                    run.finished_at = Some(stamp(now));
                    closed += 1;
                }
            }
            Ok(closed)
        }
    }

    /// A runner that records what it ran, and can be made to hold.
    struct Recorder {
        store: Arc<FakeStore>,
        /// When present, a run waits for a permit before returning.
        gate: Option<Arc<tokio::sync::Semaphore>>,
        started: Mutex<Vec<String>>,
        finished: Mutex<Vec<String>>,
        /// Job labels whose run row was already `running` when the prompt started.
        running_at_dispatch: Mutex<Vec<String>>,
        /// Set when the run found its own `running` row already in the store.
        durable_before_dispatch: AtomicBool,
        output_ref: Option<String>,
    }

    impl Recorder {
        fn new(store: &Arc<FakeStore>, gate: Option<Arc<tokio::sync::Semaphore>>) -> Arc<Self> {
            Arc::new(Self {
                store: store.clone(),
                gate,
                started: Mutex::new(Vec::new()),
                finished: Mutex::new(Vec::new()),
                running_at_dispatch: Mutex::new(Vec::new()),
                durable_before_dispatch: AtomicBool::new(true),
                output_ref: None,
            })
        }

        fn started(&self) -> Vec<String> {
            self.started.lock().unwrap().clone()
        }

        fn finished(&self) -> Vec<String> {
            self.finished.lock().unwrap().clone()
        }

        /// How many runs were dispatched with their row already in `running`.
        fn dispatched_while_running(&self) -> usize {
            self.running_at_dispatch.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl JobRunner for Recorder {
        async fn run(&self, job: &Job, run_id: &str) -> RunReport {
            let row = self.store.runs().into_iter().find(|run| run.id == run_id);
            // NFR-6: the row must already be there when the prompt starts.
            let durable = row.as_ref().is_some_and(|run| run.finished_at.is_none());
            if !durable {
                self.durable_before_dispatch.store(false, Ordering::SeqCst);
            }
            if row.is_some_and(|run| run.status == JobStatus::Running) {
                self.running_at_dispatch.lock().unwrap().push(job.label());
            }
            self.started.lock().unwrap().push(job.label());
            if let Some(gate) = &self.gate {
                let permit = gate.clone().acquire_owned().await.expect("gate");
                drop(permit);
            }
            self.finished.lock().unwrap().push(job.label());
            RunReport::ok("done").with_output_ref(self.output_ref.clone())
        }
    }

    // ------------------------------------------------------------ the helpers

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    fn job(id: &str, schedule: &str, zone: &str, next: DateTime<Utc>) -> Job {
        Job {
            id: id.to_string(),
            name: Some(id.to_string()),
            schedule: schedule.to_string(),
            timezone: zone.to_string(),
            prompt: "publish the digest".to_string(),
            cwd: "/tmp".to_string(),
            session_id: None,
            session_mode: SessionMode::New,
            enabled: true,
            allow_overlap: false,
            max_runs: None,
            runs_count: 0,
            created_at: stamp(at(2026, 1, 1, 0, 0)),
            last_run_at: None,
            next_run_at: Some(stamp(next)),
            last_status: None,
        }
    }

    fn config(policy: MissedRunPolicy, max_concurrent_jobs: usize) -> SchedulerConfig {
        SchedulerConfig {
            enabled: true,
            missed_run_policy: policy,
            max_concurrent_jobs,
            missed_run_cap: 20,
        }
    }

    /// Let every spawned task run until `check` holds. Bounded, and it advances
    /// no clock: the point is to observe an already-reached state.
    async fn until(mut check: impl FnMut() -> bool) {
        for attempt in 0..10_000 {
            if check() {
                return;
            }
            if attempt % 50 == 0 {
                tokio::time::sleep(StdDuration::from_millis(1)).await;
            } else {
                tokio::task::yield_now().await;
            }
        }
        panic!("the condition never held");
    }

    fn seeded(store: &FakeStore, job: Job) {
        store.jobs.lock().unwrap().push(job);
    }

    // ------------------------------------------------------------- the cases

    /// The M4 exit criterion: a weekly job fires on a virtual clock.
    #[tokio::test]
    async fn a_weekly_job_fires_on_a_virtual_clock() {
        let store = Arc::new(FakeStore::default());
        // 2026-03-04 12:00 UTC is a Wednesday; the next Monday 09:00 in Mexico
        // City is 2026-03-09 15:00 UTC.
        seeded(
            &store,
            job(
                "weekly",
                "0 9 * * 1",
                "America/Mexico_City",
                at(2026, 3, 9, 15, 0),
            ),
        );
        let clock = Arc::new(ManualClock::at(at(2026, 3, 4, 12, 0)));
        let runner = Recorder::new(&store, None);
        let scheduler = Scheduler::new(
            store.clone(),
            runner.clone(),
            clock.clone(),
            config(MissedRunPolicy::RunOnce, 2),
        );

        // Nothing is due yet.
        assert_eq!(scheduler.tick().await.unwrap(), TickReport::default());
        assert!(runner.started().is_empty());

        clock.set(at(2026, 3, 9, 15, 0));
        let report = scheduler.tick().await.unwrap();
        scheduler.drain().await;

        assert_eq!(report.fired, 1);
        assert_eq!(runner.started(), vec!["weekly".to_string()]);
        assert!(
            runner.durable_before_dispatch.load(Ordering::SeqCst),
            "the job_runs row must exist before the prompt is dispatched (NFR-6)"
        );
        assert_eq!(store.statuses(), vec![JobStatus::Ok]);

        // The occurrence was consumed and the schedule moved to the next Monday.
        let reloaded = store.job("weekly");
        assert_eq!(
            reloaded.next_run_at.as_deref().unwrap(),
            stamp(at(2026, 3, 16, 15, 0))
        );
        assert_eq!(reloaded.last_status.as_deref(), Some("ok"));
        assert_eq!(reloaded.runs_count, 1);
    }

    #[tokio::test]
    async fn catch_up_skip_records_a_skipped_run_and_moves_on() {
        let store = Arc::new(FakeStore::default());
        seeded(
            &store,
            job("daily", "0 9 * * *", "UTC", at(2026, 3, 2, 9, 0)),
        );
        let clock = Arc::new(ManualClock::at(at(2026, 3, 5, 12, 0)));
        let runner = Recorder::new(&store, None);
        let scheduler = Scheduler::new(
            store.clone(),
            runner.clone(),
            clock,
            config(MissedRunPolicy::Skip, 2),
        );

        let report = scheduler.catch_up().await.unwrap();
        scheduler.drain().await;

        assert_eq!(report.skipped, 1);
        assert!(runner.started().is_empty(), "`skip` must not run the job");
        assert_eq!(store.statuses(), vec![JobStatus::Skipped]);
        assert_eq!(
            store.job("daily").next_run_at.as_deref().unwrap(),
            stamp(at(2026, 3, 6, 9, 0))
        );
    }

    #[tokio::test]
    async fn catch_up_run_once_runs_the_job_a_single_time() {
        let store = Arc::new(FakeStore::default());
        seeded(
            &store,
            job("daily", "0 9 * * *", "UTC", at(2026, 3, 2, 9, 0)),
        );
        let clock = Arc::new(ManualClock::at(at(2026, 3, 5, 12, 0)));
        let runner = Recorder::new(&store, None);
        let scheduler = Scheduler::new(
            store.clone(),
            runner.clone(),
            clock,
            config(MissedRunPolicy::RunOnce, 2),
        );

        let report = scheduler.catch_up().await.unwrap();
        scheduler.drain().await;

        assert_eq!(report.fired, 1);
        assert_eq!(
            runner.started().len(),
            1,
            "three occurrences missed, one run"
        );
        assert_eq!(
            store.job("daily").next_run_at.as_deref().unwrap(),
            stamp(at(2026, 3, 6, 9, 0)),
            "the schedule resumes from now, not from the missed occurrence"
        );
    }

    #[tokio::test]
    async fn catch_up_run_all_runs_every_missed_occurrence_within_the_cap() {
        let store = Arc::new(FakeStore::default());
        seeded(
            &store,
            job("daily", "0 9 * * *", "UTC", at(2026, 3, 2, 9, 0)),
        );
        let clock = Arc::new(ManualClock::at(at(2026, 3, 5, 12, 0)));
        let runner = Recorder::new(&store, None);
        let mut settings = config(MissedRunPolicy::RunAll, 4);
        settings.missed_run_cap = 3;
        let scheduler = Scheduler::new(store.clone(), runner.clone(), clock, settings);

        let report = scheduler.catch_up().await.unwrap();
        scheduler.drain().await;

        assert_eq!(
            report.fired, 3,
            "the 2nd, 3rd and 4th at 09:00, stopped by the cap"
        );
        assert_eq!(runner.started().len(), 3);
        assert_eq!(store.job("daily").runs_count, 3);
        assert_eq!(
            store.job("daily").next_run_at.as_deref().unwrap(),
            stamp(at(2026, 3, 6, 9, 0))
        );
    }

    /// DST: the expression is a local wall-clock time, so crossing the spring
    /// shift moves the UTC instant by an hour and keeps 09:00 at 09:00.
    #[tokio::test]
    async fn a_daily_job_keeps_its_local_hour_across_a_dst_shift() {
        let store = Arc::new(FakeStore::default());
        // 2026-03-08 09:00 EST is 14:00 UTC; on the 9th, 09:00 EDT is 13:00 UTC.
        seeded(
            &store,
            job(
                "daily",
                "0 9 * * *",
                "America/New_York",
                at(2026, 3, 8, 14, 0),
            ),
        );
        let clock = Arc::new(ManualClock::at(at(2026, 3, 8, 14, 0)));
        let runner = Recorder::new(&store, None);
        let scheduler = Scheduler::new(
            store.clone(),
            runner.clone(),
            clock.clone(),
            config(MissedRunPolicy::RunOnce, 2),
        );

        scheduler.tick().await.unwrap();
        scheduler.drain().await;

        let reloaded = store.job("daily");
        assert_eq!(
            reloaded.next_run_at.as_deref().unwrap(),
            stamp(at(2026, 3, 9, 13, 0)),
            "the local hour stays 09:00 while the UTC offset changes"
        );
        let zone = crate::schedule::timezone("America/New_York").unwrap();
        let next = minion_core::parse_stamp(reloaded.next_run_at.as_deref().unwrap()).unwrap();
        assert_eq!(
            next.with_timezone(&zone).to_string(),
            "2026-03-09 09:00:00 EDT"
        );
    }

    #[tokio::test]
    async fn an_overlapping_occurrence_is_recorded_and_not_run() {
        let store = Arc::new(FakeStore::default());
        seeded(
            &store,
            job("tick", "* * * * *", "UTC", at(2026, 3, 4, 12, 0)),
        );
        let clock = Arc::new(ManualClock::at(at(2026, 3, 4, 12, 0)));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let runner = Recorder::new(&store, Some(gate.clone()));
        let scheduler = Scheduler::new(
            store.clone(),
            runner.clone(),
            clock.clone(),
            config(MissedRunPolicy::RunOnce, 4),
        );

        // First occurrence: dispatched and held.
        assert_eq!(scheduler.tick().await.unwrap().fired, 1);
        until(|| runner.started().len() == 1).await;

        // A minute later the job is due again while the first run is still up.
        clock.advance(Duration::minutes(1));
        let report = scheduler.tick().await.unwrap();

        assert_eq!(report.overlapped, 1);
        assert_eq!(report.fired, 0);
        assert_eq!(runner.started().len(), 1, "a second run must not start");
        assert_eq!(
            store.statuses(),
            vec![JobStatus::Running, JobStatus::Overlap]
        );

        gate.add_permits(1);
        scheduler.drain().await;
        assert_eq!(runner.finished().len(), 1);
    }

    #[tokio::test]
    async fn an_overlapping_occurrence_runs_when_the_job_allows_it() {
        let store = Arc::new(FakeStore::default());
        let mut job = job("tick", "* * * * *", "UTC", at(2026, 3, 4, 12, 0));
        job.allow_overlap = true;
        seeded(&store, job);
        let clock = Arc::new(ManualClock::at(at(2026, 3, 4, 12, 0)));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let runner = Recorder::new(&store, Some(gate.clone()));
        let scheduler = Scheduler::new(
            store.clone(),
            runner.clone(),
            clock.clone(),
            config(MissedRunPolicy::RunOnce, 4),
        );

        scheduler.tick().await.unwrap();
        until(|| runner.started().len() == 1).await;
        clock.advance(Duration::minutes(1));
        let report = scheduler.tick().await.unwrap();

        assert_eq!(report.fired, 1);
        assert_eq!(report.overlapped, 0);
        until(|| runner.started().len() == 2).await;
        assert_eq!(
            store.statuses(),
            vec![JobStatus::Running, JobStatus::Running]
        );

        gate.add_permits(2);
        scheduler.drain().await;
    }

    #[tokio::test]
    async fn the_concurrency_cap_queues_the_excess_and_runs_it_later() {
        let store = Arc::new(FakeStore::default());
        seeded(
            &store,
            job("one", "0 9 * * *", "UTC", at(2026, 3, 4, 12, 0)),
        );
        seeded(
            &store,
            job("two", "0 9 * * *", "UTC", at(2026, 3, 4, 12, 0)),
        );
        let clock = Arc::new(ManualClock::at(at(2026, 3, 4, 12, 0)));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let runner = Recorder::new(&store, Some(gate.clone()));
        let scheduler = Scheduler::new(
            store.clone(),
            runner.clone(),
            clock,
            config(MissedRunPolicy::RunOnce, 1),
        );

        let report = scheduler.tick().await.unwrap();

        // No run can have finished: the gate is shut, so this state is stable.
        assert_eq!(report.fired, 1);
        assert_eq!(report.queued, 1);
        assert_eq!(
            store.statuses(),
            vec![JobStatus::Running, JobStatus::Queued],
            "the excess must be recorded as queued, not dropped"
        );

        // One slot frees, and the queued run takes it.
        gate.add_permits(1);
        scheduler.drain().await;

        assert_eq!(runner.finished().len(), 2);
        assert_eq!(store.statuses(), vec![JobStatus::Ok, JobStatus::Ok]);
        assert_eq!(
            runner.dispatched_while_running(),
            2,
            "a queued run must be promoted to running before it is dispatched"
        );
    }

    #[tokio::test]
    async fn a_job_that_has_spent_its_run_budget_is_retired() {
        let store = Arc::new(FakeStore::default());
        let mut job = job("budget", "0 9 * * *", "UTC", at(2026, 3, 4, 12, 0));
        job.max_runs = Some(2);
        job.runs_count = 2;
        seeded(&store, job);
        let clock = Arc::new(ManualClock::at(at(2026, 3, 4, 12, 0)));
        let runner = Recorder::new(&store, None);
        let scheduler = Scheduler::new(
            store.clone(),
            runner.clone(),
            clock,
            config(MissedRunPolicy::RunOnce, 2),
        );

        let report = scheduler.tick().await.unwrap();
        scheduler.drain().await;

        assert_eq!(report.skipped, 1);
        assert!(runner.started().is_empty());
        assert!(!store.job("budget").enabled, "the job was disabled");
        assert!(store.runs().is_empty(), "retiring is not a run");
    }

    #[tokio::test]
    async fn a_disabled_scheduler_fires_nothing() {
        let store = Arc::new(FakeStore::default());
        seeded(
            &store,
            job("daily", "0 9 * * *", "UTC", at(2026, 3, 4, 12, 0)),
        );
        let clock = Arc::new(ManualClock::at(at(2026, 3, 4, 12, 0)));
        let runner = Recorder::new(&store, None);
        let mut settings = config(MissedRunPolicy::RunOnce, 2);
        settings.enabled = false;
        let scheduler = Scheduler::new(store.clone(), runner.clone(), clock, settings);

        assert_eq!(scheduler.tick().await.unwrap(), TickReport::default());
        assert_eq!(scheduler.catch_up().await.unwrap(), TickReport::default());
        scheduler.drain().await;

        assert!(runner.started().is_empty());
        assert!(store.runs().is_empty());
    }

    #[tokio::test]
    async fn a_run_left_in_flight_by_a_crash_is_closed_before_anything_fires() {
        let store = Arc::new(FakeStore::default());
        seeded(
            &store,
            job("daily", "0 9 * * *", "UTC", at(2026, 3, 4, 12, 0)),
        );
        store.runs.lock().unwrap().push(JobRun {
            id: "orphan".to_string(),
            job_id: "daily".to_string(),
            session_id: None,
            started_at: stamp(at(2026, 3, 4, 11, 0)),
            finished_at: None,
            status: JobStatus::Running,
            exit_summary: None,
            output_ref: None,
        });
        let clock = Arc::new(ManualClock::at(at(2026, 3, 4, 12, 0)));
        let runner = Recorder::new(&store, None);
        let scheduler = Scheduler::new(
            store.clone(),
            runner.clone(),
            clock,
            config(MissedRunPolicy::RunOnce, 2),
        );

        assert_eq!(scheduler.reconcile().await.unwrap(), 1);

        assert_eq!(store.statuses(), vec![JobStatus::Failed]);
        let orphan = store
            .runs()
            .into_iter()
            .find(|run| run.id == "orphan")
            .unwrap();
        assert!(orphan.finished_at.is_some());
    }

    #[tokio::test]
    async fn every_fire_recomputes_the_next_occurrence_from_the_completion_time() {
        let store = Arc::new(FakeStore::default());
        seeded(
            &store,
            job("daily", "0 9 * * *", "UTC", at(2026, 3, 4, 9, 0)),
        );
        // Fired a day late, so the completion must land on the 6th, not the 5th.
        let clock = Arc::new(ManualClock::at(at(2026, 3, 5, 10, 0)));
        let runner = Recorder::new(&store, None);
        let scheduler = Scheduler::new(
            store.clone(),
            runner.clone(),
            clock,
            config(MissedRunPolicy::RunOnce, 2),
        );

        scheduler.tick().await.unwrap();
        scheduler.drain().await;

        assert_eq!(
            store.job("daily").next_run_at.as_deref().unwrap(),
            stamp(at(2026, 3, 6, 9, 0))
        );
    }

    #[tokio::test]
    async fn a_reuse_job_adopts_the_session_its_run_reported() {
        let store = Arc::new(FakeStore::default());
        let mut job = job("reuser", "0 9 * * *", "UTC", at(2026, 3, 4, 9, 0));
        job.session_mode = SessionMode::Reuse;
        seeded(&store, job);
        let clock = Arc::new(ManualClock::at(at(2026, 3, 4, 9, 0)));

        struct SessionRunner;
        #[async_trait]
        impl JobRunner for SessionRunner {
            async fn run(&self, _job: &Job, _run_id: &str) -> RunReport {
                RunReport::ok("done").with_session(Some("session-7".to_string()))
            }
        }

        let scheduler = Scheduler::new(
            store.clone(),
            Arc::new(SessionRunner),
            clock,
            config(MissedRunPolicy::RunOnce, 2),
        );

        scheduler.tick().await.unwrap();
        scheduler.drain().await;

        assert_eq!(store.job("reuser").session_id.as_deref(), Some("session-7"));
    }
}
