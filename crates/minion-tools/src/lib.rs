//! Built-in tools.
//!
//! Only read-only tools are registered by default. Every write or execute tool
//! is gated behind the approval engine, which lands with milestone M2 — until
//! then they are deliberately absent rather than silently unguarded.

pub mod read_file;

pub use read_file::ReadFile;

use minion_core::tool::ToolRegistry;

/// The tool set for the current milestone: read-only only.
pub fn default_registry(max_file_bytes: u64) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(ReadFile::new(max_file_bytes));
    registry
}
