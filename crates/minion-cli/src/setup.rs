//! Session assembly: store, provider, tools, transcript, and the system prompt.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use minion_core::agent::AgentOptions;
use minion_core::config::{Config, Decision};
use minion_core::error::{Error, Result};
use minion_core::message::{Message, Role};
use minion_core::policy::PolicyEngine;
use minion_core::provider::Provider;
use minion_core::tool::ToolRegistry;
use minion_core::{Agent, new_session_id};
use minion_provider::OpenAiProvider;
use minion_store::{NewSession, SessionRow, Store};
use minion_tools::{ToolConfig, default_registry};

use crate::approval;
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
    /// Identifier of the current conversation. Doubles as the `${session}`
    /// value sent to gateways, so resuming keeps the same id.
    pub session_id: String,
    /// Transcript store.
    pub store: Arc<Store>,
    /// How many trailing messages to keep when a transcript is reloaded.
    pub history_window: usize,
    /// Retained so the provider can be rebuilt when the conversation changes.
    spec: ProviderSpec,
    /// The approval gate, shared so session-scoped allows accumulate.
    gate: Arc<PolicyEngine>,
}

/// The parameters a provider was built from, kept to rebuild it later.
struct ProviderSpec {
    base_url: String,
    api_key: Option<String>,
    max_retries: u32,
    request_timeout: Duration,
    usage_in_stream: bool,
    headers: Vec<(String, String)>,
}

impl ProviderSpec {
    fn build(&self, session_id: &str) -> OpenAiProvider {
        OpenAiProvider::new(&self.base_url, self.api_key.clone().unwrap_or_default())
            .with_max_retries(self.max_retries)
            .with_request_timeout(Some(self.request_timeout))
            .with_usage_in_stream(self.usage_in_stream)
            .with_headers(self.headers.clone())
            .with_session_id(session_id.to_string())
    }
}

impl Session {
    /// Build an [`Agent`] over this session's shared pieces.
    pub fn agent(&self) -> Agent {
        Agent::new(
            self.provider.clone(),
            self.tools.clone(),
            self.options.clone(),
        )
        .with_gate(self.gate.clone())
    }

    /// Point this session at a different conversation.
    ///
    /// The provider is rebuilt so the `${session}` header matches the new
    /// conversation; reusing the old one would make two conversations look
    /// identical to the gateway and defeat prompt-cache routing.
    pub fn adopt(&mut self, session_id: &str) {
        self.provider = Arc::new(self.spec.build(session_id));
        self.session_id = session_id.to_string();
    }

    /// Start a fresh conversation in the same session object.
    pub async fn reset(&mut self) -> Result<()> {
        let system = self.history.first().cloned();
        self.adopt(&new_session_id());
        self.store
            .create_session(NewSession {
                id: self.session_id.clone(),
                cwd: self.workspace_root.display().to_string(),
                model: Some(self.options.model.clone()),
                provider: Some(self.spec.base_url.clone()),
            })
            .await?;
        self.history = system.into_iter().collect();
        self.persist_all().await
    }

    /// Persist everything from `start` onwards, and title on first prompt.
    pub async fn persist_since(&self, start: usize) -> Result<()> {
        if start >= self.history.len() {
            return Ok(());
        }
        self.store
            .append_messages(&self.session_id, &self.history[start..])
            .await?;
        if let Some(prompt) = self.first_user_text() {
            self.store
                .title_from_first_prompt(&self.session_id, &prompt)
                .await?;
        }
        Ok(())
    }

    /// Persist the whole transcript, for a freshly created session.
    pub async fn persist_all(&self) -> Result<()> {
        self.store
            .append_messages(&self.session_id, &self.history)
            .await
    }

    /// The text of the first user message, used to derive a title.
    fn first_user_text(&self) -> Option<String> {
        self.history
            .iter()
            .find(|message| message.role == Role::User)
            .and_then(|message| message.content.clone())
    }
}

/// Assemble a session, optionally resuming a stored conversation.
pub async fn build(
    cli: &Cli,
    config: &Config,
    cwd: &Path,
    resume: Option<&str>,
) -> Result<Session> {
    // An empty `api_key_env` means the backend needs no credentials.
    let api_key = config.api_key()?;
    let spec = ProviderSpec {
        base_url: config.provider.base_url.clone(),
        api_key,
        max_retries: config.provider.max_retries,
        request_timeout: Duration::from_secs(config.provider.request_timeout_secs),
        usage_in_stream: config.provider.supports_usage_in_stream,
        headers: config.request_headers(),
    };

    let workspace_root = config.workspace_root(cwd)?;

    // The store is opened before the registry because the memory tools need a
    // handle on it, and the system prompt advertises the registered tools, so
    // the order is store -> tools -> prompt.
    let database = cli.db.clone().unwrap_or_else(|| config.database_path());
    let store = Arc::new(Store::open(&database).await?);

    let tools = Arc::new(default_registry(&tool_config(config), store.clone()));
    let system = Message::system(system_prompt(config, &workspace_root, &tools));

    let (session_id, mut history) = match resume {
        Some(id) => {
            let row = store
                .session(id)
                .await?
                .ok_or_else(|| Error::Config(format!("no session `{id}` in the database")))?;
            let mut history = store.load_messages(id).await?;
            if history.is_empty() {
                history.push(system);
            }
            (row.id, history)
        }
        None => {
            let id = new_session_id();
            store
                .create_session(NewSession {
                    id: id.clone(),
                    cwd: workspace_root.display().to_string(),
                    model: Some(config.provider.model.clone()),
                    provider: Some(config.provider.base_url.clone()),
                })
                .await?;
            (id, vec![system])
        }
    };

    apply_history_window(&mut history, config.agent.history_window);
    let history_window = config.agent.history_window;

    let provider = Arc::new(spec.build(&session_id));
    tracing::debug!(
        session_id = %session_id,
        base_url = %config.provider.base_url,
        history = history.len(),
        "session ready"
    );

    let options = AgentOptions {
        model: config.provider.model.clone(),
        max_iterations: config.agent.max_iterations,
        temperature: config.provider.temperature,
        max_tokens: None,
        parallel_tool_calls: Some(config.provider.parallel_tool_calls),
        include_usage: config.provider.supports_usage_in_stream,
        workspace_root: workspace_root.clone(),
    };

    // Whether anyone can answer a prompt. `--yes` forces the permissive path;
    // a non-TTY still cannot be prompted, so it falls back to `noninteractive`.
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let gate = build_gate(
        config,
        &store,
        &workspace_root.display().to_string(),
        tty,
        cli.yes,
        cli.deny,
    );

    let session = Session {
        provider,
        tools,
        options,
        history,
        workspace_root,
        session_id,
        store,
        history_window,
        spec,
        gate,
    };
    if resume.is_none() {
        session.persist_all().await?;
    }
    Ok(session)
}

/// The `[workspace]` and `[exec]` values that shape the tool set.
fn tool_config(config: &Config) -> ToolConfig {
    ToolConfig {
        max_file_bytes: config.workspace.max_file_bytes,
        shell: config.exec.shell.clone(),
        default_timeout: Duration::from_secs(config.exec.default_timeout_secs),
        max_timeout: Duration::from_secs(config.exec.max_timeout_secs),
        output_cap_bytes: config.exec.output_cap_bytes,
    }
}

/// The approval gate, wired from the policy config and the store.
///
/// `--yes` replaces the default decision rather than the whole engine, so deny
/// rules and the command classifier keep applying: it means "do not ask me",
/// not "do whatever you like".
fn build_gate(
    config: &Config,
    store: &Arc<Store>,
    scope: &str,
    tty: bool,
    yes: bool,
    deny: bool,
) -> Arc<PolicyEngine> {
    let allow = config
        .policy
        .allow
        .iter()
        .map(|rule| (rule.tool.clone(), rule.pattern.clone()))
        .collect();
    let deny_rules = config
        .policy
        .deny
        .iter()
        .map(|rule| (rule.tool.clone(), rule.pattern.clone()))
        .collect();

    let default = if deny {
        Decision::Deny
    } else if yes {
        Decision::Auto
    } else {
        config.policy.default
    };
    // A flag cannot conjure a prompt, so a non-TTY stays fail-closed unless
    // the user explicitly passed --yes.
    let noninteractive = if yes {
        Decision::Auto
    } else if deny {
        Decision::Deny
    } else {
        config.policy.noninteractive
    };

    Arc::new(
        PolicyEngine::new(
            allow,
            deny_rules,
            default,
            noninteractive,
            scope.to_string(),
            tty,
        )
        .with_ui(approval::ui_for(tty))
        .with_store(store.approvals()),
    )
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

/// Drop old turns, keeping the system prompt and whole user→assistant groups.
///
/// Cutting on a message boundary alone can leave an assistant `tool_calls`
/// message whose results were dropped, or a stray `tool` result, and the provider
/// rejects that transcript outright. So the cut is moved forward to the next
/// `user` message.
pub fn apply_history_window(history: &mut Vec<Message>, window: usize) {
    if window == 0 || history.len() <= 1 {
        return;
    }
    // Only the conversation is windowed: index 0 is the system prompt, which is
    // configuration rather than history and is always sent.
    let body = history.len() - 1;
    if body <= window {
        return;
    }

    let target = 1 + (body - window);
    let mut cut = (1..history.len())
        .find(|index| *index >= target && history[*index].role == Role::User)
        .unwrap_or(target);

    // Never keep a tool result whose assistant call was cut away.
    while cut < history.len() && history[cut].role == Role::Tool {
        cut += 1;
    }
    if cut >= history.len() {
        return;
    }
    history.drain(1..cut);
}

/// Every `tool_calls` entry in `history` is answered by a matching result.
///
/// This is the property that actually matters: a transcript that keeps an
/// assistant tool call but drops its result is rejected outright by the
/// provider, and it is easy to produce by trimming on a message boundary.
#[cfg(test)]
fn tool_calls_are_paired(history: &[Message]) -> bool {
    for (index, message) in history.iter().enumerate() {
        let Some(calls) = &message.tool_calls else {
            continue;
        };
        for call in calls {
            let answered = history[index + 1..].iter().any(|later| {
                later.role == Role::Tool && later.tool_call_id.as_deref() == Some(call.id.as_str())
            });
            if !answered {
                return false;
            }
        }
    }
    true
}

/// Render a session for `/sessions`.
pub fn describe_session(row: &SessionRow) -> String {
    let title = row
        .title
        .clone()
        .unwrap_or_else(|| "(untitled)".to_string());
    let model = row.model.clone().unwrap_or_else(|| "-".to_string());
    format!(
        "{}  {}  {}  {}",
        &row.id[..8.min(row.id.len())],
        row.updated_at,
        model,
        title
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::default()
    }

    /// The prompt only reads tool names and descriptions, so a registry over a
    /// throwaway in-memory store is enough. Async because opening the store is.
    async fn tools() -> ToolRegistry {
        let store = Arc::new(Store::open_in_memory().await.expect("in-memory store"));
        default_registry(&tool_config(&Config::default()), store)
    }

    fn conversation() -> Vec<Message> {
        vec![
            Message::system("sys"),
            Message::user("one"),
            Message::assistant("a1"),
            Message::user("two"),
            Message::assistant("a2"),
            Message::user("three"),
            Message::assistant("a3"),
        ]
    }

    #[tokio::test]
    async fn prompt_states_the_workspace_and_every_tool() {
        let tools = tools().await;
        let prompt = system_prompt(&config(), Path::new("/tmp/ws"), &tools);

        assert!(prompt.contains("/tmp/ws"));
        assert!(prompt.contains("read_file"));
        assert!(
            prompt.contains("remember") && prompt.contains("recall"),
            "the memory tools are registered, so the prompt must advertise them"
        );
    }

    #[tokio::test]
    async fn prompt_surfaces_deny_rules() {
        let mut config = config();
        config.policy.deny.push(minion_core::config::DenyRule {
            tool: "run_command".to_string(),
            pattern: "*sudo*".to_string(),
        });

        let prompt = system_prompt(&config, Path::new("/tmp/ws"), &tools().await);

        assert!(prompt.contains("Always refused"));
        assert!(prompt.contains("*sudo*"));
    }

    #[tokio::test]
    async fn a_missing_prompt_file_falls_back_instead_of_failing() {
        let mut config = config();
        config.agent.system_prompt_file = Some("/nonexistent/persona.md".to_string());

        let prompt = system_prompt(&config, Path::new("/tmp/ws"), &tools().await);

        assert!(prompt.contains("minion"));
    }

    #[test]
    fn a_short_history_is_left_alone() {
        let mut history = conversation();
        let before = history.len();

        apply_history_window(&mut history, 100);

        assert_eq!(history.len(), before);
    }

    #[test]
    fn the_system_prompt_survives_trimming() {
        let mut history = conversation();

        apply_history_window(&mut history, 3);

        assert_eq!(history[0].content.as_deref(), Some("sys"));
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn trimming_never_splits_a_turn() {
        let mut history = conversation();

        apply_history_window(&mut history, 4);

        assert_eq!(history[0].role, minion_core::message::Role::System);
        assert!(tool_calls_are_paired(&history));
        assert_eq!(history[1].role, minion_core::message::Role::User);
    }

    #[test]
    fn a_tool_result_is_never_orphaned() {
        use minion_core::message::{FunctionCall, ToolCall};
        let history = vec![
            Message::system("sys"),
            Message::user("one"),
            Message::assistant_with_tool_calls(
                None,
                vec![ToolCall {
                    id: "c1".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "read_file".to_string(),
                        arguments: "{}".to_string(),
                    },
                }],
            ),
            Message::tool_result("c1", "contents"),
            Message::assistant("done"),
        ];

        for window in 1..8 {
            let mut candidate = history.clone();
            apply_history_window(&mut candidate, window);
            assert_eq!(
                candidate[0].content.as_deref(),
                Some("sys"),
                "window {window} dropped the system prompt"
            );
            assert!(
                tool_calls_are_paired(&candidate),
                "window {window} left a tool call unanswered: {:?}",
                candidate.iter().map(|m| m.role).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn a_window_of_zero_keeps_everything() {
        let mut history = conversation();
        let before = history.len();

        apply_history_window(&mut history, 0);

        assert_eq!(history.len(), before, "0 means 'do not trim', not 'empty'");
    }
}
