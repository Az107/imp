//! Built-in tools.
//!
//! Risk classes are the security boundary: `ReadOnly` tools run freely, while
//! `Write` and `Execute` are refused unless the approval engine says otherwise.
//! A tool is only registered here once that gate exists, so nothing unguarded
//! can be reached from a model.

pub mod patch;
pub mod read_file;
pub mod run_command;
pub mod write_file;

pub use patch::{ApplyPatch, EditFile, Operation};
pub use read_file::ReadFile;
pub use run_command::RunCommand;
pub use write_file::WriteFile;

use std::time::Duration;

use minion_core::tool::ToolRegistry;

/// Register the tools available at this milestone.
///
/// Every `Write` and `Execute` tool here requires a gate; see
/// [`minion_core::PolicyEngine`]. A registry built without one is only safe for
/// read-only use.
pub fn default_registry(config: &ToolConfig) -> ToolRegistry {
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
