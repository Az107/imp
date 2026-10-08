//! The scheduler against the real SQLite store (M4 acceptance).
//!
//! The unit tests in `scheduler.rs` drive the decisions through an in-memory
//! fake, which is the right tool for pinning rules. What they cannot show is
//! that the record a run leaves behind is the one §5.7 promises: a `job_runs`
//! row present before dispatch, a terminal status afterwards, and a
//! `next_run_at` recomputed on the same table. That is what this file checks,
//! through `imp-store` and a real database file, so a reopened handle sees
//! exactly what the previous process wrote.
//!
//! No network: the runner is local.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};
use imp_core::config::MissedRunPolicy;
use imp_core::job::{Job, JobStatus, SessionMode, stamp};
use imp_core::{JobStore, ManualClock};
use imp_cron::{CreateJob, JobRunner, RunReport, Scheduler, SchedulerConfig, create_job};
use imp_store::Store;
use tempfile::TempDir;

fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
}

/// A runner that checks the store before it "works", and reports a session.
struct Probe {
    store: Arc<dyn JobStore>,
    ran: Mutex<Vec<String>>,
    durable: AtomicBool,
}

impl Probe {
    fn new(store: Arc<dyn JobStore>) -> Arc<Self> {
        Arc::new(Self {
            store,
            ran: Mutex::new(Vec::new()),
            durable: AtomicBool::new(true),
        })
    }
}

#[async_trait]
impl JobRunner for Probe {
    async fn run(&self, job: &Job, run_id: &str) -> RunReport {
        let runs = self.store.list_runs(&job.id, 10).await.unwrap_or_default();
        let mine = runs.iter().find(|run| run.id == run_id);
        if !mine.is_some_and(|run| run.status == JobStatus::Running) {
            self.durable.store(false, Ordering::SeqCst);
        }
        self.ran.lock().unwrap().push(job.label());
        RunReport::ok("probe finished")
            .with_output_ref(Some(format!("run:{run_id}")))
            .with_session(Some("session-1".to_string()))
    }
}

fn settings(policy: MissedRunPolicy) -> SchedulerConfig {
    SchedulerConfig {
        enabled: true,
        missed_run_policy: policy,
        max_concurrent_jobs: 2,
        missed_run_cap: 20,
    }
}

async fn database(temp: &TempDir) -> Arc<Store> {
    Arc::new(Store::open(&temp.path().join("imp.db")).await.unwrap())
}

/// Add a job the way the CLI does, through the shared validation path.
async fn open_job(store: &Arc<Store>, schedule: &str, first: DateTime<Utc>) -> Job {
    // Force the first occurrence to `first` by creating the job at the instant
    // before it, so the test controls the schedule without touching the table.
    create_job(
        store.jobs().as_ref(),
        first - Duration::seconds(1),
        CreateJob {
            name: Some("probe".to_string()),
            schedule: schedule.to_string(),
            prompt: "say hello".to_string(),
            cwd: "/tmp".to_string(),
            timezone: "UTC".to_string(),
            session_mode: SessionMode::New,
            allow_overlap: false,
            max_runs: None,
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_fire_leaves_a_terminal_run_row_and_a_recomputed_next_run() {
    let temp = tempfile::tempdir().unwrap();
    let store = database(&temp).await;
    let job = open_job(&store, "0 9 * * 1", at(2026, 3, 9, 9, 0)).await;
    assert_eq!(
        job.next_run_at.as_deref(),
        Some(stamp(at(2026, 3, 9, 9, 0)).as_str())
    );

    let clock = Arc::new(ManualClock::at(at(2026, 3, 9, 9, 0)));
    let jobs = store.jobs();
    let probe = Probe::new(jobs.clone());
    let scheduler = Scheduler::new(
        jobs.clone(),
        probe.clone(),
        clock,
        settings(MissedRunPolicy::RunOnce),
    );

    assert_eq!(scheduler.tick().await.unwrap().fired, 1);
    scheduler.drain().await;

    assert!(
        probe.durable.load(Ordering::SeqCst),
        "the job_runs row must be `running` before the prompt is dispatched (NFR-6)"
    );
    assert_eq!(probe.ran.lock().unwrap().as_slice(), ["probe"]);

    let runs = jobs.list_runs(&job.id, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, JobStatus::Ok);
    assert!(runs[0].finished_at.is_some(), "a terminal run has an end");
    assert_eq!(runs[0].exit_summary.as_deref(), Some("probe finished"));

    let reloaded = jobs.job("probe").await.unwrap().unwrap();
    assert_eq!(
        reloaded.next_run_at.as_deref(),
        Some(stamp(at(2026, 3, 16, 9, 0)).as_str()),
        "the next occurrence is recomputed on completion"
    );
    assert_eq!(reloaded.last_status.as_deref(), Some("ok"));
    assert_eq!(reloaded.runs_count, 1);
}

#[tokio::test]
async fn catch_up_runs_a_job_missed_while_the_process_was_closed() {
    let temp = tempfile::tempdir().unwrap();

    // The previous process wrote the job and closed. Its `next_run_at` is now in
    // the past, which is exactly the state the startup path has to repair.
    let first_run_at;
    {
        let store = database(&temp).await;
        let job = open_job(&store, "0 9 * * *", at(2026, 3, 4, 9, 0)).await;
        first_run_at = job.next_run_at.clone().unwrap();
    }

    // A new process, three days later, over the same file.
    let store = database(&temp).await;
    let jobs = store.jobs();
    let clock = Arc::new(ManualClock::at(at(2026, 3, 7, 12, 0)));
    let probe = Probe::new(jobs.clone());
    let scheduler = Scheduler::new(
        jobs.clone(),
        probe.clone(),
        clock,
        settings(MissedRunPolicy::RunOnce),
    );

    assert_eq!(
        scheduler.reconcile().await.unwrap(),
        0,
        "nothing was in flight"
    );
    let report = scheduler.catch_up().await.unwrap();
    scheduler.drain().await;

    assert_eq!(
        report.fired, 1,
        "run_once runs the missed occurrence a single time"
    );
    assert_eq!(probe.ran.lock().unwrap().len(), 1);

    let job = jobs.job("probe").await.unwrap().unwrap();
    assert_eq!(
        job.next_run_at.as_deref(),
        Some(stamp(at(2026, 3, 8, 9, 0)).as_str()),
        "the schedule resumes after now"
    );
    assert_ne!(job.next_run_at.as_deref(), Some(first_run_at.as_str()));
    let runs = jobs.list_runs(&job.id, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, JobStatus::Ok);
}

#[tokio::test]
async fn a_reuse_job_appends_to_the_same_session_across_runs() {
    let temp = tempfile::tempdir().unwrap();
    let store = database(&temp).await;
    let job = create_job(
        store.jobs().as_ref(),
        at(2026, 3, 4, 8, 0),
        CreateJob {
            name: Some("reuser".to_string()),
            schedule: "0 9 * * *".to_string(),
            prompt: "append to the log".to_string(),
            cwd: "/tmp".to_string(),
            timezone: "UTC".to_string(),
            session_mode: SessionMode::Reuse,
            allow_overlap: false,
            max_runs: None,
        },
    )
    .await
    .unwrap();

    let clock = Arc::new(ManualClock::at(at(2026, 3, 4, 9, 0)));
    let jobs = store.jobs();
    // The runner's session has to exist: `jobs.session_id` is a foreign key, and
    // the CLI runner creates the session before it reports the id.
    store
        .create_session(imp_store::NewSession {
            id: "session-1".to_string(),
            cwd: "/tmp".to_string(),
            model: None,
            provider: None,
        })
        .await
        .unwrap();
    let probe = Probe::new(jobs.clone());
    let scheduler = Scheduler::new(
        jobs.clone(),
        probe.clone(),
        clock.clone(),
        settings(MissedRunPolicy::RunOnce),
    );

    scheduler.tick().await.unwrap();
    scheduler.drain().await;
    // The first run reported a session, and the job adopts it.
    assert_eq!(
        jobs.job(&job.id)
            .await
            .unwrap()
            .unwrap()
            .session_id
            .as_deref(),
        Some("session-1")
    );

    // A second run, and it keeps the same session.
    clock.set(at(2026, 3, 5, 9, 0));
    scheduler.tick().await.unwrap();
    scheduler.drain().await;

    assert_eq!(probe.ran.lock().unwrap().len(), 2);
    assert_eq!(
        jobs.job(&job.id)
            .await
            .unwrap()
            .unwrap()
            .session_id
            .as_deref(),
        Some("session-1"),
        "a reuse job accumulates in one session"
    );
    assert_eq!(jobs.list_runs(&job.id, 10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_reuse_job_whose_session_is_missing_still_records_its_run() {
    let temp = tempfile::tempdir().unwrap();
    let store = database(&temp).await;
    let job = create_job(
        store.jobs().as_ref(),
        at(2026, 3, 4, 8, 0),
        CreateJob {
            name: Some("ghost".to_string()),
            schedule: "0 9 * * *".to_string(),
            prompt: "append".to_string(),
            cwd: "/tmp".to_string(),
            timezone: "UTC".to_string(),
            session_mode: SessionMode::Reuse,
            allow_overlap: false,
            max_runs: None,
        },
    )
    .await
    .unwrap();

    let clock = Arc::new(ManualClock::at(at(2026, 3, 4, 9, 0)));
    let jobs = store.jobs();
    // A runner that names a session the store does not have. The adoption must
    // fail without taking the run's terminal status with it.
    let probe = Probe::new(jobs.clone());
    let scheduler = Scheduler::new(
        jobs.clone(),
        probe,
        clock,
        settings(MissedRunPolicy::RunOnce),
    );

    scheduler.tick().await.unwrap();
    scheduler.drain().await;

    let runs = jobs.list_runs(&job.id, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, JobStatus::Ok, "the run is not left running");
    assert!(runs[0].finished_at.is_some());
    assert!(
        jobs.job(&job.id)
            .await
            .unwrap()
            .unwrap()
            .session_id
            .is_none()
    );
}

#[tokio::test]
async fn removing_a_job_from_the_cli_path_takes_its_history_with_it() {
    let temp = tempfile::tempdir().unwrap();
    let store = database(&temp).await;
    let job = open_job(&store, "0 9 * * *", at(2026, 3, 4, 9, 0)).await;

    let clock = Arc::new(ManualClock::at(at(2026, 3, 4, 9, 0)));
    let jobs = store.jobs();
    let probe = Probe::new(jobs.clone());
    let scheduler = Scheduler::new(
        jobs.clone(),
        probe.clone(),
        clock,
        settings(MissedRunPolicy::RunOnce),
    );
    scheduler.tick().await.unwrap();
    scheduler.drain().await;
    assert_eq!(jobs.list_runs(&job.id, 10).await.unwrap().len(), 1);

    assert!(imp_cron::remove_job(jobs.as_ref(), "probe").await.unwrap());

    assert!(jobs.job(&job.id).await.unwrap().is_none());
    assert!(jobs.list_runs(&job.id, 10).await.unwrap().is_empty());
    assert!(jobs.list_jobs().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_overlap_is_recorded_on_a_real_job_row() {
    let temp = tempfile::tempdir().unwrap();
    let store = database(&temp).await;
    // A per-minute job, with the first occurrence at 09:00.
    let first = at(2026, 3, 4, 9, 0);
    let job = create_job(
        store.jobs().as_ref(),
        first - Duration::seconds(1),
        CreateJob {
            name: Some("minutely".to_string()),
            schedule: "* * * * *".to_string(),
            prompt: "tick".to_string(),
            cwd: "/tmp".to_string(),
            timezone: "UTC".to_string(),
            session_mode: SessionMode::New,
            allow_overlap: false,
            max_runs: None,
        },
    )
    .await
    .unwrap();

    let clock = Arc::new(ManualClock::at(at(2026, 3, 4, 9, 0)));
    let jobs = store.jobs();
    // A runner that never returns, so the first run stays in flight.
    struct Hold;
    #[async_trait]
    impl JobRunner for Hold {
        async fn run(&self, _job: &Job, _run_id: &str) -> RunReport {
            std::future::pending::<()>().await;
            RunReport::ok("never")
        }
    }
    let scheduler = Scheduler::new(
        jobs.clone(),
        Arc::new(Hold),
        clock.clone(),
        settings(MissedRunPolicy::RunOnce),
    );

    assert_eq!(scheduler.tick().await.unwrap().fired, 1);

    clock.set(at(2026, 3, 4, 9, 1));
    let report = scheduler.tick().await.unwrap();

    assert_eq!(report.overlapped, 1);
    let runs = jobs.list_runs(&job.id, 10).await.unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].status, JobStatus::Overlap);
    assert!(
        runs[0]
            .exit_summary
            .as_deref()
            .unwrap_or_default()
            .contains("overlap"),
        "the row must say why it was skipped"
    );
    // The run that is still in flight keeps the job busy, and the leak is on
    // purpose: the test ends without draining a task that never finishes.
    assert_eq!(scheduler.running_count(), 1);
}
