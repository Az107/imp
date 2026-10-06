//! Built-in tools.
//!
//! Risk classes are the security boundary: `ReadOnly` tools run freely, while
//! `Write` and `Execute` are refused unless the approval engine says otherwise.
//! A tool is only registered here once that gate exists, so nothing unguarded
//! can be reached from a model.

pub mod cron;
pub mod http_fetch;
pub mod memory;
pub mod patch;
pub mod read_file;
pub mod run_command;
pub mod write_file;

pub use cron::{CronAdd, CronList, CronRemove, CronTools};
pub use http_fetch::HttpFetch;
pub use memory::{Recall, Remember};
pub use patch::{ApplyPatch, EditFile, Operation};
pub use read_file::ReadFile;
pub use run_command::RunCommand;
pub use write_file::WriteFile;

use std::sync::Arc;
use std::time::Duration;

use minion_core::clock::{Clock, SystemClock};
use minion_core::config::HttpFetchConfig;
use minion_core::tool::ToolRegistry;
use minion_store::Store;

/// What the cron tools need from the process, beyond the store.
#[derive(Clone)]
pub struct CronContext {
    /// Time source for a new job's first occurrence.
    pub clock: Arc<dyn Clock>,
    /// Timezone a job gets when it does not name one.
    pub timezone: String,
}

impl Default for CronContext {
    fn default() -> Self {
        Self {
            clock: Arc::new(SystemClock),
            timezone: "UTC".to_string(),
        }
    }
}

/// Register the tools available at this milestone.
///
/// Every `Write` and `Execute` tool here requires a gate; see
/// [`minion_core::PolicyEngine`]. A registry built without one is only safe for
/// read-only use.
///
/// `store` backs the memory and cron tools. It is the same handle the session
/// persists through, so a fact written by a tool, or a job it creates, is in the
/// transcript's database.
pub fn default_registry(config: &ToolConfig, store: Arc<Store>, cron: CronContext) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(ReadFile::new(config.max_file_bytes));
    registry.register(EditFile::new(config.max_file_bytes));
    registry.register(ApplyPatch::new(config.max_file_bytes));
    registry.register(WriteFile::new(config.max_file_bytes));
    registry.register(RunCommand::new(
        &config.shell,
        config.default_timeout,
        config.max_timeout,
        config.output_cap_bytes,
    ));
    registry.register(Remember::new(store.clone()));
    registry.register(Recall::new(store.clone()));
    registry.register(HttpFetch::new(&config.http_fetch));

    // The cron tools share the session's store, so a job added by the model is
    // in the same database the scheduler ticks over.
    let cron_tools = CronTools {
        store: store.jobs(),
        clock: cron.clock,
        timezone: cron.timezone,
    };
    registry.register(CronAdd::new(cron_tools.clone()));
    registry.register(CronList::new(cron_tools.clone()));
    registry.register(CronRemove::new(cron_tools));
    registry
}

/// The subset of `[workspace]`, `[exec]` and `[http_fetch]` that shapes the
/// tool set.
#[derive(Debug, Clone)]
pub struct ToolConfig {
    /// Largest file a read or write may touch.
    pub max_file_bytes: u64,
    /// Shell used for `run_command`.
    pub shell: String,
    /// Default command timeout.
    pub default_timeout: Duration,
    /// Ceiling on any command timeout.
    pub max_timeout: Duration,
    /// Bytes retained per output stream.
    pub output_cap_bytes: u64,
    /// `[http_fetch]`: the domain allowlist and the SSRF guard's limits.
    pub http_fetch: HttpFetchConfig,
}
