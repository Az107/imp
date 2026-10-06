//! Job creation and rendering, shared by the `cron_*` tools and the CLI.
//!
//! Both entry points must validate the same way — parse the expression, resolve
//! the timezone, compute the first occurrence, refuse a job that can never fire
//! — so the rules live here rather than in each caller (§5.5: "Cron expression
//! parsed before insert; next run computed; reject if never fires").

use chrono::{DateTime, Utc};

use minion_core::error::{Error, Result};
use minion_core::job::{Job, NewJob, SessionMode, stamp};
use minion_core::{JobStore, new_session_id};

use crate::schedule;

/// A validated request to create a job.
#[derive(Debug, Clone)]
pub struct CreateJob {
    /// Optional unique label.
    pub name: Option<String>,
    /// Five-field cron expression.
    pub schedule: String,
    /// The prompt dispatched on each fire.
    pub prompt: String,
    /// Workspace the run executes in. Defaults to the caller's workspace.
    pub cwd: String,
    /// IANA timezone. Defaults to `[cron].timezone`.
    pub timezone: String,
    /// `new` (default) or `reuse`.
    pub session_mode: SessionMode,
    /// Whether overlapping runs are permitted.
    pub allow_overlap: bool,
    /// Optional run budget.
    pub max_runs: Option<i64>,
}

/// Create a job, computing its first occurrence.
///
/// `now` is passed in rather than read, so a test can place the first run on a
/// virtual clock and the CLI can use the real one.
pub async fn create_job(
    store: &dyn JobStore,
    now: DateTime<Utc>,
    request: CreateJob,
) -> Result<Job> {
    if request.prompt.trim().is_empty() {
        return Err(Error::Config("a job needs a prompt".to_string()));
    }
    if request.cwd.trim().is_empty() {
        return Err(Error::Config("a job needs a working directory".to_string()));
    }
    let zone = schedule::timezone(&request.timezone)?;
    let expression = schedule::expression(&request.schedule)?;
    let next = schedule::next_after_or_error(&expression, zone, now, &request.schedule)?;

    store
        .create_job(NewJob {
            id: new_session_id(),
            name: request
                .name
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty()),
            schedule: request.schedule,
            timezone: request.timezone,
            prompt: request.prompt,
            cwd: request.cwd,
            session_mode: request.session_mode,
            allow_overlap: request.allow_overlap,
            max_runs: request.max_runs,
            next_run_at: Some(stamp(next)),
            created_at: stamp(now),
        })
        .await
}

/// Delete a job by id or name.
pub async fn remove_job(store: &dyn JobStore, key: &str) -> Result<bool> {
    store.remove_job(key.trim()).await
}

/// One line for `minion cron list` and `/cron`.
///
/// The next-run column is rendered in the job's own zone, because that is the
/// zone the expression was written in.
pub fn describe(job: &Job) -> String {
    let next = match (&job.next_run_at, schedule::timezone(&job.timezone)) {
        (Some(raw), Ok(zone)) => match minion_core::parse_stamp(raw) {
            Ok(at) => at
                .with_timezone(&zone)
                .format("%a %Y-%m-%d %H:%M %Z")
                .to_string(),
            Err(_) => raw.clone(),
        },
        (Some(raw), Err(_)) => raw.clone(),
        (None, _) => "never".to_string(),
    };
    format!(
        "{:<20} {:<14} next {:<26} last {:<9} {} session{}",
        job.label(),
        job.schedule,
        next,
        job.last_status.clone().unwrap_or_else(|| "-".to_string()),
        job.session_mode.as_str(),
        if job.enabled { "" } else { " (disabled)" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use minion_core::job::{JobStatus, JobStore, JobUpdate, NewJob, NewJobRun};

    /// A store that only remembers what it was handed.
    #[derive(Default)]
    struct Recorder {
        created: std::sync::Mutex<Vec<NewJob>>,
    }

    #[async_trait::async_trait]
    impl JobStore for Recorder {
        async fn create_job(&self, new: NewJob) -> Result<Job> {
            let job = Job {
                id: new.id.clone(),
                name: new.name.clone(),
                schedule: new.schedule.clone(),
                timezone: new.timezone.clone(),
                prompt: new.prompt.clone(),
                cwd: new.cwd.clone(),
                session_id: None,
                session_mode: new.session_mode,
                enabled: true,
                allow_overlap: new.allow_overlap,
                max_runs: new.max_runs,
                runs_count: 0,
                created_at: new.created_at.clone(),
                last_run_at: None,
                next_run_at: new.next_run_at.clone(),
                last_status: None,
            };
            self.created.lock().unwrap().push(new);
            Ok(job)
        }
        async fn job(&self, _key: &str) -> Result<Option<Job>> {
            Ok(None)
        }
        async fn list_jobs(&self) -> Result<Vec<Job>> {
            Ok(Vec::new())
        }
        async fn remove_job(&self, _key: &str) -> Result<bool> {
            Ok(true)
        }
        async fn due_jobs(&self, _now: DateTime<Utc>) -> Result<Vec<Job>> {
            Ok(Vec::new())
        }
        async fn start_run(&self, _run: NewJobRun, _update: JobUpdate) -> Result<()> {
            Ok(())
        }
        async fn finish_run(
            &self,
            _run_id: &str,
            _status: JobStatus,
            _finished_at: DateTime<Utc>,
            _exit_summary: Option<String>,
            _output_ref: Option<String>,
            _update: JobUpdate,
        ) -> Result<()> {
            Ok(())
        }
        async fn update_job(&self, _job_id: &str, _update: JobUpdate) -> Result<()> {
            Ok(())
        }
        async fn mark_run_running(&self, _run_id: &str) -> Result<()> {
            Ok(())
        }
        async fn list_runs(
            &self,
            _job_id: &str,
            _limit: usize,
        ) -> Result<Vec<minion_core::JobRun>> {
            Ok(Vec::new())
        }
        async fn abandon_open_runs(&self, _now: DateTime<Utc>) -> Result<usize> {
            Ok(0)
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 4, 12, 0, 0).unwrap()
    }

    fn request() -> CreateJob {
        CreateJob {
            name: Some("weekly".to_string()),
            schedule: "0 9 * * 1".to_string(),
            prompt: "publish".to_string(),
            cwd: "/tmp".to_string(),
            timezone: "America/Mexico_City".to_string(),
            session_mode: SessionMode::New,
            allow_overlap: false,
            max_runs: None,
        }
    }

    #[tokio::test]
    async fn creating_a_job_computes_its_first_occurrence_in_its_zone() {
        let store = Recorder::default();

        let job = create_job(&store, now(), request()).await.unwrap();

        let zone = schedule::timezone("America/Mexico_City").unwrap();
        let next = minion_core::parse_stamp(job.next_run_at.as_deref().unwrap()).unwrap();
        assert_eq!(
            next.with_timezone(&zone).to_string(),
            "2026-03-09 09:00:00 CST"
        );
        assert_eq!(job.name.as_deref(), Some("weekly"));
    }

    #[tokio::test]
    async fn a_bad_expression_or_timezone_is_rejected_before_anything_is_stored() {
        let store = Recorder::default();

        let mut bad_expression = request();
        bad_expression.schedule = "every monday".to_string();
        assert!(create_job(&store, now(), bad_expression).await.is_err());

        let mut bad_zone = request();
        bad_zone.timezone = "Mars/Olympus".to_string();
        assert!(create_job(&store, now(), bad_zone).await.is_err());

        let mut no_prompt = request();
        no_prompt.prompt = "  ".to_string();
        assert!(create_job(&store, now(), no_prompt).await.is_err());

        assert!(
            store.created.lock().unwrap().is_empty(),
            "a rejected request must not reach the store"
        );
    }

    #[tokio::test]
    async fn an_expression_that_never_fires_again_is_refused() {
        let store = Recorder::default();
        let mut request = request();
        request.schedule = "0 9 30 2 *".to_string();

        let err = create_job(&store, now(), request).await.unwrap_err();

        assert!(err.to_string().contains("never fire"), "was: {err}");
        assert!(store.created.lock().unwrap().is_empty());
    }

    #[test]
    fn a_job_line_names_its_next_run_in_its_own_zone() {
        let job = Job {
            id: "0123456789abcdef".to_string(),
            name: Some("weekly".to_string()),
            schedule: "0 9 * * 1".to_string(),
            timezone: "America/Mexico_City".to_string(),
            prompt: "publish".to_string(),
            cwd: "/tmp".to_string(),
            session_id: None,
            session_mode: SessionMode::New,
            enabled: true,
            allow_overlap: false,
            max_runs: None,
            runs_count: 0,
            created_at: stamp(now()),
            last_run_at: None,
            next_run_at: Some(stamp(Utc.with_ymd_and_hms(2026, 3, 9, 15, 0, 0).unwrap())),
            last_status: None,
        };

        let line = describe(&job);

        assert!(line.contains("weekly"), "was: {line}");
        assert!(line.contains("Mon 2026-03-09 09:00 CST"), "was: {line}");
        assert!(line.contains("new session"), "was: {line}");
    }
}
