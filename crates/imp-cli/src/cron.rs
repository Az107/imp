//! `imp cron add|list|remove` (§5.12) and the scheduler that runs with a session.
//!
//! Two entry points share this file because they share a store and a set of
//! rules. The subcommand is the operator's; the service is the session's, and it
//! is the same `imp-cron` scheduler either way. Nothing here decides *when* a
//! job fires — that is `imp-cron`'s job — and nothing here decides whether a
//! job may run a tool — that is the policy engine's.

use std::process::ExitCode;
use std::sync::Arc;

use imp_core::clock::{Clock, SystemClock};
use imp_core::config::Config;
use imp_core::error::Result;
use imp_core::job::{Job, SessionMode};
use imp_cron::{
    CreateJob, JobRunner, Scheduler, SchedulerConfig, TickReport, create_job, describe, remove_job,
};
use imp_store::Store;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::cli::{Cli, CronAction, CronArgs};
use crate::style::{self, Glyph, Theme};

/// Run a `imp cron` subcommand.
pub async fn run(cli: &Cli, config: &Config, args: CronArgs) -> Result<ExitCode> {
    let cwd = cli
        .cwd
        .clone()
        .map(|dir| dir.canonicalize().unwrap_or(dir))
        .unwrap_or(std::env::current_dir()?);
    let database = cli.db.clone().unwrap_or_else(|| config.database_path());
    let store = Arc::new(Store::open(&database).await?);
    let jobs = store.jobs();
    let theme = style::stdout_theme(cli, config);

    match args.action {
        CronAction::Add {
            schedule,
            prompt,
            name,
            cwd: job_cwd,
            timezone,
            session_mode,
            max_runs,
            allow_overlap,
        } => {
            let job = create_job(
                jobs.as_ref(),
                SystemClock.now(),
                CreateJob {
                    name,
                    schedule,
                    prompt,
                    cwd: job_cwd
                        .map(|dir| dir.canonicalize().unwrap_or(dir).display().to_string())
                        .unwrap_or_else(|| cwd.display().to_string()),
                    timezone: timezone.unwrap_or_else(|| config.cron.timezone.clone()),
                    session_mode: match session_mode {
                        Some(raw) => SessionMode::parse(&raw)?,
                        None => SessionMode::New,
                    },
                    allow_overlap,
                    max_runs,
                },
            )
            .await?;

            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "type": "job",
                        "id": job.id,
                        "name": job.name,
                        "schedule": job.schedule,
                        "timezone": job.timezone,
                        "session_mode": job.session_mode.as_str(),
                        "next_run_at": job.next_run_at,
                    })
                );
            } else {
                println!("{}", announce(&job, theme));
            }
            Ok(ExitCode::SUCCESS)
        }

        CronAction::List => {
            let jobs = jobs.list_jobs().await?;
            if cli.json {
                for job in &jobs {
                    println!(
                        "{}",
                        serde_json::json!({
                            "type": "job",
                            "id": job.id,
                            "name": job.name,
                            "schedule": job.schedule,
                            "timezone": job.timezone,
                            "session_mode": job.session_mode.as_str(),
                            "enabled": job.enabled,
                            "runs_count": job.runs_count,
                            "next_run_at": job.next_run_at,
                            "last_run_at": job.last_run_at,
                            "last_status": job.last_status,
                        })
                    );
                }
                return Ok(ExitCode::SUCCESS);
            }
            if jobs.is_empty() {
                println!(
                    "{}",
                    theme.dim("No jobs scheduled. Add one with `imp cron add`.")
                );
                return Ok(ExitCode::SUCCESS);
            }
            for job in &jobs {
                println!(
                    "  {} {}",
                    theme.dim(theme.glyph(Glyph::Selected)),
                    describe(job)
                );
            }
            Ok(ExitCode::SUCCESS)
        }

        CronAction::Remove { key } => {
            let removed = remove_job(jobs.as_ref(), &key).await?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({ "type": "job_removed", "key": key, "removed": removed })
                );
            } else if removed {
                println!("{} {}", theme.success("removed"), theme.bold(&key));
            } else {
                println!("{}", theme.warn(&format!("no job matches `{key}`")));
            }
            Ok(if removed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(4)
            })
        }
    }
}

/// One line describing a newly created job, in its own timezone.
fn announce(job: &Job, theme: Theme) -> String {
    let label = job.label();
    let next = match imp_core::parse_stamp(job.next_run_at.as_deref().unwrap_or_default()) {
        Ok(at) => imp_cron::timezone(&job.timezone)
            .map(|zone| {
                at.with_timezone(&zone)
                    .format("%a %Y-%m-%d %H:%M %Z")
                    .to_string()
            })
            .unwrap_or_else(|_| job.next_run_at.clone().unwrap_or_default()),
        Err(_) => "never".to_string(),
    };
    format!(
        "{} \"{}\" — next run {} ({} {}, {} session)",
        theme.success("Created job"),
        theme.bold(&label),
        theme.info(&next),
        job.schedule,
        job.timezone,
        job.session_mode.as_str()
    )
}

/// A scheduler running inside this process.
///
/// Dropping it stops the tick loop, so a session that ends takes the scheduler
/// with it — which is the point of the in-process design (D5): no daemon is left
/// behind, and no job fires for a imp that is not running.
pub struct CronService {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

impl CronService {
    /// Stop ticking, and stop the task with it. Idempotent.
    fn stop(&self) {
        self.cancel.cancel();
        self.handle.abort();
    }
}

impl Drop for CronService {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the tick loop, after repairing what a previous process left behind.
///
/// `None` means `[cron].enabled = false`: no reconciliation, no catch-up, no
/// loop. Returns the scheduler's own report of the startup work so a caller can
/// log it.
pub async fn start(
    config: &Config,
    store: &Arc<Store>,
    runner: Arc<dyn JobRunner>,
) -> Result<Option<(CronService, Startup)>> {
    if !config.cron.enabled {
        return Ok(None);
    }
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let scheduler = Scheduler::new(
        store.jobs(),
        runner,
        clock,
        SchedulerConfig::from(&config.cron),
    );

    // A run left `running` by a process that died is not running now. Closing it
    // first keeps `/cron` honest and stops catch-up from seeing a job as busy.
    let reconciled = scheduler.reconcile().await?;
    let caught_up = scheduler.catch_up().await?;

    let cancel = CancellationToken::new();
    let handle = imp_cron::spawn_loop(scheduler, cancel.clone());
    Ok(Some((
        CronService { cancel, handle },
        Startup {
            reconciled,
            caught_up,
        },
    )))
}

/// What startup did, for a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Startup {
    /// Runs closed because a previous process died mid-flight.
    pub reconciled: usize,
    /// What the catch-up pass decided.
    pub caught_up: TickReport,
}
