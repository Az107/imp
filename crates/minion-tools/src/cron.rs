//! `cron_add` / `cron_list` / `cron_remove` (FR-11, SDD §5.5).
//!
//! Three tools, two risk classes: creating and removing a job is `Write` and
//! goes through the gate, listing is `ReadOnly` and runs freely. That split is
//! what makes the non-interactive rule do real work here — a cron prompt cannot
//! prompt for approval, so a job can neither create nor delete jobs unless an
//! allow rule says so. It cannot escalate its own reach.
//!
//! Creation goes through the same validation the CLI uses (`minion_cron::create_job`),
//! so a schedule the tool accepts is a schedule the scheduler can honour: parsed
//! before insert, first occurrence computed, and an expression that never fires
//! refused rather than stored as a job that silently does nothing.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use minion_core::clock::Clock;
use minion_core::error::{Error, Result};
use minion_core::job::{Job, JobStore, SessionMode};
use minion_core::tool::{Risk, Tool, ToolCtx, ToolOutput};
use minion_cron::CreateJob;

/// Everything the cron tools need from the running process.
#[derive(Clone)]
pub struct CronTools {
    /// The job table.
    pub store: Arc<dyn JobStore>,
    /// Time source, so the first occurrence can be computed.
    pub clock: Arc<dyn Clock>,
    /// Timezone a job gets when it does not name one.
    pub timezone: String,
}

/// Turn a job into the JSON the model sees.
fn render(job: &Job) -> Value {
    json!({
        "id": job.id,
        "name": job.name,
        "schedule": job.schedule,
        "timezone": job.timezone,
        "session_mode": job.session_mode.as_str(),
        "enabled": job.enabled,
        "allow_overlap": job.allow_overlap,
        "runs_count": job.runs_count,
        "max_runs": job.max_runs,
        "next_run_at": job.next_run_at,
        "last_run_at": job.last_run_at,
        "last_status": job.last_status,
        "prompt": job.prompt,
        "cwd": job.cwd,
    })
}

/// Read a string argument, trimmed, rejecting an empty one.
fn required<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::ToolArgs {
            tool: "cron".to_string(),
            message: format!("`{name}` is required"),
        })
}

/// `cron_add`.
pub struct CronAdd {
    tools: CronTools,
}

impl CronAdd {
    /// Build the tool over a job store and a clock.
    pub fn new(tools: CronTools) -> Self {
        Self { tools }
    }
}

#[async_trait]
impl Tool for CronAdd {
    fn name(&self) -> &'static str {
        "cron_add"
    }

    fn description(&self) -> &'static str {
        "Schedule a prompt to run on a 5-field cron expression. Params: schedule, prompt, \
         name?, cwd?, timezone?, session_mode? (`new`|`reuse`), max_runs?, allow_overlap?."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "schedule": {
                    "type": "string",
                    "description": "5-field cron: minute hour day-of-month month day-of-week. \
                                    Day of week is 0-7 with 0 and 7 meaning Sunday."
                },
                "prompt": { "type": "string", "description": "The prompt to run." },
                "name": { "type": "string", "description": "Optional unique label." },
                "cwd": { "type": "string", "description": "Workspace for the run. Defaults to the current workspace." },
                "timezone": { "type": "string", "description": "IANA timezone. Defaults to the configured one." },
                "session_mode": { "type": "string", "enum": ["new", "reuse"] },
                "max_runs": { "type": "integer", "description": "Stop after this many runs." },
                "allow_overlap": { "type": "boolean", "description": "Allow a run to start while the previous one is still active." }
            },
            "required": ["schedule", "prompt"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    /// The job's name, so a policy allow rule can name one job rather than all of them.
    fn approval_subject(&self, args: &Value) -> Option<String> {
        args.get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .or_else(|| {
                args.get("schedule")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
    }

    async fn invoke(&self, ctx: ToolCtx, args: Value) -> Result<ToolOutput> {
        let schedule = required(&args, "schedule")?.to_string();
        let prompt = required(&args, "prompt")?.to_string();
        let timezone = args
            .get("timezone")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&self.tools.timezone)
            .to_string();
        // A model-supplied `cwd` is a path, so it goes through the same guard
        // every other path-taking tool uses rather than becoming an absolute
        // path the run would execute somewhere unexpected.
        let cwd = match args.get("cwd").and_then(Value::as_str) {
            Some(raw) if !raw.trim().is_empty() => ctx.resolve(raw)?.display().to_string(),
            _ => ctx.workspace_root.display().to_string(),
        };
        let session_mode = match args.get("session_mode").and_then(Value::as_str) {
            Some(raw) => SessionMode::parse(raw)?,
            None => SessionMode::New,
        };

        let job = minion_cron::create_job(
            self.tools.store.as_ref(),
            self.tools.clock.now(),
            CreateJob {
                name: args.get("name").and_then(Value::as_str).map(str::to_string),
                schedule,
                prompt,
                cwd,
                timezone,
                session_mode,
                allow_overlap: args
                    .get("allow_overlap")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                max_runs: args.get("max_runs").and_then(Value::as_i64),
            },
        )
        .await
        .map_err(|err| Error::Tool {
            tool: "cron_add".to_string(),
            message: err.to_string(),
        })?;

        Ok(ToolOutput::json(render(&job)))
    }
}

/// `cron_list`.
pub struct CronList {
    tools: CronTools,
}

impl CronList {
    /// Build the tool over a job store.
    pub fn new(tools: CronTools) -> Self {
        Self { tools }
    }
}

#[async_trait]
impl Tool for CronList {
    fn name(&self) -> &'static str {
        "cron_list"
    }

    fn description(&self) -> &'static str {
        "List scheduled jobs with their next run, last status, and session mode."
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    async fn invoke(&self, _ctx: ToolCtx, _args: Value) -> Result<ToolOutput> {
        let jobs = self
            .tools
            .store
            .list_jobs()
            .await
            .map_err(|err| Error::Tool {
                tool: "cron_list".to_string(),
                message: err.to_string(),
            })?;
        let items: Vec<Value> = jobs.iter().map(render).collect();
        Ok(ToolOutput::json(json!({ "jobs": items })))
    }
}

/// `cron_remove`.
pub struct CronRemove {
    tools: CronTools,
}

impl CronRemove {
    /// Build the tool over a job store.
    pub fn new(tools: CronTools) -> Self {
        Self { tools }
    }
}

#[async_trait]
impl Tool for CronRemove {
    fn name(&self) -> &'static str {
        "cron_remove"
    }

    fn description(&self) -> &'static str {
        "Delete a scheduled job by `id` or `name`."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "name": { "type": "string" }
            }
        })
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn approval_subject(&self, args: &Value) -> Option<String> {
        for field in ["name", "id"] {
            if let Some(value) = args.get(field).and_then(Value::as_str)
                && !value.trim().is_empty()
            {
                return Some(value.trim().to_string());
            }
        }
        None
    }

    async fn invoke(&self, _ctx: ToolCtx, args: Value) -> Result<ToolOutput> {
        let key = args
            .get("id")
            .or_else(|| args.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::ToolArgs {
                tool: "cron_remove".to_string(),
                message: "`id` or `name` is required".to_string(),
            })?;

        let removed = minion_cron::remove_job(self.tools.store.as_ref(), key)
            .await
            .map_err(|err| Error::Tool {
                tool: "cron_remove".to_string(),
                message: err.to_string(),
            })?;

        Ok(ToolOutput::json(json!({ "removed": removed, "key": key })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use minion_core::job::{JobRun, JobStatus, JobUpdate, NewJob, NewJobRun};
    use minion_core::{ManualClock, SystemClock};
    use std::sync::Mutex;

    /// A job store that keeps everything in memory.
    #[derive(Default)]
    struct Memory {
        jobs: Mutex<Vec<Job>>,
    }

    fn apply(jobs: &Mutex<Vec<Job>>, id: &str, update: &JobUpdate) {
        let mut jobs = jobs.lock().unwrap();
        let Some(job) = jobs.iter_mut().find(|job| job.id == id) else {
            return;
        };
        if let Some(next) = &update.next_run_at {
            job.next_run_at = Some(next.clone());
        }
        if let Some(status) = &update.last_status {
            job.last_status = Some(status.clone());
        }
        if update.increment_runs {
            job.runs_count += 1;
        }
    }

    #[async_trait]
    impl JobStore for Memory {
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
        async fn update_job(&self, job_id: &str, update: JobUpdate) -> Result<()> {
            apply(&self.jobs, job_id, &update);
            Ok(())
        }
        async fn mark_run_running(&self, _run_id: &str) -> Result<()> {
            Ok(())
        }
        async fn list_runs(&self, _job_id: &str, _limit: usize) -> Result<Vec<JobRun>> {
            Ok(Vec::new())
        }
        async fn abandon_open_runs(&self, _now: DateTime<Utc>) -> Result<usize> {
            Ok(0)
        }
    }

    fn tools(store: &Arc<Memory>, clock: Arc<dyn Clock>) -> CronTools {
        CronTools {
            store: store.clone(),
            clock,
            timezone: "America/Mexico_City".to_string(),
        }
    }

    fn context() -> ToolCtx {
        ToolCtx {
            workspace_root: std::path::PathBuf::from("/tmp"),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    fn clock() -> Arc<ManualClock> {
        Arc::new(ManualClock::at(
            Utc.with_ymd_and_hms(2026, 3, 4, 12, 0, 0).unwrap(),
        ))
    }

    #[tokio::test]
    async fn cron_add_stores_a_job_and_returns_its_next_run() {
        let store = Arc::new(Memory::default());
        let add = CronAdd::new(tools(&store, clock()));

        let out = add
            .invoke(
                context(),
                json!({ "schedule": "0 9 * * 1", "prompt": "publish", "name": "weekly" }),
            )
            .await
            .unwrap();

        let body: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(body["name"], "weekly");
        assert_eq!(body["session_mode"], "new");
        assert_eq!(
            body["next_run_at"], "2026-03-09T15:00:00.000Z",
            "Monday 09:00 in Mexico City is 15:00 UTC"
        );
        assert_eq!(store.jobs.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cron_add_refuses_a_schedule_that_never_fires() {
        let store = Arc::new(Memory::default());
        let add = CronAdd::new(tools(&store, clock()));

        let err = add
            .invoke(
                context(),
                json!({ "schedule": "0 9 30 2 *", "prompt": "never" }),
            )
            .await
            .unwrap_err();

        assert!(err.to_string().contains("never fire"), "was: {err}");
        assert!(store.jobs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cron_add_defaults_the_timezone_and_takes_a_session_mode() {
        let store = Arc::new(Memory::default());
        let add = CronAdd::new(tools(&store, clock()));

        let out = add
            .invoke(
                context(),
                json!({ "schedule": "* * * * *", "prompt": "tick", "session_mode": "reuse" }),
            )
            .await
            .unwrap();

        let body: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(body["timezone"], "America/Mexico_City");
        assert_eq!(body["session_mode"], "reuse");
        assert_eq!(body["cwd"], "/tmp");
    }

    #[tokio::test]
    async fn cron_list_shows_what_was_added() {
        let store = Arc::new(Memory::default());
        let add = CronAdd::new(tools(&store, clock()));
        add.invoke(
            context(),
            json!({ "schedule": "0 9 * * 1", "prompt": "publish", "name": "weekly" }),
        )
        .await
        .unwrap();

        let list = CronList::new(tools(&store, Arc::new(SystemClock)));
        let out = list.invoke(context(), json!({})).await.unwrap();

        let body: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(body["jobs"].as_array().unwrap().len(), 1);
        assert_eq!(body["jobs"][0]["name"], "weekly");
        assert_eq!(body["jobs"][0]["last_status"], Value::Null);
    }

    #[tokio::test]
    async fn cron_remove_takes_either_the_id_or_the_name() {
        let store = Arc::new(Memory::default());
        let add = CronAdd::new(tools(&store, clock()));
        let out = add
            .invoke(
                context(),
                json!({ "schedule": "0 9 * * 1", "prompt": "publish", "name": "weekly" }),
            )
            .await
            .unwrap();
        let id = serde_json::from_str::<Value>(&out.content).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let remove = CronRemove::new(tools(&store, Arc::new(SystemClock)));
        let by_name = remove
            .invoke(context(), json!({ "name": "weekly" }))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&by_name.content).unwrap()["removed"],
            true
        );
        assert!(store.jobs.lock().unwrap().is_empty());

        let missing = remove.invoke(context(), json!({ "id": id })).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&missing.content).unwrap()["removed"],
            false
        );
        assert!(remove.invoke(context(), json!({})).await.is_err());
    }

    #[tokio::test]
    async fn cron_add_confines_the_job_workspace_to_the_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = Arc::new(Memory::default());
        let add = CronAdd::new(tools(&store, clock()));
        let ctx = || ToolCtx {
            workspace_root: root.clone(),
            cancel: tokio_util::sync::CancellationToken::new(),
        };

        let err = add
            .invoke(
                ctx(),
                json!({ "schedule": "* * * * *", "prompt": "x", "cwd": "/etc" }),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("escapes"), "was: {err}");
        assert!(store.jobs.lock().unwrap().is_empty());

        let out = add
            .invoke(
                ctx(),
                json!({ "schedule": "* * * * *", "prompt": "x", "cwd": "sub" }),
            )
            .await
            .unwrap();
        let body: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(body["cwd"], root.join("sub").display().to_string());
    }

    /// §5.5: creating and removing a job is `Write`, listing is `ReadOnly`.
    #[test]
    fn the_cron_tools_carry_the_risk_classes_the_spec_names() {
        let store = Arc::new(Memory::default());
        let tools = tools(&store, Arc::new(SystemClock));

        assert_eq!(CronAdd::new(tools.clone()).risk(), Risk::Write);
        assert_eq!(CronRemove::new(tools.clone()).risk(), Risk::Write);
        assert_eq!(CronList::new(tools).risk(), Risk::ReadOnly);
    }

    #[test]
    fn a_job_is_named_as_the_subject_so_a_rule_can_single_it_out() {
        let store = Arc::new(Memory::default());
        let add = CronAdd::new(tools(&store, Arc::new(SystemClock)));

        assert_eq!(
            add.approval_subject(&json!({ "name": "weekly", "schedule": "* * * * *" })),
            Some("weekly".to_string())
        );
        assert_eq!(
            add.approval_subject(&json!({ "schedule": "* * * * *" })),
            Some("* * * * *".to_string())
        );
    }
}
