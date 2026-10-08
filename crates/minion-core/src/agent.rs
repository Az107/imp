//! The turn state machine: model ⇄ tools until the model stops asking.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::args::parse_tool_arguments;
use crate::error::{Error, Result};
use crate::message::{FunctionCall, Message, Role, ToolCall};
use crate::policy::ToolGate;
use crate::provider::{ChatEvent, ChatRequest, FinishReason, Provider, ToolSchema, Usage};
use crate::tokens::{estimate_message, estimate_messages, estimate_tools};
use crate::tool::{ToolCtx, ToolOutput, ToolRegistry};

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The model produced a final answer.
    Completed,
    /// `max_iterations` was reached before the model settled.
    IterationLimit,
    /// `max_tool_calls_per_turn` was reached.
    ///
    /// Distinct from [`IterationLimit`](Self::IterationLimit): the loop stopped
    /// because the model kept reaching for tools, not because it kept talking.
    ToolBudget,
    /// The caller cancelled the turn.
    Cancelled,
    /// The provider stream failed.
    ProviderError,
}

impl StopReason {
    /// Stable machine-readable name, emitted by `--json` and stored with runs.
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Completed => "completed",
            StopReason::IterationLimit => "iteration_limit",
            StopReason::ToolBudget => "tool_budget",
            StopReason::Cancelled => "cancelled",
            StopReason::ProviderError => "provider_error",
        }
    }
}

/// The result of one turn.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    /// Last assistant text seen, if any.
    pub text: Option<String>,
    /// Why the turn ended.
    pub stop: StopReason,
    /// Token usage summed across the turn's provider calls.
    pub usage: Usage,
    /// How many provider round-trips the turn took.
    pub iterations: u32,
}

/// Progress reported while a turn runs, for rendering and for the audit trail.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A fragment of assistant text.
    TextDelta(String),
    /// A tool is about to run.
    ToolStarted {
        /// Tool name.
        name: String,
        /// Raw JSON arguments.
        arguments: String,
    },
    /// A tool finished, successfully or not.
    ToolFinished {
        /// Tool name.
        name: String,
        /// Whether the tool returned a result rather than an error.
        ok: bool,
        /// One-line summary for display.
        summary: String,
    },
    /// The turn is ending because of a provider failure.
    Failed(String),
    /// Something the loop handled or noticed that is not part of the answer.
    ///
    /// Used for the anomalies a weak model provokes — a tool call cut off by the
    /// token limit, a tool budget running out — so the user sees what happened
    /// without it being mistaken for the model's own words.
    Notice(String),
}

/// Immutable settings for a single [`Agent`].
#[derive(Debug, Clone, Default)]
pub struct AgentOptions {
    /// Model identifier sent to the provider.
    pub model: String,
    /// Maximum provider round-trips per turn.
    pub max_iterations: u32,
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Upper bound on generated tokens.
    pub max_tokens: Option<u32>,
    /// Whether to ask for parallel tool calls.
    pub parallel_tool_calls: Option<bool>,
    /// Whether to request usage on the final stream chunk.
    pub include_usage: bool,
    /// Maximum tool calls the model may make within one turn. `0` is unlimited.
    pub max_tool_calls_per_turn: u32,
    /// Token budget for the whole request. `None` disables trimming.
    ///
    /// Covers the system prompt, history and reserved room for the tool
    /// schemas. Enforced before every provider call, because tool results
    /// accumulate between iterations.
    pub context_tokens: Option<usize>,
    /// Largest number of characters of any one tool result sent to the model.
    pub tool_result_chars: Option<usize>,
    /// Ask an empty reply for one more chance instead of ending the turn.
    pub nudge_on_empty: bool,
    /// Repair fenced or trailing-comma tool arguments before giving up.
    pub repair_arguments: bool,
    /// Boundary for every path-taking tool.
    pub workspace_root: PathBuf,
}

/// Drives a conversation against a [`Provider`] and a [`ToolRegistry`].
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    options: AgentOptions,
    gate: Option<Arc<dyn ToolGate>>,
    /// Conversation id, so an auditing gate can stamp the rows it writes.
    session_id: Option<String>,
}

impl Agent {
    /// Build an agent.
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
        options: AgentOptions,
    ) -> Self {
        Self {
            provider,
            tools,
            options,
            gate: None,
            session_id: None,
        }
    }

    /// Install the approval gate every tool call must pass.
    ///
    /// Without one, `Write` and `Execute` tools run unchecked, so any caller
    /// registering them is expected to set this.
    pub fn with_gate(mut self, gate: Arc<dyn ToolGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Name the conversation this agent works on.
    ///
    /// The gate is told this at the start of each turn so an audit trail can
    /// record the session a decision belonged to (§7).
    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Tools advertised to the model.
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Run one turn, appending the assistant and tool messages to `history`.
    ///
    /// The caller is responsible for pushing the user (or cron) message onto
    /// `history` before calling and for pushing any system prompt.
    pub async fn run(
        &self,
        history: &mut Vec<Message>,
        sink: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> TurnOutcome {
        let mut usage = Usage::default();
        let mut iterations = 0u32;
        let mut last_text: Option<String> = None;
        // Answers already produced this turn, keyed by `name` + canonical
        // arguments. A weak model repeats the same `read_file` or `grep` three
        // and four times; this answers the repeat from the first result instead
        // of running it again. Cleared whenever a state-changing call runs, so a
        // read is never answered from before a mutation.
        let mut answered: HashMap<String, (bool, String)> = HashMap::new();
        let mut tool_calls_used: u32 = 0;
        let budget = self.options.max_tool_calls_per_turn;
        // Whether the one bounded nudge for an empty reply has been spent.
        let mut nudged = false;

        // One turn id per `run`, so every audit row this turn writes can be
        // grouped back to it. Generated before the loop so a cancelled turn
        // still has one.
        let turn_id = crate::new_session_id();
        if let (Some(gate), Some(session_id)) = (&self.gate, &self.session_id) {
            gate.begin_turn(session_id, &turn_id).await;
        }

        loop {
            if cancel.is_cancelled() {
                return outcome(last_text, StopReason::Cancelled, usage, iterations);
            }
            if iterations >= self.options.max_iterations {
                return outcome(last_text, StopReason::IterationLimit, usage, iterations);
            }
            iterations += 1;

            // The catalogue is read live, so the schemas are fetched once per
            // iteration and reused for both budgeting and the request.
            let schemas = self.tools.schemas();
            if let Some(context) = self.options.context_tokens
                && let Some(message) = self.fit_context(history, &schemas, context)
            {
                let _ = sink.send(AgentEvent::Failed(message));
                return outcome(last_text, StopReason::ProviderError, usage, iterations);
            }

            let request = ChatRequest {
                model: self.options.model.clone(),
                messages: history.clone(),
                tools: schemas,
                temperature: self.options.temperature,
                max_tokens: self.options.max_tokens,
                parallel_tool_calls: self.options.parallel_tool_calls,
                include_usage: self.options.include_usage,
            };

            let mut stream = self.provider.stream(request, cancel.clone());
            let mut text = String::new();
            let mut calls: BTreeMap<usize, PartialCall> = BTreeMap::new();
            let mut failure: Option<String> = None;
            let mut finish: Option<FinishReason> = None;

            while let Some(event) = stream.next().await {
                match event {
                    Ok(ChatEvent::TextDelta(delta)) => {
                        text.push_str(&delta);
                        let _ = sink.send(AgentEvent::TextDelta(delta));
                    }
                    Ok(ChatEvent::ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments,
                    }) => {
                        let entry = calls.entry(index).or_default();
                        if let Some(id) = id {
                            entry.id = Some(id);
                        }
                        if let Some(name) = name {
                            entry.name = Some(name);
                        }
                        entry.arguments.push_str(&arguments);
                    }
                    Ok(ChatEvent::Usage(reported)) => usage.absorb(reported),
                    Ok(ChatEvent::Done { finish_reason }) => finish = Some(finish_reason),
                    Err(err) => {
                        failure = Some(err.to_string());
                        break;
                    }
                }
            }

            if let Some(err) = failure {
                let _ = sink.send(AgentEvent::Failed(err));
                return outcome(last_text, StopReason::ProviderError, usage, iterations);
            }
            if cancel.is_cancelled() {
                return outcome(last_text, StopReason::Cancelled, usage, iterations);
            }

            if calls.is_empty() {
                if finish == Some(FinishReason::Length) {
                    let _ = sink.send(AgentEvent::Notice(TRUNCATED_REPLY.to_string()));
                }
                // A reply with neither text nor a tool call is a stall. Give a
                // weak model one bounded second chance before ending the turn.
                if text.is_empty() && self.options.nudge_on_empty && !nudged {
                    nudged = true;
                    let _ = sink.send(AgentEvent::Notice(
                        "the model returned nothing; asking once more".to_string(),
                    ));
                    history.push(Message::user(NUDGE));
                    continue;
                }
                if !text.is_empty() {
                    last_text = Some(text.clone());
                }
                history.push(Message::assistant(text));
                return outcome(last_text, StopReason::Completed, usage, iterations);
            }

            // A turn cut off by the token limit stopped part-way through a tool
            // call's arguments, so the JSON is half an object. Parsing it would
            // only produce a made-up error the model cannot act on, and running
            // it would run a call the model never finished asking for. Drop the
            // whole batch, say so, and ask for a shorter answer (M10.3).
            if finish == Some(FinishReason::Length) {
                if !text.is_empty() {
                    last_text = Some(text.clone());
                    history.push(Message::assistant(text));
                }
                let _ = sink.send(AgentEvent::Notice(TRUNCATED_TOOL_CALL.to_string()));
                history.push(Message::system(TRUNCATED_TOOL_CALL));
                continue;
            }

            let tool_calls: Vec<ToolCall> = calls
                .into_iter()
                .map(|(index, call)| call.finish(index))
                .collect();
            let content = (!text.is_empty()).then(|| text.clone());
            if !text.is_empty() {
                last_text = Some(text.clone());
            }
            history.push(Message::assistant_with_tool_calls(
                content,
                tool_calls.clone(),
            ));

            // TODO(FR-8): run independent calls concurrently with a bounded
            // JoinSet once approval prompting lands, since prompts must be
            // serialized against the terminal.
            let mut exhausted = false;
            for call in tool_calls {
                let name = call.function.name.clone();
                let arguments = call.function.arguments.clone();

                // The budget counts every call the model asked for, a repeat
                // included: it bounds a runaway loop, not merely its cost. The
                // refused call is still *answered*, so the stored transcript
                // keeps every `tool_calls` id paired with a result.
                if budget != 0 && tool_calls_used >= budget {
                    exhausted = true;
                    let _ = sink.send(AgentEvent::ToolFinished {
                        name: name.clone(),
                        ok: false,
                        summary: BUDGET_EXHAUSTED_SUMMARY.to_string(),
                    });
                    history.push(Message::tool_result(call.id.clone(), budget_payload()));
                    continue;
                }
                tool_calls_used += 1;

                let key = call_key(&name, &arguments);
                if let Some((ok, content)) = answered.get(&key) {
                    let _ = sink.send(AgentEvent::ToolFinished {
                        name: name.clone(),
                        ok: *ok,
                        summary: format!("{} {}", REPEAT_SUMMARY, summarize(content)),
                    });
                    history.push(Message::tool_result(call.id.clone(), content.clone()));
                    continue;
                }

                // A call that may change the machine invalidates reads answered
                // before it: their answers might no longer be true.
                let observation = self
                    .tools
                    .get(&name)
                    .map(|tool| tool.risk().is_observation())
                    .unwrap_or(true);
                if !observation {
                    answered.clear();
                }

                let _ = sink.send(AgentEvent::ToolStarted {
                    name: name.clone(),
                    arguments: arguments.clone(),
                });

                match self.dispatch(&name, &arguments, cancel.clone()).await {
                    Ok(output) => {
                        // A tool may have spent tokens on a backend this turn's
                        // provider never saw — a peer call, for one. Fold the
                        // value it reported into the turn's usage (R8, §5.13).
                        if let Some(reported) = output.reported_usage() {
                            usage.absorb(reported);
                        }
                        let _ = sink.send(AgentEvent::ToolFinished {
                            name: name.clone(),
                            ok: true,
                            summary: summarize(&output.content),
                        });
                        answered.insert(key, (true, output.content.clone()));
                        history.push(Message::tool_result(call.id.clone(), output.content));
                    }
                    Err(err) => {
                        let _ = sink.send(AgentEvent::ToolFinished {
                            name: name.clone(),
                            ok: false,
                            summary: err.to_string(),
                        });
                        let payload = error_payload(&err);
                        answered.insert(key, (false, payload.clone()));
                        history.push(Message::tool_result(call.id.clone(), payload));
                    }
                }
            }

            if exhausted {
                let _ = sink.send(AgentEvent::Notice(BUDGET_EXHAUSTED.to_string()));
                history.push(Message::system(BUDGET_EXHAUSTED));
                return outcome(last_text, StopReason::ToolBudget, usage, iterations);
            }
        }
    }

    /// Validate arguments, apply the timeout, and invoke one tool.
    async fn dispatch(
        &self,
        name: &str,
        arguments: &str,
        cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        let Some(tool) = self.tools.get(name) else {
            let available = self
                .tools
                .risks()
                .iter()
                .map(|(name, _)| name.to_string())
                .collect();
            return Err(Error::UnknownTool {
                name: name.to_string(),
                available,
            });
        };
        let args = parse_tool_arguments(name, arguments, self.options.repair_arguments)?;

        // Ask before running, and before spending the timeout budget. A refusal
        // surfaces to the model as a tool error it can read and react to.
        if let Some(gate) = &self.gate {
            let subject = tool.approval_subject(&args);
            gate.check(name, tool.risk(), &args, subject.as_deref())
                .await?;
        }

        let ctx = ToolCtx {
            workspace_root: self.options.workspace_root.clone(),
            cancel,
        };
        let budget = tool.timeout();
        let started = std::time::Instant::now();
        let result = match tokio::time::timeout(budget, tool.invoke(ctx, args)).await {
            Ok(Ok(mut output)) => {
                if let Some(cap) = self.options.tool_result_chars {
                    cap_output(&mut output, cap);
                }
                Ok(output)
            }
            Ok(Err(err)) => Err(err),
            Err(_) => Err(Error::Tool {
                tool: name.to_string(),
                message: format!("timed out after {budget:?}"),
            }),
        };
        // The gate held this call's audit row on `check`; finishing it here is
        // what makes the row carry both the decision and its result (§7).
        if let Some(gate) = &self.gate {
            gate.record_outcome(name, result.is_ok(), started.elapsed().as_millis() as u64)
                .await;
        }
        result
    }

    /// Keep `history` within `budget` tokens, reserving room for the schemas.
    ///
    /// Returns a message when the system prompt and the tool schemas alone
    /// already exceed the budget: trimming cannot help there, so the turn is
    /// failed with an actionable explanation rather than sent and rejected.
    fn fit_context(
        &self,
        history: &mut Vec<Message>,
        schemas: &[ToolSchema],
        budget: usize,
    ) -> Option<String> {
        let tools = estimate_tools(schemas);
        let system = history.first().map(estimate_message).unwrap_or(0);
        if tools + system > budget {
            return Some(format!(
                "context budget of {budget} tokens is smaller than the system prompt ({system}) \
                 plus the tool schemas ({tools}); raise agent.context_tokens"
            ));
        }
        trim_to_budget(history, budget - tools);
        None
    }
}

/// The one-time correction sent when a model returns an empty turn.
const NUDGE: &str = "[minion: your previous reply was empty. Either call one of the available \
                      tools or answer the user in plain text.]";

/// Shown when a text-only reply was cut off by the token limit.
const TRUNCATED_REPLY: &str = "the model hit its output limit; the reply may be truncated";

/// Drop the oldest whole turns until `history` fits `budget` tokens.
///
/// Index 0 (the system prompt) is always kept, and the cut lands on a `user`
/// message so an assistant `tool_calls` message is never separated from the
/// results that answer it. The same invariant as the message-count window, but
/// measured in tokens, which is what a small context actually cares about.
pub fn trim_to_budget(history: &mut Vec<Message>, budget: usize) {
    if history.len() <= 1 {
        return;
    }
    loop {
        if estimate_messages(history) <= budget {
            return;
        }
        // The next `user` message at or beyond index 2 begins the second turn,
        // so messages [1..cut) are the oldest complete turn.
        let next_user = history[1..]
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, message)| message.role == Role::User)
            .map(|(offset, _)| offset + 1);
        match next_user {
            Some(cut) => {
                history.drain(1..cut);
            }
            // Only the newest turn is left; trimming further would orphan it.
            None => return,
        }
    }
}

/// Clamp a tool result to `cap` characters, noting what was dropped.
fn cap_output(output: &mut ToolOutput, cap: usize) {
    let chars = output.content.chars().count();
    if chars <= cap {
        return;
    }
    let kept: String = output.content.chars().take(cap).collect();
    output.content = format!("{kept}\n… [truncated, {} characters omitted]", chars - cap);
    output.truncated = true;
}

fn outcome(text: Option<String>, stop: StopReason, usage: Usage, iterations: u32) -> TurnOutcome {
    TurnOutcome {
        text,
        stop,
        usage,
        iterations,
    }
}

/// A tool call assembled from streamed fragments.
#[derive(Default)]
struct PartialCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl PartialCall {
    fn finish(self, index: usize) -> ToolCall {
        ToolCall {
            id: self.id.unwrap_or_else(|| format!("call_{index}")),
            kind: "function".to_string(),
            function: FunctionCall {
                name: self.name.unwrap_or_default(),
                arguments: self.arguments,
            },
        }
    }
}

/// Error text returned to the model so it can correct itself.
fn error_payload(err: &Error) -> String {
    serde_json::json!({ "error": err.to_string() }).to_string()
}

/// Returned for a tool call the per-turn budget refused to run.
fn budget_payload() -> String {
    serde_json::json!({
        "error": "tool-call budget for this turn is exhausted; this call was not run",
    })
    .to_string()
}

/// Handed to the model when its tool call was cut off by the token limit.
const TRUNCATED_TOOL_CALL: &str = "Your previous response was cut off by the token \
limit before the tool call was complete, so it was discarded and nothing ran. Ask \
again with a shorter response: one tool call at a time, and keep the arguments minimal.";

/// Handed to the model, and shown to the user, when the tool budget runs out.
const BUDGET_EXHAUSTED: &str = "This turn's tool-call budget is exhausted, so the turn \
stops here. Send a new message to continue with another round of tools.";

/// One-line summary for a call refused because the budget is spent.
const BUDGET_EXHAUSTED_SUMMARY: &str = "tool-call budget exhausted; not run";

/// Prefix on the summary of a call answered from the turn's repeat cache.
const REPEAT_SUMMARY: &str = "repeat, reused:";

/// A cache key that treats two calls with the same arguments as one.
///
/// The arguments are re-serialized through `serde_json`, so `{"a":1,"b":2}` and
/// `{ "b": 2, "a": 1 }` collide: a model that reformats the same call twice
/// should still get one invocation. Arguments that are not JSON fall back to
/// their trimmed text, so a malformed repeat dedupes too. The unit separator
/// keeps a name and its arguments from running together.
fn call_key(name: &str, arguments: &str) -> String {
    let canonical = match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(value) => value.to_string(),
        Err(_) => arguments.trim().to_string(),
    };
    format!("{name}\u{1f}{canonical}")
}

/// First line of `content`, clipped for one-line display.
fn summarize(content: &str) -> String {
    let line = content.lines().next().unwrap_or("").trim();
    if line.chars().count() > 120 {
        let clipped: String = line.chars().take(117).collect();
        format!("{clipped}...")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Role;
    use crate::provider::FinishReason;
    use crate::tool::{Risk, Tool};

    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// One scripted provider response.
    enum Script {
        Events(Vec<ChatEvent>),
        Fail(String),
    }

    /// A provider that replays scripted turns and records the requests it saw.
    struct MockProvider {
        scripts: Mutex<VecDeque<Script>>,
        requests: Mutex<Vec<ChatRequest>>,
    }

    impl MockProvider {
        fn new(scripts: Vec<Script>) -> Self {
            Self {
                scripts: Mutex::new(scripts.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn last_request(&self) -> Option<ChatRequest> {
            self.requests.lock().unwrap().last().cloned()
        }
    }

    impl Provider for MockProvider {
        fn stream(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
        ) -> BoxStream<'static, Result<ChatEvent>> {
            self.requests.lock().unwrap().push(request);
            let items: Vec<Result<ChatEvent>> = match self.scripts.lock().unwrap().pop_front() {
                Some(Script::Events(events)) => events.into_iter().map(Ok).collect(),
                Some(Script::Fail(message)) => vec![Err(Error::Provider(message))],
                None => vec![Ok(ChatEvent::Done {
                    finish_reason: FinishReason::Stop,
                })],
            };
            Box::pin(futures::stream::iter(items))
        }
    }

    /// A tool that returns its `text` argument verbatim.
    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "Echo the `text` argument back."
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        fn risk(&self) -> Risk {
            Risk::ReadOnly
        }
        async fn invoke(&self, _ctx: ToolCtx, args: serde_json::Value) -> Result<ToolOutput> {
            Ok(ToolOutput::text(
                args["text"].as_str().unwrap_or_default().to_string(),
            ))
        }
    }

    /// A tool that spent tokens on a backend this turn's provider never saw —
    /// a peer delegation (M10.2) — and reports them in its metadata.
    struct Reporting;

    #[async_trait]
    impl Tool for Reporting {
        fn name(&self) -> &'static str {
            "peer__big_ask"
        }
        fn description(&self) -> &'static str {
            "Delegate a brief to a peer."
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        fn risk(&self) -> Risk {
            Risk::Network
        }
        async fn invoke(&self, _ctx: ToolCtx, _args: serde_json::Value) -> Result<ToolOutput> {
            Ok(
                ToolOutput::text("the peer's answer").with_metadata(serde_json::json!({
                    "peer": "big",
                    "usage": { "prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12 }
                })),
            )
        }
    }

    fn agent(provider: MockProvider, tools: ToolRegistry, workspace: &Path) -> Agent {
        Agent::new(
            Arc::new(provider),
            Arc::new(tools),
            AgentOptions {
                model: "mock".to_string(),
                max_iterations: 4,
                temperature: None,
                max_tokens: None,
                parallel_tool_calls: None,
                include_usage: false,
                max_tool_calls_per_turn: 0,
                workspace_root: workspace.to_path_buf(),
                ..AgentOptions::default()
            },
        )
    }

    fn tool_call(name: &str, arguments: &str) -> ChatEvent {
        ChatEvent::ToolCallDelta {
            index: 0,
            id: Some("call_1".to_string()),
            name: Some(name.to_string()),
            arguments: arguments.to_string(),
        }
    }

    /// A call at `index`, with a distinct id, for a batch of several calls.
    fn call_at(index: usize, name: &str, arguments: &str) -> ChatEvent {
        ChatEvent::ToolCallDelta {
            index,
            id: Some(format!("call_{index}")),
            name: Some(name.to_string()),
            arguments: arguments.to_string(),
        }
    }

    /// A tool that counts how many times it actually ran.
    ///
    /// Dedup is only provable by counting invocations: a `tool` message that
    /// looks right can also come from re-running the tool.
    struct Counting {
        name: &'static str,
        risk: Risk,
        runs: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for Counting {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "count invocations"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        fn risk(&self) -> Risk {
            self.risk
        }
        async fn invoke(&self, _ctx: ToolCtx, args: serde_json::Value) -> Result<ToolOutput> {
            let n = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(ToolOutput::text(format!(
                "{}:{n}",
                args["text"].as_str().unwrap_or_default()
            )))
        }
    }

    fn counting(name: &'static str, risk: Risk, runs: &Arc<AtomicUsize>) -> Counting {
        Counting {
            name,
            risk,
            runs: runs.clone(),
        }
    }

    /// Like `agent`, but with an explicit per-turn tool budget.
    fn agent_with_budget(
        provider: MockProvider,
        tools: ToolRegistry,
        workspace: &Path,
        budget: u32,
    ) -> Agent {
        Agent::new(
            Arc::new(provider),
            Arc::new(tools),
            AgentOptions {
                model: "mock".to_string(),
                max_iterations: 6,
                temperature: None,
                max_tokens: None,
                parallel_tool_calls: None,
                include_usage: false,
                max_tool_calls_per_turn: budget,
                workspace_root: workspace.to_path_buf(),
                ..AgentOptions::default()
            },
        )
    }

    fn done(reason: FinishReason) -> ChatEvent {
        ChatEvent::Done {
            finish_reason: reason,
        }
    }

    #[tokio::test]
    async fn completes_when_the_model_stops() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![Script::Events(vec![
            ChatEvent::TextDelta("hello".to_string()),
            done(FinishReason::Stop),
        ])]);
        let agent = agent(provider, ToolRegistry::new(), dir.path());
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("hi")];

        let outcome = agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        assert_eq!(outcome.stop, StopReason::Completed);
        assert_eq!(outcome.text.as_deref(), Some("hello"));
        assert_eq!(outcome.iterations, 1);
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].role, Role::Assistant);
    }

    #[tokio::test]
    async fn runs_a_tool_and_feeds_the_result_back() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            Script::Events(vec![
                tool_call("echo", "{\"text\":\"pong\"}"),
                done(FinishReason::ToolCalls),
            ]),
            Script::Events(vec![
                ChatEvent::TextDelta("done".to_string()),
                done(FinishReason::Stop),
            ]),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Echo);
        let agent = agent(provider, tools, dir.path());
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("echo pong")];

        let outcome = agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        assert_eq!(outcome.stop, StopReason::Completed);
        assert_eq!(outcome.iterations, 2);
        // user, assistant(tool_calls), tool(result), assistant(text)
        assert_eq!(history.len(), 4);
        assert_eq!(history[2].role, Role::Tool);
        assert_eq!(history[2].content.as_deref(), Some("pong"));
        assert_eq!(history[2].tool_call_id.as_deref(), Some("call_1"));
    }

    #[tokio::test]
    async fn argument_fragments_are_concatenated_before_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            Script::Events(vec![
                tool_call("echo", "{\"text\":\"po"),
                tool_call("echo", "ng\"}"),
                done(FinishReason::ToolCalls),
            ]),
            Script::Events(vec![done(FinishReason::Stop)]),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Echo);
        let agent = agent(provider, tools, dir.path());
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("echo")];

        agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        assert_eq!(history[2].content.as_deref(), Some("pong"));
    }

    #[tokio::test]
    async fn unknown_tools_are_reported_back_instead_of_aborting() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            Script::Events(vec![tool_call("nope", "{}"), done(FinishReason::ToolCalls)]),
            Script::Events(vec![
                ChatEvent::TextDelta("recovered".to_string()),
                done(FinishReason::Stop),
            ]),
        ]);
        let agent = agent(provider, ToolRegistry::new(), dir.path());
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("go")];

        let outcome = agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        assert_eq!(outcome.stop, StopReason::Completed);
        let result = history[2].content.clone().unwrap_or_default();
        assert!(
            result.contains("unknown tool"),
            "unexpected result: {result}"
        );
    }

    #[tokio::test]
    async fn malformed_arguments_become_a_tool_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            Script::Events(vec![
                tool_call("echo", "{not json"),
                done(FinishReason::ToolCalls),
            ]),
            Script::Events(vec![done(FinishReason::Stop)]),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Echo);
        let agent = agent(provider, tools, dir.path());
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("go")];

        agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        let result = history[2].content.clone().unwrap_or_default();
        assert!(
            result.contains("invalid arguments"),
            "unexpected result: {result}"
        );
    }

    #[tokio::test]
    async fn the_iteration_limit_ends_a_tool_loop() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = (0..4)
            .map(|_| {
                Script::Events(vec![
                    tool_call("echo", "{\"text\":\"x\"}"),
                    done(FinishReason::ToolCalls),
                ])
            })
            .collect();
        let provider = MockProvider::new(scripts);
        let mut tools = ToolRegistry::new();
        tools.register(Echo);
        let agent = agent(provider, tools, dir.path());
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("loop")];

        let outcome = agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        assert_eq!(outcome.stop, StopReason::IterationLimit);
        assert_eq!(outcome.iterations, 4);
    }

    #[tokio::test]
    async fn provider_errors_end_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![Script::Fail("upstream exploded".to_string())]);
        let agent = agent(provider, ToolRegistry::new(), dir.path());
        let (sink, mut events) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("hi")];

        let outcome = agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        assert_eq!(outcome.stop, StopReason::ProviderError);
        let reported = std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, AgentEvent::Failed(_)));
        assert!(reported, "the failure was not surfaced to the UI");
    }

    #[tokio::test]
    async fn cancellation_ends_the_turn_before_the_first_call() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![Script::Events(vec![done(FinishReason::Stop)])]);
        let agent = agent(provider, ToolRegistry::new(), dir.path());
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("hi")];

        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = agent.run(&mut history, &sink, cancel).await;

        assert_eq!(outcome.stop, StopReason::Cancelled);
        assert_eq!(outcome.iterations, 0);
    }

    #[tokio::test]
    async fn tool_events_are_emitted_for_the_ui() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            Script::Events(vec![
                tool_call("echo", "{\"text\":\"x\"}"),
                done(FinishReason::ToolCalls),
            ]),
            Script::Events(vec![done(FinishReason::Stop)]),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Echo);
        let agent = agent(provider, tools, dir.path());
        let (sink, mut events) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("go")];

        agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        let observed: Vec<AgentEvent> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        assert!(
            observed
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolStarted { name, .. } if name == "echo"))
        );
        assert!(
            observed
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolFinished { ok: true, .. }))
        );
    }

    #[tokio::test]
    async fn the_request_carries_the_registered_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(MockProvider::new(vec![Script::Events(vec![done(
            FinishReason::Stop,
        )])]));
        let mut tools = ToolRegistry::new();
        tools.register(Echo);
        let agent = Agent::new(
            provider.clone(),
            Arc::new(tools),
            AgentOptions {
                model: "mock".to_string(),
                max_iterations: 4,
                temperature: None,
                max_tokens: None,
                parallel_tool_calls: None,
                include_usage: false,
                max_tool_calls_per_turn: 0,
                workspace_root: dir.path().to_path_buf(),
                ..AgentOptions::default()
            },
        );
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("hi")];

        agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        let request = provider.last_request().expect("a request was sent");
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, "echo");
        assert_eq!(request.model, "mock");
        assert_eq!(request.messages.len(), 1);
        assert_eq!(provider.request_count(), 1);
    }

    /// R8, §5.13: tokens a *tool* spent elsewhere join the turn's usage, which
    /// is what `/cost` reads. The provider's own report is still counted, and
    /// the two are added, not replaced.
    #[tokio::test]
    async fn a_tools_reported_usage_joins_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            Script::Events(vec![
                tool_call("peer__big_ask", "{\"brief\":\"hi\"}"),
                ChatEvent::Usage(crate::provider::Usage {
                    prompt_tokens: 3,
                    completion_tokens: 4,
                    total_tokens: 7,
                }),
                done(FinishReason::ToolCalls),
            ]),
            Script::Events(vec![done(FinishReason::Stop)]),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Reporting);
        let agent = agent(provider, tools, dir.path());
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("ask the peer")];

        let outcome = agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        assert_eq!(outcome.usage.prompt_tokens, 3 + 5);
        assert_eq!(outcome.usage.completion_tokens, 4 + 7);
        assert_eq!(outcome.usage.total_tokens, 7 + 12);
        // The value is advisory: the answer is still in the transcript.
        assert_eq!(history[2].content.as_deref(), Some("the peer's answer"));
    }

    // ------------------------------------------------- tool calls that misbehave

    /// A turn cut off by the token limit stops mid-way through a tool call's
    /// arguments, so the JSON is half an object. It must be discarded, not
    /// parsed and not run, and the model must be asked for a shorter answer.
    #[tokio::test]
    async fn a_tool_call_truncated_by_the_token_limit_is_discarded_and_reasked() {
        let dir = tempfile::tempdir().unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        let provider = MockProvider::new(vec![
            Script::Events(vec![
                // Half a JSON object, then `length`.
                tool_call("count", "{\"text\":\"ha"),
                done(FinishReason::Length),
            ]),
            Script::Events(vec![
                ChatEvent::TextDelta("short answer".to_string()),
                done(FinishReason::Stop),
            ]),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(counting("count", Risk::ReadOnly, &runs));
        let agent = agent(provider, tools, dir.path());
        let (sink, mut events) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("go")];

        let outcome = agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        assert_eq!(outcome.stop, StopReason::Completed);
        assert_eq!(outcome.text.as_deref(), Some("short answer"));
        assert_eq!(
            runs.load(Ordering::SeqCst),
            0,
            "the unfinished call must not run"
        );
        assert!(
            !history.iter().any(|message| message.tool_calls.is_some()),
            "the truncated batch must not reach the transcript: {history:?}"
        );
        assert!(
            !history.iter().any(|message| message.role == Role::Tool),
            "nothing may answer a call the model never finished"
        );
        assert!(
            history.iter().any(|message| message.role == Role::System
                && message
                    .content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("cut off")),
            "the model must be told to shorten its answer"
        );
        let noticed = std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, AgentEvent::Notice(_)));
        assert!(noticed, "the user must be told too");
    }

    #[tokio::test]
    async fn a_repeated_tool_call_is_answered_without_running_again() {
        let dir = tempfile::tempdir().unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        let provider = MockProvider::new(vec![
            Script::Events(vec![
                tool_call("count", "{\"text\":\"a\"}"),
                done(FinishReason::ToolCalls),
            ]),
            Script::Events(vec![
                tool_call("count", "{\"text\":\"a\"}"),
                done(FinishReason::ToolCalls),
            ]),
            Script::Events(vec![
                ChatEvent::TextDelta("done".to_string()),
                done(FinishReason::Stop),
            ]),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(counting("count", Risk::ReadOnly, &runs));
        let agent = agent_with_budget(provider, tools, dir.path(), 0);
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("go")];

        let outcome = agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        assert_eq!(outcome.stop, StopReason::Completed);
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "the second, identical call is a repeat"
        );
        let results: Vec<&str> = history
            .iter()
            .filter(|message| message.role == Role::Tool)
            .filter_map(|message| message.content.as_deref())
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], results[1], "the repeat reuses the first answer");
    }

    #[tokio::test]
    async fn duplicate_calls_in_one_batch_run_once() {
        let dir = tempfile::tempdir().unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        let provider = MockProvider::new(vec![
            Script::Events(vec![
                call_at(0, "count", "{}"),
                call_at(1, "count", "{}"),
                done(FinishReason::ToolCalls),
            ]),
            Script::Events(vec![done(FinishReason::Stop)]),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(counting("count", Risk::ReadOnly, &runs));
        let agent = agent_with_budget(provider, tools, dir.path(), 0);
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("go")];

        agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        assert_eq!(runs.load(Ordering::SeqCst), 1);
        let answered: Vec<(String, Option<String>)> = history
            .iter()
            .filter(|message| message.role == Role::Tool)
            .map(|message| {
                (
                    message.tool_call_id.clone().unwrap_or_default(),
                    message.content.clone(),
                )
            })
            .collect();
        assert_eq!(answered.len(), 2, "both ids must be answered");
        assert_eq!(answered[0].0, "call_0");
        assert_eq!(answered[1].0, "call_1");
        assert_eq!(answered[0].1, answered[1].1);
    }

    /// A call that may change the machine must void a read answered before it:
    /// otherwise a model that writes a file and re-reads it sees stale bytes.
    #[tokio::test]
    async fn a_state_changing_call_invalidates_cached_reads() {
        let dir = tempfile::tempdir().unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(AtomicUsize::new(0));
        let provider = MockProvider::new(vec![
            Script::Events(vec![tool_call("peek", "{}"), done(FinishReason::ToolCalls)]),
            Script::Events(vec![tool_call("poke", "{}"), done(FinishReason::ToolCalls)]),
            Script::Events(vec![tool_call("peek", "{}"), done(FinishReason::ToolCalls)]),
            Script::Events(vec![done(FinishReason::Stop)]),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(counting("peek", Risk::ReadOnly, &reads));
        tools.register(counting("poke", Risk::Write, &writes));
        let agent = agent_with_budget(provider, tools, dir.path(), 0);
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("go")];

        agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "the read after the write must run again, not come from the cache"
        );
    }

    /// The budget bounds a runaway turn *and* leaves a transcript the provider
    /// will accept: every `tool_calls` id still has a matching result.
    #[tokio::test]
    async fn the_tool_budget_stops_a_runaway_turn_answering_every_call() {
        let dir = tempfile::tempdir().unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        let provider = MockProvider::new(vec![Script::Events(vec![
            call_at(0, "count", "{}"),
            call_at(1, "count", "{}"),
            call_at(2, "count", "{}"),
            done(FinishReason::ToolCalls),
        ])]);
        let mut tools = ToolRegistry::new();
        tools.register(counting("count", Risk::ReadOnly, &runs));
        let agent = agent_with_budget(provider, tools, dir.path(), 1);
        let (sink, mut events) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("go")];

        let outcome = agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        assert_eq!(outcome.stop, StopReason::ToolBudget);
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "only the first call may run"
        );
        let results: Vec<&str> = history
            .iter()
            .filter(|message| message.role == Role::Tool)
            .filter_map(|message| message.content.as_deref())
            .collect();
        assert_eq!(results.len(), 3, "every tool_call id stays answered");
        assert!(results[0].contains(":1"));
        assert!(results[1].contains("budget"));
        assert!(results[2].contains("budget"));
        assert!(
            history.iter().any(|message| message.role == Role::System
                && message
                    .content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("budget")),
            "the model and the user are told why the turn stopped"
        );
        let noticed = std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, AgentEvent::Notice(_)));
        assert!(noticed);
    }

    #[test]
    fn call_keys_canonicalise_arguments() {
        assert_eq!(
            call_key("read_file", "{\"a\":1,\"b\":2}"),
            call_key("read_file", "{ \"b\": 2, \"a\": 1 }"),
            "key order must not defeat the dedup"
        );
        assert_ne!(
            call_key("read_file", "{\"a\":1}"),
            call_key("edit_file", "{\"a\":1}"),
            "the tool name is part of the key"
        );
    }

    #[test]
    fn trim_to_budget_keeps_the_system_prompt_and_newest_turn() {
        let mut history = vec![
            Message::system("sys"),
            Message::user("one"),
            Message::assistant("a1"),
            Message::user("two"),
            Message::assistant("a2"),
        ];

        trim_to_budget(&mut history, 1);

        assert_eq!(history[0].content.as_deref(), Some("sys"));
        assert_eq!(
            history.len(),
            3,
            "only the system prompt and newest turn survive"
        );
        assert_eq!(history[1].content.as_deref(), Some("two"));
        assert_eq!(history[2].content.as_deref(), Some("a2"));
    }

    #[test]
    fn trim_to_budget_never_splits_a_tool_call_from_its_result() {
        let call = |id: &str| ToolCall {
            id: id.to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "echo".to_string(),
                arguments: "{\"text\":\"x\"}".to_string(),
            },
        };
        let mut history = vec![
            Message::system("sys"),
            Message::user("turn one"),
            Message::assistant_with_tool_calls(None, vec![call("c1")]),
            Message::tool_result("c1", "result one"),
            Message::assistant("done one"),
            Message::user("turn two"),
            Message::assistant_with_tool_calls(None, vec![call("c2")]),
            Message::tool_result("c2", "result two"),
            Message::assistant("done two"),
        ];

        // A budget that cannot fit both turns forces the oldest one out.
        trim_to_budget(&mut history, 1);

        assert_eq!(
            history.first().map(|message| message.role),
            Some(Role::System)
        );
        assert_eq!(
            history[1].content.as_deref(),
            Some("turn two"),
            "the cut lands on the newest user turn"
        );
        // Whatever survives must be a well-formed transcript: every `tool`
        // message still answers an assistant call that is also present. An
        // orphan here is exactly the corruption the boundary rule prevents.
        let mut answerable: Vec<String> = Vec::new();
        for message in &history {
            match message.role {
                Role::Assistant => {
                    answerable = message
                        .tool_calls
                        .clone()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|call| call.id)
                        .collect();
                }
                Role::Tool => {
                    let id = message.tool_call_id.clone().unwrap_or_default();
                    assert!(
                        answerable.contains(&id),
                        "orphaned tool result `{id}` after trimming"
                    );
                    answerable.retain(|pending| pending != &id);
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn missing_tool_ids_get_unique_fallbacks() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            Script::Events(vec![
                ChatEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: Some("echo".to_string()),
                    arguments: "{\"text\":\"a\"}".to_string(),
                },
                ChatEvent::ToolCallDelta {
                    index: 1,
                    id: None,
                    name: Some("echo".to_string()),
                    arguments: "{\"text\":\"b\"}".to_string(),
                },
                done(FinishReason::ToolCalls),
            ]),
            Script::Events(vec![done(FinishReason::Stop)]),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Echo);
        let agent = agent(provider, tools, dir.path());
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("go")];

        agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        let ids: Vec<String> = history
            .iter()
            .filter(|message| message.role == Role::Tool)
            .filter_map(|message| message.tool_call_id.clone())
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1], "fallback ids must not collide");
    }

    #[tokio::test]
    async fn an_empty_reply_is_retried_once() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(MockProvider::new(vec![
            Script::Events(vec![done(FinishReason::Stop)]),
            Script::Events(vec![
                ChatEvent::TextDelta("late".to_string()),
                done(FinishReason::Stop),
            ]),
        ]));
        let options = AgentOptions {
            model: "mock".to_string(),
            max_iterations: 4,
            nudge_on_empty: true,
            workspace_root: dir.path().to_path_buf(),
            ..AgentOptions::default()
        };
        let agent = Agent::new(provider.clone(), Arc::new(ToolRegistry::new()), options);
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("hi")];

        let outcome = agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        assert_eq!(outcome.text.as_deref(), Some("late"));
        assert_eq!(provider.request_count(), 2, "the nudge costs one retry");
    }

    #[tokio::test]
    async fn long_tool_results_are_capped_for_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let long = "x".repeat(200);
        let provider = MockProvider::new(vec![
            Script::Events(vec![
                tool_call("echo", &format!("{{\"text\":\"{long}\"}}")),
                done(FinishReason::ToolCalls),
            ]),
            Script::Events(vec![done(FinishReason::Stop)]),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Echo);
        let options = AgentOptions {
            model: "mock".to_string(),
            max_iterations: 4,
            tool_result_chars: Some(20),
            workspace_root: dir.path().to_path_buf(),
            ..AgentOptions::default()
        };
        let agent = Agent::new(Arc::new(provider), Arc::new(tools), options);
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("go")];

        agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        let result = history[2].content.clone().unwrap_or_default();
        assert!(result.contains("truncated"), "unexpected result: {result}");
        assert!(result.chars().count() < 200);
    }

    #[tokio::test]
    async fn a_malformed_but_repairable_call_is_salvaged() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            Script::Events(vec![
                tool_call("echo", "{\"text\":\"pong\",}"),
                done(FinishReason::ToolCalls),
            ]),
            Script::Events(vec![done(FinishReason::Stop)]),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Echo);
        let options = AgentOptions {
            model: "mock".to_string(),
            max_iterations: 4,
            repair_arguments: true,
            workspace_root: dir.path().to_path_buf(),
            ..AgentOptions::default()
        };
        let agent = Agent::new(Arc::new(provider), Arc::new(tools), options);
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("go")];

        agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        assert_eq!(history[2].content.as_deref(), Some("pong"));
    }

    #[tokio::test]
    async fn an_unknown_tool_lists_the_alternatives() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            Script::Events(vec![
                tool_call("ecoh", "{\"text\":\"x\"}"),
                done(FinishReason::ToolCalls),
            ]),
            Script::Events(vec![done(FinishReason::Stop)]),
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Echo);
        let agent = agent(provider, tools, dir.path());
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut history = vec![Message::user("go")];

        agent
            .run(&mut history, &sink, CancellationToken::new())
            .await;

        let result = history[2].content.clone().unwrap_or_default();
        assert!(result.contains("echo"), "unexpected result: {result}");
    }
}
