//! Session assembly: provider, tools, options, and the system prompt.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use minion_core::agent::AgentOptions;
use minion_core::config::Config;
use minion_core::message::Message;
use minion_core::provider::Provider;
use minion_core::tool::ToolRegistry;
use minion_core::{Agent, Result};
use minion_provider::OpenAiProvider;
use minion_tools::default_registry;

use crate::cli::Cli;

/// Everything one interactive or one-shot run needs.
pub struct Session {
    /// Model backend, shared with every [`Agent`] built from this session.
    pub provider: Arc<dyn Provider>,
    /// Tools offered to the model.
    pub tools: Arc<ToolRegistry>,
    /// Loop settings. `model` is mutable so `/model` can retarget a session.
    pub options: AgentOptions,
    /// Conversation so far. Index 0 is always the system prompt.
    pub history: Vec<Message>,
    /// Canonical workspace root.
    pub workspace_root: PathBuf,
    /// Stable identifier for this conversation, sent to gateways that need it.
    pub session_id: String,
}

impl Session {
    /// Build an [`Agent`] over this session's shared pieces.
    pub fn agent(&self) -> Agent {
        Agent::new(
            self.provider.clone(),
            self.tools.clone(),
            self.options.clone(),
        )
    }
}

/// Assemble a session from resolved configuration.
pub fn build(cli: &Cli, config: &Config, cwd: &Path) -> Result<Session> {
    // An empty `api_key_env` means the backend needs no credentials.
    let api_key = config.api_key()?.unwrap_or_default();
    let session_id = minion_core::new_session_id();

    let provider = OpenAiProvider::new(&config.provider.base_url, api_key)
        .with_max_retries(config.provider.max_retries)
        .with_request_timeout(Some(Duration::from_secs(
            config.provider.request_timeout_secs,
        )))
        .with_usage_in_stream(config.provider.supports_usage_in_stream)
        .with_headers(config.request_headers())
        .with_session_id(session_id.clone());

    tracing::debug!(
        session_id = %session_id,
        base_url = %config.provider.base_url,
        headers = config.provider.headers.len(),
        "session started"
    );

    let workspace_root = config.workspace_root(cwd)?;
    let tools = Arc::new(default_registry(config.workspace.max_file_bytes));
    let history = vec![Message::system(system_prompt(
        config,
        &workspace_root,
        &tools,
    ))];

    let options = AgentOptions {
        model: config.provider.model.clone(),
        max_iterations: config.agent.max_iterations,
        temperature: config.provider.temperature,
        max_tokens: None,
        parallel_tool_calls: Some(config.provider.parallel_tool_calls),
        include_usage: config.provider.supports_usage_in_stream,
        workspace_root: workspace_root.clone(),
    };

    let _ = cli;
    Ok(Session {
        provider: Arc::new(provider),
        tools,
        options,
        history,
        workspace_root,
        session_id,
    })
}

/// The system prompt: persona plus a digest of what the agent is allowed to do.
///
/// Stating the boundary in the prompt reduces wasted tool calls against denied
/// paths, but the enforcement lives in the tools and the approval engine — the
/// prompt is a hint, never a control.
pub fn system_prompt(config: &Config, root: &Path, tools: &ToolRegistry) -> String {
    let persona = match &config.agent.system_prompt_file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(path, error = %err, "cannot read system_prompt_file; using the built-in persona");
                builtin_persona()
            }
        },
        None => builtin_persona(),
    };

    let mode = if std::io::stdin().is_terminal() {
        "interactive (you may ask for approval)"
    } else {
        "non-interactive (approval is unavailable; actions that need consent will be refused)"
    };

    let mut prompt = persona;
    prompt.push_str(&format!(
        "\n\nWorkspace root: {} (read-write)",
        root.display()
    ));
    prompt.push_str(&format!("\nMode: {mode}"));

    prompt.push_str("\n\nAvailable tools:");
    for (name, risk) in tools.risks() {
        prompt.push_str(&format!("\n- {name} ({})", risk.as_str()));
    }

    if !config.policy.deny.is_empty() {
        prompt.push_str("\n\nAlways refused:");
        for rule in &config.policy.deny {
            prompt.push_str(&format!("\n- {} matching `{}`", rule.tool, rule.pattern));
        }
    }

    prompt
}

fn builtin_persona() -> String {
    "You are minion, a minimalist Unix-native agent. Work in small, verifiable steps, \
     prefer tools over guesses, and report what you actually observed. \
     When a task needs a capability you do not have, say so plainly."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::default()
    }

    #[test]
    fn prompt_states_the_workspace_and_every_tool() {
        let tools = default_registry(1024);
        let prompt = system_prompt(&config(), Path::new("/tmp/ws"), &tools);

        assert!(prompt.contains("/tmp/ws"));
        assert!(prompt.contains("read_file"));
    }

    #[test]
    fn prompt_surfaces_deny_rules() {
        let mut config = config();
        config.policy.deny.push(minion_core::config::DenyRule {
            tool: "run_command".to_string(),
            pattern: "*sudo*".to_string(),
        });

        let prompt = system_prompt(&config, Path::new("/tmp/ws"), &default_registry(1024));

        assert!(prompt.contains("Always refused"));
        assert!(prompt.contains("*sudo*"));
    }

    #[test]
    fn a_missing_prompt_file_falls_back_instead_of_failing() {
        let mut config = config();
        config.agent.system_prompt_file = Some("/nonexistent/persona.md".to_string());

        let prompt = system_prompt(&config, Path::new("/tmp/ws"), &default_registry(1024));

        assert!(prompt.contains("minion"));
    }
}
