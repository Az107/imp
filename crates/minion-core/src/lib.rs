//! Core agent loop, message model, configuration, and tool abstractions.
//!
//! This crate deliberately has no terminal, network, or database dependencies so
//! that the loop can be exercised against a scripted mock provider.

pub mod agent;
pub mod classify;
pub mod config;
pub mod error;
pub mod fs;
pub mod glob;
pub mod memory;
pub mod message;
pub mod policy;
pub mod provider;
pub mod session;
pub mod tool;

pub use agent::{Agent, AgentEvent, AgentOptions, StopReason, TurnOutcome};
pub use classify::classify_command;
pub use config::{Config, Credentials, HttpFetchConfig};
pub use error::{Error, Result};
pub use fs::write_private_file;
pub use glob::glob_match;
pub use memory::namespace_for;
pub use message::{Message, Role, ToolCall};
pub use policy::{
    ApprovalChoice, ApprovalRequest, ApprovalStore, ApprovalUi, PolicyEngine, ToolGate,
};
pub use provider::{ChatEvent, ChatRequest, FinishReason, Provider, ToolSchema, Usage};
pub use session::new_session_id;
pub use tool::{Risk, Tool, ToolCtx, ToolOutput, ToolRegistry};
