//! Built-in tools.
//!
//! Risk classes are the security boundary: `ReadOnly` tools run freely, while
//! `Write` and `Execute` are refused unless the approval engine says otherwise.
//! A tool is only registered here once that gate exists, so nothing unguarded
//! can be reached from a model.

pub mod memory;
pub mod patch;
pub mod read_file;
pub mod run_command;
pub mod write_file;

pub use memory::{Recall, Remember};
pub use patch::{ApplyPatch, EditFile, Operation};
pub use read_file::ReadFile;
pub use run_command::RunCommand;
pub use write_file::WriteFile;

use std::sync::Arc;
use std::time::Duration;

use minion_core::tool::ToolRegistry;
use minion_store::Store;

/// Register the tools available at this milestone.
///
/// Every `Write` and `Execute` tool here requires a gate; see
/// [`minion_core::PolicyEngine`]. A registry built without one is only safe for
/// read-only use.
///
/// `store` backs the memory tools. It is the same handle the session persists
/// through, so a fact written by a tool is in the transcript's database.
pub fn default_registry(config: &ToolConfig, store: Arc<Store>) -> ToolRegistry {
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
    registry.register(Recall::new(store));
    registry
}

/// The subset of `[workspace]` and `[exec]` that shapes the tool set.
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
}
