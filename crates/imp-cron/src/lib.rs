//! The in-process cron scheduler: when a job fires, and what a missed one costs.
//!
//! The scheduler is a task inside `imp`, not an OS crontab entry (D5), so
//! jobs do not run while imp is closed (R6). It is split from the storage
//! layer on purpose: everything here is a decision, and the decisions are what
//! the tests pin down — with an injected clock (§8) and a fake runner, so no
//! case needs a socket, an API key, or a sleep.

pub mod jobs;
pub mod schedule;
pub mod scheduler;

pub use jobs::{CreateJob, create_job, describe, remove_job};
pub use schedule::{expression, next_after, timezone};
pub use scheduler::{JobRunner, RunReport, Scheduler, SchedulerConfig, TickReport, spawn_loop};
