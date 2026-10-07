# Software Design Document — `minion`

**A minimal, Unix-native AI agent harness with a shell-like REPL, an OpenAI-compatible model client, and an MCP server.**

| Field | Value |
|---|---|
| Project | `minion` |
| Document | Software Design Document (SDD) |
| Version | 0.1 (Draft) |
| Status | For review |
| Date | 2026-09-29 |
| Language | Rust (edition 2024), `tokio` async runtime |
| Primary artifact | Single static binary `minion` |
| Platforms | macOS, Linux (primary); Windows best-effort |

### Glossary

| Term | Meaning |
|---|---|
| Turn | One user request → model calls → tool executions → final answer |
| Tool | A capability the model can invoke (JSON-schema described) |
| Job | A cron-scheduled prompt owned by the in-process scheduler |
| Approval | Interactive gate before a side-effecting action executes |
| MCP | Model Context Protocol (server = we expose, client = we consume) |
| Workspace | The directory tree the agent is permitted to touch |

---

## 1. Overview

### 1.1 Problem

Most AI agent harnesses are heavy: a full-screen TUI, an IDE-like pane layout, a daemon with a web UI, and a large dependency surface. That makes them awkward to embed, script, pipe, and audit. `fx` (Vercel Labs, Zig) demonstrated the opposite: an agent that behaves like a **Unix shell** — preserves scrollback, emits minimal output, and composes with pipes and scripts.

`minion` applies that philosophy to a general-purpose (not coding-specific) mini agent that:

1. Talks to any **OpenAI-compatible** `/v1/chat/completions` endpoint.
2. Ships a **small, audited tool set** — command execution, file access, HTTP, cron, memory.
3. Presents an **fx-like line REPL** (no alternate screen).
4. Runs an **in-process cron scheduler**.
5. Is **both an MCP server and an MCP client**, so other models can drive it and it can drive others.

### 1.2 Goals

| ID | Goal |
|---|---|
| G1 | One small static binary; no daemon required for interactive use |
| G2 | Provider-agnostic via the OpenAI Chat Completions API |
| G3 | Line-oriented REPL that preserves scrollback and composes with pipes |
| G4 | Side effects are gated: nothing dangerous runs without policy consent |
| G5 | Durable sessions, jobs, and memory in a single SQLite file |
| G6 | Expose the agent as an MCP server; consume external MCP servers as tools |
| G7 | Every component independently testable without network access |

### 1.3 Non-goals (v1)

- No subagents / recursive task spawning.
- No embeddings or vector search (memory is keyword/FTS based).
- No full-screen TUI panes.
- No browser automation or GUI control.
- No multi-user server, auth, or tenancy.
- No own model serving (we are a client, not an inference server).

### 1.4 Design principles

1. **Unix-native.** stdout is data, stderr is diagnostics. Respect `NO_COLOR`, `PIPE`, `--json`.
2. **Fail closed.** Unknown tool → denied. Non-TTY → no implicit `--yes`.
3. **Explicit over magic.** No hidden retries on side-effecting tools. No silent network calls.
4. **One source of truth.** SQLite owns state; config files own policy; nothing else persists.
5. **Small surface.** Six tool families. Each has a schema, a policy class, and a test.

---

## 2. Requirements

### 2.1 Functional requirements

**Model / provider**

| ID | Requirement |
|---|---|
| FR-1 | Call any OpenAI-compatible `/v1/chat/completions` with configurable `base_url` and API key env var |
| FR-2 | Stream responses (SSE) and render text incrementally |
| FR-3 | Support OpenAI tool calling: send `tools[]`, parse `tool_calls`, return `role:"tool"` results |
| FR-4 | Retry transient failures (429, 5xx) with exponential backoff + jitter, honoring `Retry-After` |
| FR-5 | Track token usage per turn from the `usage` field; degrade gracefully when absent |
| FR-6 | Support provider quirks via config flags (disable parallel tool calls, disable usage streaming) |

**Agent loop**

| ID | Requirement |
|---|---|
| FR-7 | Iterate model ⇄ tools until a final text answer, iteration cap, or budget cap |
| FR-8 | Execute multiple tool calls from one assistant message, bounded concurrency |
| FR-9 | Halt with a machine-readable reason (`completed`, `iteration_limit`, `token_budget`, `cancelled`, `error`) |
| FR-10 | Cancel an in-flight turn on Ctrl-C, killing child processes |

**Tools**

| ID | Requirement |
|---|---|
| FR-11 | `run_command`, `cron_add`/`cron_list`/`cron_remove`, `read_file`/`write_file`/`edit_file`, `http_fetch`, `mcp_call`, `remember`/`recall` |
| FR-12 | Every tool declares a JSON schema, a risk class, a timeout, and an output cap |
| FR-13 | Tool errors are returned to the model as structured results, not panics |

**Approval / policy**

| ID | Requirement |
|---|---|
| FR-14 | Classify each invocation as `auto`, `ask`, or `deny` from config + static risk rules |
| FR-15 | Interactive approval prompt with one-shot, session, and persistent allowlist options |
| FR-16 | Persistent allowlist entries are scoped (tool + pattern + workspace) and revocable |
| FR-17 | Non-interactive mode never prompts: config decides, default `deny` for side effects |
| FR-43 | Optional System One guard (`POST {base}/v1/systemone`) may resolve a flagged `run_command` prompt when the category is eligible and the model is confident; it may never widen what the static rules allow |
| FR-44 | Guard eligibility is a floor: `privilege`, `remote-execution` and `destructive` never reach the model and always prompt |
| FR-45 | Every guard failure — network, timeout, rate limit, unparseable body, withdrawn model — falls back to prompting, never to allowing |
| FR-46 | Only the command string is sent as System One `state`; the transcript, tool output and file contents are never included |
| FR-47 | Two thresholds with a human in the middle: a verdict above `deny_threshold` or between the thresholds prompts; only at or below `allow_threshold` resolves silently, and every verdict is audited |

**Cron**

| ID | Requirement |
|---|---|
| FR-18 | Create/list/remove jobs with 5-field cron expressions and IANA timezone |
| FR-19 | Fire jobs while the agent runs; persist before acknowledgment |
| FR-20 | Catch up missed runs on startup according to a per-job policy (`skip`, `run_once`, `run_all`) |
| FR-21 | Record each run with status, duration, and output reference |

**MCP**

| ID | Requirement |
|---|---|
| FR-22 | Serve MCP over stdio: tools, resources, and a prompt for "ask the agent" |
| FR-23 | Consume external MCP servers (stdio) declared in config; expose their tools as `mcp__<server>__<tool>` |
| FR-24 | Deny `run_command` exposure over MCP unless explicitly enabled |
| FR-25 | Per-server allowlists and per-server approval policy |

**Interface**

| ID | Requirement |
|---|---|
| FR-26 | REPL default; one-shot `minion run "<prompt>"`; stdin piping |
| FR-27 | Slash commands for local control (`/help`, `/model`, `/sessions`, `/cron`, `/allow`, `/quit`) |
| FR-28 | `!<cmd>` shell escape that bypasses the model |
| FR-29 | `--json` machine-readable output for one-shot and management subcommands |
| FR-30 | Session management: list, show, resume, delete |
| FR-31 | `minion init` configures the model backend (base URL, API key env var, model) and writes a config file |
| FR-32 | The config file records only the API key env var *name*; the key value is never written to a config, so a config may be committed |
| FR-33 | `init` discovers models via `GET {base_url}/models` when reachable, and falls back to free-text entry when it is not |
| FR-34 | `init` is idempotent and non-destructive: an existing config is diffed and requires `--force` to replace, and writes are atomic |
| FR-35 | `init --check` probes the backend for reachability and auth before writing, and writes nothing when the probe fails |
| FR-36 | Interactive `init` asks for the real API key with terminal echo disabled **before** the prompt is printed, and stores it in a `0600` credentials file; `--no-store-token` opts out |
| FR-37 | Credential resolution order is `provider.api_key_env` (if set) → `provider.api_key_file` → error, so one shell or CI job can override the stored secret without editing files |
| FR-38 | Arbitrary extra headers for the provider are configurable under `[provider.headers]`, and `init --header` records them |
| FR-39 | A header value may reference `${session}`, expanded to the stable per-conversation id so gateways can pin routing and reuse prompt caches |
| FR-40 | An empty `provider.api_key_env` *and* an empty `provider.api_key_file` means the backend needs no credentials, and no `Authorization` header is sent |
| FR-41 | `apply_patch` applies a set of anchored operations across one or more files atomically, failing the whole patch if any operation is ambiguous or absent |
| FR-42 | A file-writing or command-executing tool never runs without a resolved decision; a denied or unattributable call returns an error to the model instead of executing |

### 2.2 Non-functional requirements

| ID | Requirement | Target |
|---|---|---|
| NFR-1 | Cold start to prompt | < 60 ms |
| NFR-2 | Idle RSS | < 40 MB |
| NFR-3 | Release binary size (stripped, musl) | < 15 MB |
| NFR-4 | Tool output captured to memory capped by default | 256 KiB (configurable) |
| NFR-5 | Default command timeout | 120 s |
| NFR-6 | Crash safety: no lost job or message on `SIGKILL` | SQLite WAL + write-before-ack |
| NFR-7 | Offline unit/integration test suite | No network required |
| NFR-8 | Accessibility | No color dependence; `NO_COLOR` honored; screen-reader-safe line output. Markdown rendering is additive — it reflows plain text and adds table borders, and no meaning is carried by colour alone |
| NFR-9 | Log redaction | API keys and env values never logged |
| NFR-10 | `init` performs no network I/O unless `--check` is passed | Setup works offline (local backends, air-gapped machines) |

---

## 3. Architecture

### 3.1 High-level view

```
┌────────────┐        ┌──────────────────────── minion (one binary) ─────────────────────┐
│  Terminal  │        │                                                                  │
│ TTY / pipe │◄──────►│  ┌──────────┐        ┌──────────────┐      ┌────────────────┐     │
└────────────┘        │  │  REPL /  │        │  Agent Loop  │      │  Provider      │─────┼──► OpenAI-compatible
                      │  │  Frontend│◄──────►│ (turn state  │─────►│  Client        │     │    /chat/completions
                      │  │ rustyline│        │  machine)    │◄─────│  (SSE + tools) │     │
                      │  └────┬─────┘        └──────┬───────┘      └────────────────┘     │
                      │       │                     │                                     │
                      │       │ approval            │ dispatch                            │
                      │       ▼                     ▼                                     │
                      │  ┌──────────┐        ┌──────────────┐      ┌────────────────┐     │
                      │  │ Approval │        │  Tool        │─────►│  MCP Client    │─────┼──► external MCP
                      │  │ Engine   │        │  Registry    │      │  (stdio)       │     │    servers
                      │  └────┬─────┘        └──────┬───────┘      └────────────────┘     │
                      │       │                     │                                     │
                      │       ▼                     ▼                                     │
                      │  ┌──────────┐        ┌──────────────┐                              │
                      │  │ Policy   │        │  SQLite      │                              │
                      │  │ Store    │───────►│  Store (WAL) │                              │
                      │  └──────────┘        └──────┬───────┘                              │
                      │                              ▲                                      │
                      │  ┌──────────────┐            │                                      │
                      │  │  Cron        │────────────┘                                      │
                      │  │  Scheduler   │                                                   │
                      │  └──────┬───────┘                                                   │
                      │         │ inject prompt                                               │
                      │         └──────────────► Agent Loop                                   │
                      │                                                                      │
                      │  ┌────────────────────────────────────────┐                          │
                      │  │  MCP Server (stdio)                    │◄──── other models / hosts│
                      │  │  tools: agent_ask, agent_*, cron_*, …  │                          │
                      │  └────────────────────────────────────────┘                          │
                      └──────────────────────────────────────────────────────────────────────┘
```

### 3.2 Component responsibilities

| Component | Responsibility | Must not |
|---|---|---|
| Frontend (REPL/TUI) | Read input, render stream, prompt for approval | Contain policy logic |
| Agent Loop | Turn state machine, message assembly, tool dispatch | Execute commands itself |
| Provider Client | HTTP + SSE to the model, retries, usage accounting | Know about tools' semantics |
| Tool Registry | Schema, validation, timeouts, output caps, risk class | Decide approval outcomes |
| Approval Engine | Resolve `auto`/`ask`/`deny`, prompt, persist allowlist | Execute tools |
| Policy Store | Persist allowlist, deny rules, workspace roots | Interpret commands |
| Cron Scheduler | Compute due jobs, dispatch prompts, record runs | Mutate job definitions |
| SQLite Store | Sole persistence: sessions, messages, jobs, runs, memory, policy | Hold secrets in plaintext |
| MCP Client | Discover/call external MCP tools | Bypass approval |
| MCP Server | Expose minion capabilities to other models | Expose `run_command` by default |

### 3.3 Execution model

- **One `tokio` runtime.** A multi-threaded runtime with a modest worker count (`min(4, num_cpus)`).
- **Frontend** runs on the main task; it owns terminal I/O exclusively (single writer).
- **Agent loop** runs as a spawned task per turn. Ctrl-C sends cancellation via a `CancellationToken`.
- **Tool execution** uses a `JoinSet` with a semaphore (default concurrency 4). Each tool runs in its own task so a hung tool cannot block the loop.
- **Cron scheduler** is a long-lived task ticking every second, comparing `next_run_at <= now` against the store.
- **MCP server** is a separate task over stdio; it must not interleave with REPL output — when serving MCP, the REPL is disabled.
- **Channels:** `mpsc` for events frontend ← loop, `oneshot` per approval request, `broadcast` for cancellation and shutdown.

### 3.4 A turn, end to end

```
1. Input arrives (REPL line, `run` arg, stdin, cron fire, or MCP agent_ask)
2. Persist user message
3. Assemble request: system prompt + policy digest + tool schemas + history window
4. POST /chat/completions (stream)
5. Stream deltas → render content; accumulate tool_calls
6. If finish_reason != tool_calls → persist assistant, emit final, done
7. Else: persist assistant message with tool_calls
8. For each tool call (bounded concurrency):
     a. Registry validates args against schema      → invalid: tool error result
     b. Approval engine resolves policy             → ask: prompt; deny: tool error result
     c. Execute with timeout + output cap + cancellation
     d. Persist tool result
9. Inject tool results as role:"tool"; loop to 4
10. Stop on completed / iteration cap / budget / cancel / error
```

---

## 4. Module layout

Proposed crates (single workspace, one binary + library for testability):

```
minion/
├── Cargo.toml                # workspace
├── crates/
│   ├── minion-core/          # lib: loop, messages, tool traits, policy  (no I/O deps on terminal)
│   ├── minion-provider/      # OpenAI-compatible client (SSE, retries, usage)
│   ├── minion-tools/         # built-in tools + registry
│   ├── minion-store/         # SQLite schema, migrations, repositories
│   ├── minion-cron/          # scheduler + catch-up policy
│   ├── minion-mcp/           # MCP server + client (rmcp)
│   └── minion-cli/           # bin: clap, REPL frontend, subcommands
└── tests/                    # cross-crate integration + fixtures
```

Rationale: `minion-core` has zero terminal and zero network dependencies, so the loop is unit-testable with a scripted mock provider.

### 4.1 Dependencies (indicative)

| Concern | Crate |
|---|---|
| Async runtime | `tokio` (rt-multi-thread, process, signal, sync, time) |
| CLI | `clap` (derive) |
| Serialization | `serde`, `serde_json`, `toml` |
| Schemas | `schemars` (derive tool JSON schemas from Rust types) |
| HTTP + SSE | `reqwest` (rustls), `eventsource-stream` or manual SSE line parser |
| SQLite | `rusqlite` (`bundled`) + a small pool |
| MCP | `rmcp` (stdio server + client) |
| REPL | `rustyline` (history, line editing, no alternate screen) |
| Cron | `cron` (parse) + custom tick loop |
| Time | `chrono`, `chrono-tz` |
| Paths | `directories` |
| Globbing | `globset`, `ignore` |
| Diffing | `similar` |
| Errors | `thiserror` (libraries), `anyhow` (binary) |
| Logging | `tracing`, `tracing-subscriber` |
| IDs | `uuid` (v7) |
| Testing | `insta`, `wiremock`, `assert_cmd`, `proptest`, `tempfile` |

---

## 5. Detailed design

### 5.1 Configuration

**Precedence** (highest first): CLI flags → env vars (`MINION_*`) → project `minion.toml` → user config → built-in defaults.

Locations: `$XDG_CONFIG_HOME/minion/config.toml` (or `~/Library/Application Support/minion/` on macOS), plus `./minion.toml` in the workspace.

```toml
[provider]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"   # name only; the value is never stored or logged
api_key_file = "~/.config/minion/credentials"  # the value itself lives here, 0600
model = "gpt-4.1-mini"
temperature = 0.2
stream = true
request_timeout_secs = 120
max_retries = 3
# provider quirks
supports_usage_in_stream = true
parallel_tool_calls = true

# Extra headers sent on every request. A value may contain `${session}`, which
# expands to the stable identifier of the current conversation. Gateways that
# pin a conversation to one upstream need this; OpenCode Go rejects requests
# without it.
[provider.headers]
"x-opencode-session" = "${session}"

[agent]
system_prompt_file = "~/.config/minion/system.md"
max_iterations = 25
max_tokens_per_turn = 200_000
history_window = 40            # messages; older ones are summarized or dropped
summarize_on_truncate = true

[workspace]
roots = ["."]                  # path guard boundary; canonicalized at load
read_only_roots = []
max_file_bytes = 2_097_152
follow_symlinks = false

[exec]
default_timeout_secs = 120
max_timeout_secs = 3600
output_cap_bytes = 262_144
shell = "/bin/sh"
login_shell = false

[policy]
default = "ask"                # auto | ask | deny
noninteractive = "deny"        # policy when stdin is not a TTY
allow = [
  { tool = "run_command", pattern = "ls *",   scope = "session" },
  { tool = "run_command", pattern = "git status", scope = "always" },
]
deny = [
  { tool = "run_command", pattern = "*sudo*" },
  { tool = "run_command", pattern = "*rm -rf /*" },
]

[http_fetch]
allowed_domains = ["docs.rs", "*.github.com"]
block_private_ips = true
max_bytes = 1_048_576
timeout_secs = 20

# Optional System One judge for a flagged `run_command` (M3.5, D16). Off by
# default: it is a network call carrying command text to a third party.
[guard]
enabled = false
base_url = "http://127.0.0.1:8081"   # server root; the request goes to {base_url}/v1/systemone
allow_threshold = 0.25               # at or below this, a verdict resolves the prompt
deny_threshold = 0.75                # above this, a confident refusal; the band between prompts too
timeout_secs = 10

[cron]
enabled = true
timezone = "America/Mexico_City"
missed_run_policy = "run_once"   # skip | run_once | run_all
max_concurrent_jobs = 2
missed_run_cap = 20              # ceiling on the runs one run_all catch-up may start

[mcp.server]
enabled = true
transport = "stdio"
expose_exec = false              # run_command over MCP defaults OFF
expose_write = false
expose_cron_write = true

[mcp.client.servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/data"]
approval = "ask"
tool_allow = ["read_file", "list_directory"]

[logging]
level = "info"
format = "text"                  # text | json
file = "~/.local/state/minion/minion.log"
redact_env = true
```

**Secrets.** The config records only the *names* of where a secret comes from. The value is read at
run time, held in a `SecretString`-like wrapper that has no `Debug`/`Display`, and never written to
logs, SQLite, or a config file. `init` stores the value it collects in a separate `0600`
credentials file, so a config may be committed while the secret never is.

**Credential resolution.** `Config::api_key()` is tried in order:

1. `provider.api_key_env` — if the named variable is set and non-empty, it wins. This is what lets a
   single shell or CI job override a stored key without editing anything.
2. `provider.api_key_file` — a TOML file holding `api_key = "…"`, written by `minion init`.
3. Otherwise an error naming both sources, *unless both are empty*, which is how a keyless local
   backend is expressed.

Both keys are strings rather than optionals, because "unset" and "empty" must mean the same thing:
an empty value must override the built-in default rather than fall back to it. A keyless backend
therefore has to set **both** to `""` — clearing only `api_key_env` would leave the default
credentials path active and the backend would still appear to need a key.


#### 5.1.1 `minion init` — configuring the model backend

`init` is the supported way to produce a first config. It is an opinionated wizard over the schema
above: it asks only for what the provider layer actually needs, then writes one file.

**Question flow** (interactive, on a TTY):

1. **Backend preset**:

   | Preset | `base_url` | `api_key_env` | Notes |
   |---|---|---|---|
   | OpenAI | `https://api.openai.com/v1` | `OPENAI_API_KEY` | default |
   | OpenRouter | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` | |
   | OpenCode Go | `https://opencode.ai/zen/go/v1` | `OPENCODE_API_KEY` | sets `x-opencode-session = "${session}"` |
   | OpenCode Zen | `https://opencode.ai/zen/v1` | `OPENCODE_API_KEY` | same session header |
   | Ollama (local) | `http://localhost:11434/v1` | *none* | no key required |
   | vLLM / LM Studio (local) | `http://localhost:8000/v1` | *none* | |
   | Custom | free text | free text or none | |

2. **Base URL** — prefilled from the preset, editable.
3. **API key environment variable** — the *name* to record, prefilled and editable. Local presets may
   answer *none* (flag: `--no-api-key`), which clears the name. The value is never requested here.
4. **API key** — the real token, asked for once, on a TTY. Echo is disabled *before* the prompt is
   printed, so even a paste cannot put it into terminal scrollback. Pressing Enter skips it and falls
   back to the environment variable. `--no-store-token` keeps the value in memory for discovery and
   `--check` without persisting it.
5. **Model** — if `GET {base_url}/models` succeeds the results are offered as a picker; otherwise
   the user types an id. Discovery uses the token just typed, so setup works before anything has been
   exported. A failed listing is a warning, never an error.
6. **Workspace root** — default `.`, written into `workspace.roots`.
7. **Confirmation** showing the target path and a summary of the values about to be written.

**Where the key is stored.** The token goes to a `0600` credentials file beside the user config
(`$XDG_CONFIG_HOME/minion/credentials`), created inside a `0700` directory. The config is written
*without* the value, so `--project` output stays committable. `--credentials-file` chooses a
different location, and that path is then recorded as `provider.api_key_file` so the two cannot
disagree. In the default case the path is left unstated, because a machine-specific path in a shared
config would be noise at best and misleading at worst.

**Extra headers.** `--header "Name: value"` may be repeated and is recorded under
`[provider.headers]`; a header supplied this way overrides the preset's value of the same name.
This is how a backend whose gateway requires a routing header — OpenCode Go's
`x-opencode-session` — is configured without a bespoke code path:

```
minion init --non-interactive --preset custom \
  --base-url https://gateway.internal/v1 --model internal-1 \
  --api-key-env GATEWAY_KEY --header 'x-opencode-session: ${session}'
```

**Target file.** The user config path from §5.1 by default; `--project` writes `./minion.toml`
instead (for a shared, secret-free setup that can be committed). Parent directories are created
`0700` and the file is created `0600`. Writes are atomic (temp file in the same directory, then
rename), so an interrupted `init` never leaves a partial config.

**Refusing to clobber.** If the target already exists, `init` prints a diff of the keys it would
change and exits `4` without writing, unless `--force` is passed.

**Non-interactive form.** Every prompt has a flag, so `init` is usable from provisioning scripts:

```
minion init --non-interactive \
  --base-url http://localhost:11434/v1 \
  --model llama3.1 --no-api-key --project --force
```

**Validation.** `init --check` probes `GET {base_url}/models` (falling back to a single-token
completion when the endpoint does not implement `/models`) *before* writing. On failure it writes
nothing and exits `3` with the provider's own message, so a misconfigured backend never produces a
config that merely looks correct.

**Credential warning.** If a credential is named but nothing can be found — the env var is unset and
no token was just stored — the config is still written, but the gap is printed, and `minion doctor`
repeats it. Storing a token *is* a credential, so a key saved moments earlier must not trigger this
warning.

**`--json`.** Emits `{"type":"init","path":…,"credentials":…,"provider":{…},"check":"ok|skipped|failed"}`
so scripted setups can consume the result. It reports the credentials *path*; the value is never in
the output.

**Relationship to `config`.** `minion config init` is retained as an alias for `minion init`;
`minion config show|path` stay read-only.

### 5.2 Provider client

A thin, hand-rolled client rather than a heavy SDK, for control over base URLs and provider quirks.

```rust
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Option<Vec<ToolSchema>>,
    pub tool_choice: ToolChoice,          // Auto | None | Required | {name}
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub stream: bool,
    pub parallel_tool_calls: Option<bool>,
    pub stream_options: Option<StreamOptions>, // include_usage
}

pub enum ChatEvent {
    TextDelta(String),
    ToolCallDelta { index: usize, id: Option<String>, name: Option<String>, args_fragment: String },
    Usage(Usage),
    Done { finish_reason: FinishReason },
}

pub trait Provider: Send + Sync {
    fn stream(&self, req: ChatRequest, cancel: CancellationToken)
        -> impl Stream<Item = Result<ChatEvent>>;
}
```

**Behaviors**

| Aspect | Decision |
|---|---|
| Transport | `POST {base_url}/chat/completions`, `Accept: text/event-stream` |
| SSE parsing | Line-buffered; ignore comment/keepalive lines; tolerate missing `[DONE]` |
| Tool-call assembly | Accumulate fragments by `index`; concatenate `arguments` strings before JSON parse |
| Malformed args | Return a `tool` message with `{"error":"invalid_json"}` so the model can self-correct |
| Retries | Only before any delta is emitted; never mid-stream, never for non-idempotent tools |
| Backoff | `min(2^attempt * 250ms, 8s)` + jitter; honor `Retry-After` |
| Errors | Map HTTP/JSON failures to typed errors: `Auth`, `RateLimit{retry_after}`, `BadRequest{body}`, `Transport`, `Provider{status,body}` |
| Timeouts | Connect 10 s, idle-stream 60 s (resets per delta), total `request_timeout_secs` |
| Custom headers | `[provider.headers]` is applied to every request; `${session}` expands to the conversation id |
| Authentication | `Authorization: Bearer` is sent only when a credential is resolved; `api_key_env` first, then `api_key_file`. Both empty means a keyless backend |
| Model discovery | `GET {base_url}/models` returns the advertised ids, used by `init` for the picker and by `--check` |

#### 5.2.1 Conversation identity and `${session}`

Some gateways need to recognise that two requests belong to the same conversation, so they can pin
it to one upstream and reuse its prompt cache. OpenCode Go is the concrete case: it requires an
`x-opencode-session` header on every request and rejects calls that omit it.

Rather than hard-coding a provider-specific header, the client exposes one generic mechanism:

1. Every session gets an identifier from `new_session_id()` — a v7 UUID, so it is unique and also
   sorts by creation time.
2. The id is stable for the whole conversation: the REPL keeps one for its lifetime, `run` uses one
   per invocation, and M1 persists it as `sessions.id`.
3. Configured header values containing `${session}` are expanded with it at request time.

A header whose value is empty *after* expansion is dropped rather than sent blank, so a config that
references `${session}` still works for a one-off call that has no session.

This keeps two properties: the mechanism is provider-agnostic (any header can use the placeholder),
and the identifier is never invented per request — it cannot be, or prompt-cache routing would break.

### 5.3 Agent loop

```rust
pub enum StopReason { Completed, IterationLimit, TokenBudget, Cancelled, ProviderError }

pub struct TurnOutcome {
    pub final_text: Option<String>,
    pub stop: StopReason,
    pub usage: Usage,
    pub iterations: u32,
}
```

Rules:

- The system prompt is assembled once per turn: static persona + a **policy digest** (workspace roots, non-interactive status, available tools, notable denies) so the model knows what it may do.
- History is trimmed to `history_window`; if `summarize_on_truncate`, older turns are replaced by a generated summary message flagged `[summary]`.
- Tool results are `role:"tool"` messages with `tool_call_id`; every assistant `tool_calls` entry must get exactly one matching result, or the request is invalid and the loop repairs it with a synthetic error result.
- Token budget is checked after each provider response; exceeding it stops with `TokenBudget`.
- Cancellation is cooperative: the `CancellationToken` is checked between iterations and passed into every tool.

### 5.4 Tool framework

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;          // from schemars
    fn risk(&self) -> Risk;                          // ReadOnly | Write | Execute | Network
    fn default_timeout(&self) -> Duration;
    async fn invoke(&self, ctx: ToolCtx, args: serde_json::Value) -> Result<ToolOutput>;
}

pub struct ToolOutput {
    pub content: String,       // fed to the model
    pub truncated: bool,
    pub metadata: serde_json::Value, // e.g. exit code, bytes, duration
}

pub struct ToolCtx {
    pub workspace: WorkspaceGuard,
    pub store: Arc<Store>,
    pub cancel: CancellationToken,
    pub session_id: Uuid,
    pub progress: mpsc::Sender<Progress>,
}
```

Invariants enforced by the registry, not by individual tools:

- Args are validated against `schema()` before `invoke`; failures produce a tool-level error result.
- Output is truncated to `output_cap_bytes` with a `truncated` flag and a hint appended.
- Every invocation is wrapped in `tokio::time::timeout` and tied to the cancellation token.
- Every invocation emits a `tracing` span and an audit row.

### 5.5 Tool specifications

#### `run_command`

| Field | Spec |
|---|---|
| Risk | Execute |
| Params | `command: string` (required), `cwd?: string`, `timeout_ms?: int`, `stdin?: string` |
| Execution | `<shell> -c <command>` (or `-lc` if `login_shell`); new process group so the whole tree can be killed |
| Returns | `{ exit_code, stdout, stderr, duration_ms, truncated }` |
| Caps | `output_cap_bytes` per stream; `max_timeout_secs` clamps whatever the model asks for |
| Kill | On timeout or cancel: `SIGTERM` to the process group, then `SIGKILL` after 5 s |
| Env | Not settable by the model. A free-form `env` map is a way to smuggle values into a child; a fixed allowlist can come later if something needs it. |
| Approval | At least `ask` unless an allowlist pattern matches. Destructive, privileged, and network-pipe commands stay at `ask` even when `policy.default = "auto"`. |
| Notes | Streaming output options: `--stream` shows live output to stderr without entering model context |

#### `read_file` / `write_file` / `edit_file`

| Field | Spec |
|---|---|
| Risk | ReadOnly / Write / Write |
| Path guard | Canonicalize; must be inside a `workspace.roots` entry; reject symlink escape when `follow_symlinks = false`; reject non-UTF-8 by default |
| `read_file` | Params `path`, `offset?`, `limit?`; returns numbered lines; caps at `max_file_bytes` |
| `write_file` | Params `path`, `content`, `create_dirs?: bool`, `overwrite?: bool` (default false) |
| `edit_file` | Params `path`, `old_string`, `new_string`, `replace_all?: bool`; exact-match, must be unique unless `replace_all`; returns a unified diff |
| Atomicity | Write to temp file in the same directory, `fsync`, then `rename` |
| Approval | Writes `ask` by default; in-workspace writes may be allowlisted |

#### `apply_patch`

`edit_file` asks the model to reproduce one small snippet verbatim. For a change spread over several
sites, or across several files, that is both verbose and the most common way a model goes wrong. So
`apply_patch` exists alongside it: a set of *anchored operations* applied in one atomic call.

The format is structured JSON rather than a textual diff, so every operation is self-describing and
failures are exact instead of "patch did not apply":

```json
{
  "files": [
    {
      "path": "src/store.rs",
      "operations": [
        { "op": "replace",       "old": "let x = 1;", "new": "let x = 2;" },
        { "op": "insert_before", "anchor": "fn main() {", "new": "    setup()?;\n" },
        { "op": "insert_after",  "anchor": "use std::io;", "new": "use std::path::Path;" },
        { "op": "delete",        "old": "    // TODO: remove\n" }
      ]
    },
    { "path": "tests/store.rs", "operations": [ ... ] }
  ]
}
```

| Rule | Behaviour |
|---|---|
| Anchor | The operation's `old`/`anchor` snippet must match **exactly once**. A unique anchor is required; guessing between several matches is how a patch corrupts a file. |
| `count` | Optional. When given, the snippet must match exactly `count` times and all are replaced. Any other number is an error. |
| `replace` | Swap `old` for `new`. |
| `insert_before` / `insert_after` | Insert `new` relative to a unique anchor. |
| `delete` | Remove `old`. |
| Atomicity | Every operation is computed against the original text, and files are written only if **all** operations across **all** files succeed. A failure in operation 3 of 5 leaves every file untouched. |
| Ordering | Operations apply in order to the accumulating result, so a later operation may anchor on text an earlier one inserted. |
| Existence | Target files must already exist; creating a file is `write_file`'s job. |
| Guards | Same path guard and `max_file_bytes` cap as `write_file`. |
| Returns | A unified diff per file, plus the count of operations applied. |
| Approval | `Write`. One approval covers the whole patch, not one per operation. |

`edit_file` is retained: for a one-line change it is a smaller, less error-prone request than a patch.

#### `cron_add` / `cron_list` / `cron_remove`

| Field | Spec |
|---|---|
| Risk | Write |
| `cron_add` | Params `name?`, `schedule` (5-field cron), `prompt`, `cwd?`, `timezone?`, `session_mode?` (`reuse` \| `new`), `max_runs?`, `allow_overlap?` |
| `cron_list` | Returns jobs with `next_run_at`, `last_status` |
| `cron_remove` | Params `id` or `name` |
| Validation | Cron expression parsed before insert; next run computed; reject if never fires |
| Approval | `ask` for add/remove; list is `auto` |

#### `http_fetch`

| Field | Spec |
|---|---|
| Risk | Network |
| Params | `url`, `method?: GET\|POST\|PUT\|DELETE`, `headers?`, `body?`, `max_bytes?` |
| Guards | Scheme must be `https` (or `http` only if the host is explicitly allowlisted); domain must match `allowed_domains` globs; resolve DNS and reject private/loopback/link-local ranges when `block_private_ips`; no cross-domain redirects unless allowlisted; response capped |
| Returns | `{ status, headers, body, truncated }` |
| Approval | `auto` if domain allowlisted, otherwise `ask` |

#### `remember` / `recall`

| Field | Spec |
|---|---|
| Risk | Write / ReadOnly |
| `remember` | Params `key`, `value`, `tags?: string[]`; upsert into `memory` (namespace = workspace) |
| `recall` | Params `query`, `limit?`; SQLite FTS5 over `key`, `value`, `tags`; returns ranked snippets |
| Notes | Not a vector store; deterministic and inspectable. Useful for cross-session facts and preferences. |

#### `mcp_call`

> **Not implemented (M5).** The flattened `mcp__<server>__<tool>` tools below are the surface that
> ships; `mcp_call` is deferred because the per-server approval policy keys on a tool name and this
> tool would have to be special-cased to read `args.server` for its decision. See §5.10.

| Field | Spec |
|---|---|
| Risk | Inherited from the target tool's declared risk, minimum `Network` |
| Params | `server`, `tool`, `arguments` |
| Guards | Server must be configured; tool must be in `tool_allow` for that server; approval per server config |
| Returns | Raw MCP tool result content, size-capped |
| Discovery | External tools are also surfaced directly to the model as `mcp__<server>__<tool>` so the model can call them without the indirection; `mcp_call` exists as a generic escape hatch and for models that prefer a compact tool list |

### 5.6 Approval engine

Policy resolution for each invocation:

```
deny-rule match            → Deny
allowlist match (any scope)→ Auto
tool.risk == ReadOnly      → Auto
!stdin.is_tty()            → policy.noninteractive (default Deny)   -- for everything else
tool.risk == Execute|Write → Ask
otherwise                  → policy.default
```

`ReadOnly` is allowed before the TTY is consulted, so a piped process can still read. `Network` is
gated with `Execute`/`Write` rather than with `ReadOnly`, because the request leaves the machine and
can be induced by untrusted content. See D15.

**Optional System One guard (M3.5, D16).** When `[guard].enabled` is set and a flagged `run_command`
falls in an eligible category, the engine may consult a `/v1/systemone` model before deciding to
prompt. It sits between rule 3 and rule 4 and can only convert a prompt into a silent allow:

```
classifier flags an ineligible category   → Ask, with no network call
classifier flags an eligible category    → guard; allow at/below allow_threshold, else Ask
```

Deny rules and allowlists are unaffected and still run first, and a guard error resolves to `Ask`
rather than `Ok`. FR-43 through FR-47 are the normative statement.

**Per-family fallbacks (M5, D21).** The engine can also carry a list of `ToolPolicy { prefix, decision }`
that substitutes the *fallback* rules 5 and 6 for a family of tools. It is how an MCP server gets its
own approval policy: the client registers one entry per server, keyed on the `mcp__<server>__` prefix
its tools carry, so a trusted local server can be `auto` while a network server stays `ask`. Only the
fallback moves — deny rules, allow rules and the classifier still run first, and the longest matching
prefix wins so the outcome does not depend on the order the servers were configured in.

The guard is consulted only where a prompt would otherwise be shown. A non-interactive run has no
prompt to resolve, so it never reaches the model and still takes `policy.noninteractive` (D15): a
silent allow there would widen a decision the static rules already made. Eligibility is a static
floor — `privilege`, `remote-execution` and `destructive` are resolved in-process, so an ineligible
command never leaves the machine (FR-44). The request is `POST {base_url}/v1/systemone` with
`{"state": "<command>"}` and nothing else (FR-46); the reply is a number, `{"unsafe": <0..1>}`, and
the thresholds are applied here, in our code (D16). Bands are audited as `guard_allow`,
`guard_uncertain`, `guard_deny`, and a guard that fails is audited as `guard_error` before the
ordinary prompt takes over (FR-45, FR-47).

Deny rules always win; an explicit deny cannot be allowlisted away at runtime.

Interactive prompt:

```
⚠  run_command requests execution
   $ rm -rf ./build
   risk: execute   cwd: /Users/me/proj

   [o]nce  [s]ession  [a]lways  [d]eny  [e]dit
```

- `once` → this call only.
- `session` → in-memory pattern for this process.
- `always` → persisted to `approvals` with a normalized pattern derived from the command's first token/verb (e.g. `rm *` is *not* derivable; the UI shows the exact pattern being persisted and requires confirmation).
- `edit` → hand the command back to the user as a shell-escape so they can run it themselves.

Risk classification is a small, auditable rule set, not a whitelist of everything:

```
Execute patterns requiring explicit consent:
  - filesystem destructive: rm -r, dd, mkfs, shred, truncate on existing paths
  - privilege: sudo, doas, su, chown, chmod with setuid/777
  - remote execution: curl|wget piped to sh, bash -c from network input
  - version control history: git push --force, git reset --hard, filter-branch
  - package/global mutation: npm -g, brew install, apt/pip system installs
  - process control: kill -9 of non-child pids, systemctl
  - anything writing outside workspace roots
```

### 5.7 Cron scheduler

- A single task ticks every 1000 ms (aligned to the next second boundary) and queries `jobs WHERE enabled = 1 AND next_run_at <= now`.
- Firing: insert a `job_runs` row with status `running` **before** dispatch, then dispatch a prompt into the agent loop as a synthetic user message tagged `[cron:<name>]`.
- Session mode: `new` creates a fresh session per run (default for runaway isolation); `reuse` appends to a named session so the job accumulates context.
- Concurrency capped by `max_concurrent_jobs`; excess due jobs queue in memory and are recorded as `queued`.
- On completion, `next_run_at` is recomputed from the cron expression and the job is `UPDATE`d in the same transaction as the run's terminal status.
- **Catch-up on startup:** for each job with `next_run_at` in the past, apply `missed_run_policy`:
  - `skip` — recompute `next_run_at` and record a `skipped` run.
  - `run_once` — run once, then recompute (default).
  - `run_all` — run each missed occurrence, bounded by a safety cap (e.g. 20).
- Preventing overlap: a job that is still `running` when due is marked `skipped (overlap)` unless it has `allow_overlap = true`.
- Job prompts run with the **non-interactive** policy: they cannot prompt for approval, so any tool that would `ask` is denied unless allowlisted. This is intentional and documented.
- `missed_run_policy` is a `[cron]` setting rather than a per-job one: the `jobs` table has no column for it, and adding one would mean a migration for a knob nobody has asked to vary per job.

**Fire semantics (as implemented, M4).** A fire *consumes* its occurrence: `next_run_at` is advanced to the next occurrence and `last_run_at`/`last_status` are set in the same transaction that inserts the `job_runs` row. Without that, a job whose run outlasts its own interval would look due on every tick. Completion then recomputes `next_run_at` from the cron expression **and the completion instant**, which can only move it later, and writes it in the same transaction as the run's terminal status — the two rules together are what §5.7's "on completion" sentence means in practice.

The statuses are used distinctly: `skipped` is a catch-up decision, `overlap` is a live run colliding with its own next occurrence, `queued` is an accepted run waiting for a concurrency slot, and `running`/`ok`/`failed` are the run's own life. A `reuse` job's session id is written on completion as a *separate* statement: `jobs.session_id` is a foreign key, and an adoption that fails must not roll back the run's terminal status and leave the row claiming to be running forever.

Catch-up replays occurrences with no overlap check, because the occurrences being replayed never ran — there is no live run to collide with. The `max_concurrent_jobs` cap still applies, so a `run_all` burst is bounded twice: by `missed_run_cap` and by the queue.

### 5.8 SQLite store

Location: `$XDG_STATE_HOME/minion/minion.db` (or macOS Application Support). WAL mode, `foreign_keys=ON`, `busy_timeout=5000`, `synchronous=NORMAL`.

```sql
CREATE TABLE schema_migrations (
  version    INTEGER PRIMARY KEY,
  applied_at TEXT NOT NULL
);

CREATE TABLE sessions (
  id          TEXT PRIMARY KEY,          -- uuid v7
  title       TEXT,
  cwd         TEXT NOT NULL,
  model       TEXT,
  provider    TEXT,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  meta        TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE messages (
  id           TEXT PRIMARY KEY,
  session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq          INTEGER NOT NULL,
  role         TEXT NOT NULL CHECK (role IN ('system','user','assistant','tool')),
  content      TEXT,
  tool_calls   TEXT,                     -- JSON array, assistant only
  tool_call_id TEXT,                     -- tool role only
  tool_name    TEXT,
  is_summary   INTEGER NOT NULL DEFAULT 0,
  tokens_in    INTEGER,
  tokens_out   INTEGER,
  created_at   TEXT NOT NULL,
  UNIQUE (session_id, seq)
);
CREATE INDEX idx_messages_session ON messages(session_id, seq);

CREATE TABLE jobs (
  id               TEXT PRIMARY KEY,
  name             TEXT UNIQUE,
  schedule         TEXT NOT NULL,        -- 5-field cron
  timezone         TEXT NOT NULL,
  prompt           TEXT NOT NULL,
  cwd              TEXT NOT NULL,
  session_id       TEXT REFERENCES sessions(id) ON DELETE SET NULL,
  session_mode     TEXT NOT NULL DEFAULT 'new',
  enabled          INTEGER NOT NULL DEFAULT 1,
  allow_overlap    INTEGER NOT NULL DEFAULT 0,
  max_runs         INTEGER,
  runs_count       INTEGER NOT NULL DEFAULT 0,
  created_at       TEXT NOT NULL,
  last_run_at      TEXT,
  next_run_at      TEXT,
  last_status      TEXT
);
CREATE INDEX idx_jobs_due ON jobs(enabled, next_run_at);

CREATE TABLE job_runs (
  id           TEXT PRIMARY KEY,
  job_id       TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  session_id   TEXT REFERENCES sessions(id) ON DELETE SET NULL,
  started_at   TEXT NOT NULL,
  finished_at  TEXT,
  status       TEXT NOT NULL,            -- queued|running|ok|failed|skipped|overlap
  exit_summary TEXT,
  output_ref   TEXT
);
CREATE INDEX idx_job_runs_job ON job_runs(job_id, started_at DESC);

CREATE TABLE approvals (
  id          TEXT PRIMARY KEY,
  tool        TEXT NOT NULL,
  pattern     TEXT NOT NULL,
  scope       TEXT NOT NULL,             -- workspace path
  decision    TEXT NOT NULL CHECK (decision IN ('allow','deny')),
  created_at  TEXT NOT NULL,
  expires_at  TEXT,
  UNIQUE (tool, pattern, scope, decision)
);

CREATE TABLE memory (
  id         TEXT PRIMARY KEY,
  namespace  TEXT NOT NULL,              -- workspace hash
  key        TEXT NOT NULL,
  value      TEXT NOT NULL,
  tags       TEXT NOT NULL DEFAULT '[]',
  session_id TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (namespace, key)
);

CREATE VIRTUAL TABLE memory_fts USING fts5(
  key, value, tags, content='memory', content_rowid='rowid'
);

CREATE TABLE audit_log (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  ts         TEXT NOT NULL,
  session_id TEXT,
  turn_id    TEXT,
  tool       TEXT,
  risk       TEXT,
  decision   TEXT,                       -- auto|allow_once|allow_session|allow_always|deny
  args_digest TEXT,                      -- hash, not raw args, for commands
  outcome    TEXT,
  duration_ms INTEGER
);
```

Notes:
- `output_ref` points at a file under the state dir when a job's output exceeds an inline cap.
- `args_digest` stores a hash of sensitive arguments so the audit trail is useful without persisting secrets.
- Messages are append-only; the `seq` counter is allocated inside the transaction that writes the message.

**Migrations.** A forward-only list in `minion-store::migrate`, each entry applied once and recorded
in `schema_migrations`. A database newer than this build understands is refused rather than read
optimistically, and `PRAGMA quick_check` runs before any migration so a corrupt file fails loudly
instead of half-initialising. Tables that later milestones own are created up front, with the FTS
triggers that keep `memory_fts` in step — without those, `recall` would silently match nothing in M3.

**Write semantics.** A turn is persisted as one transaction: the user message, every assistant and
tool message, and the session's `updated_at` either all land or none do, so a transcript can never
contain half a turn. A hard crash mid-turn loses that turn; nothing before it.

**History windowing.** Reloading a long transcript trims to `agent.history_window`, but index 0 (the
system prompt) is configuration rather than history and is always retained. The cut is advanced to a
`user` message, and never left sitting on a `tool` result: an assistant `tool_calls` message whose
results were dropped is rejected by the provider, so a turn must never be split.

### 5.9 MCP server

The server task is started with `minion mcp serve` (stdio). It uses `rmcp`'s server trait and shares the same core loop, store, and policy engine.

**Exposed tools** (default surface):

| Tool | Purpose | Default gate |
|---|---|---|
| `agent_ask` | Run one agent turn; params `prompt`, `session_id?`, `model?`, `max_iterations?`, `allow_tools?` | `auto` (read-only tool subset) |
| `agent_list_sessions` | Enumerate sessions | `auto` |
| `agent_get_session` | Fetch transcript | `auto` |
| `agent_list_tools` | Introspect enabled tools + risk classes | `auto` |
| `cron_add` / `cron_list` / `cron_remove` | Manage jobs | `auto` if `expose_cron_write`, else denied |
| `agent_run_command` | Shell execution | **denied unless `mcp.server.expose_exec = true`** |
| `agent_write_file` | File writes | **denied unless `mcp.server.expose_write = true`** |

**Resources:** `minion://sessions`, `minion://sessions/{id}`, `minion://jobs`, `minion://config-redacted`.

**Prompts:** `minion_agent` — a parameterized prompt that frames a task for the agent, so hosts that only support prompts (not tools) can still use minion.

**Protocol posture:** read-only by default; write/exec capabilities are opt-in flags that are printed loudly at server startup, so an operator cannot enable them unknowingly.

**As implemented (M6).** The server is `minion mcp serve` over stdio, in
`minion-cli::mcp_serve`. It assembles the same pieces a session does — the same
store, the same built-in tool registry, the same `PolicyEngine` — and it never
starts a REPL, because both own stdin/stdout (R5). A turn's output goes nowhere
but the protocol, so diagnostics stay on stderr.

- **The surface, name for name.** `agent_ask`, `agent_list_sessions`,
  `agent_get_session`, `agent_list_tools`, then `cron_add`/`cron_list`/
  `cron_remove` when `expose_cron_write`, then `agent_run_command` and
  `agent_write_file`. `agent_ask`'s inner agent gets the **read-only subset** of
  the built-in registry, chosen by the risk class itself rather than by a
  hand-kept list, and `allow_tools` can narrow it further (an unknown name is
  dropped, fail-closed). So the default surface observes and nothing else.
- **A disabled capability is denied, not hidden.** `agent_run_command` and
  `agent_write_file` are always listed. Without their flag the gate carries a
  deny rule that refuses every call, and the refusal is written to `audit_log`
  like any other decision; the caller receives it as a readable tool error, not
  an opaque protocol error. Hiding the tool would leave the host guessing at
  what the server can do.
- **The gate is non-interactive, and an enabled family substitutes the fallback.**
  stdin is the protocol pipe, so there is no terminal to prompt on: an enabled
  family gets a `ToolPolicy` that resolves to `auto` (D21) and everything else
  takes `policy.noninteractive`. Deny and allow rules run first as always, which
  is why `agent_run_command` is decided under the name `run_command` — an
  operator's existing `run_command` rules protect the MCP surface too, and the
  command classifier still sees the command.
- **Resources and prompt.** `minion://sessions`, `minion://jobs`,
  `minion://config-redacted` and the `minion://sessions/{id}` template; the
  `minion_agent` prompt frames a task for a host that has prompts but no tools.
  `config-redacted` serialises the effective config with header values whose
  *name* looks like a credential replaced by `<redacted>`, so the resource cannot
  become an exfiltration path for a hand-written config.
- **The scheduler does not run here.** A job added over MCP is stored in the
  same database and runs the next time a process with a scheduler opens it.
  `mcp serve` is a protocol server, not a daemon; starting a scheduler inside it
  is a separate decision and is not part of this milestone.

### 5.10 MCP client

- Servers from `[mcp.client.servers.*]` are spawned at startup (lazily on first use if `lazy = true`).
- `tools/list` results are flattened into the registry as `mcp__<server>__<tool>`, with `inputSchema` passed through verbatim (minion does not rewrite third-party schemas).
- `tool_allow` per server filters what reaches the model; anything not listed is invisible and uncallable.
- Per-server approval policy overrides the global default, so a trusted local server can be `auto` while a network server is `ask`.
- Server failures degrade gracefully: the tools are removed from the catalog and a `system` notice explains why. They are retried on the next turn.
- Tool name collisions with built-ins are resolved by prefixing, never shadowing: built-ins always win.

**Configuration.** Each `[mcp.client.servers.<name>]` entry is `command`, `args`, `lazy`, `tool_allow`
and `approval`. The table key is the namespace: it names every tool of that server, and a key containing
`__` is rejected at load time rather than left to collide with another server's. `command` is spawned
directly, never through a shell, so an argument cannot become a second command.

```toml
[mcp.client.servers.files]
command = "mcp-server-files"
args = ["--root", "/srv"]
lazy = true                       # contact it on the first turn, not at startup
tool_allow = ["read_*", "list"]   # empty allows nothing; ["*"] allows everything
approval = "auto"                 # this server's tools substitute the global default
```

**What is published, and what is gated (as implemented, M5).** An external tool is an ordinary `Tool`:
the same registry, the same `Agent::dispatch`, and the same approval gate as `run_command`. There is no
path from the model to a server that skips a policy decision. Its risk class is always `Network` —
`§5.5` says "inherited from the target tool's declared risk, minimum `Network`", and since MCP states
risk only in `ToolAnnotations`, which are *hints from a server minion does not vouch for*, an inherited
hint could only ever lower the class. With the floor at `Network` the honest reading is the floor
itself (D21, T6). `tool_allow` is applied before the catalogue is built, so a tool the operator did not
name is absent from the schemas the model receives *and* unresolvable by name; there is one thing the
operator has to write to widen it, and it is in the config.

**The catalogue is live, not a snapshot.** `minion-core` exposes a `ToolCatalog` trait, and the
registry consults it on every read of the tool list. That is what makes §5.10's "retried on the next
turn" real: at the start of each turn the client re-attempts every server that is not up, and a server
that answers publishes its tools into the catalogue the model is about to be offered — through the
same frozen `Arc<ToolRegistry>` the session was built with. Ordering is the shadowing rule: registered
tools come first and a name already taken is skipped, so an `mcp__…` tool can never displace a
built-in, whatever a server calls itself.

**`lazy`.** A lazy server is not contacted while the session is assembled; the first turn does it
(`OnStart::Eager` versus `OnStart::All`). Its tools still reach the catalogue from that point, because
a tool list cannot be discovered without a connection — `lazy` buys a cheaper session start, not a
hidden tool set. `minion mcp list` and `minion mcp tools <server>` are explicit uses and start a lazy
server immediately, which is also how an operator inspects one.

**Failure.** A server that cannot be started costs its tools and nothing else: the turn completes, the
tools are gone from the catalogue, and the reason arrives as a `system` message. Notices are emitted on
*transitions* only, so a server that stays down is explained once rather than on every turn.

**Shutdown.** `minion` closes its MCP connections on the way out of both the REPL and a one-shot run,
so a session does not leave server processes behind. A hard kill (SIGKILL) can still orphan a child;
that is documented rather than hidden.

**`mcp_call` is not implemented.** §5.5 lists it as a generic escape hatch for models that prefer a
compact tool list. The flattened tools are the primary surface this milestone delivers, and a generic
`mcp_call(server, tool, …)` would need a policy decision that the per-server policy cannot express —
the gate decides on a tool name, and `mcp_call` would have to be special-cased to read `args.server`.
Deferred rather than half-gated; see AGENTS.md's known gaps.

### 5.11 Frontend (fx-like REPL)

**Philosophy:** the terminal is a scrollback, not a canvas. No alternate screen, no full redraw, no mouse capture.

```
$ minion
minion 0.1 · gpt-4.1-mini · /Users/me/proj · policy: ask

› summarize the TODOs in this repo and open a job to nag me weekly

  ▸ recall {"query":"todo conventions"}            auto      12ms
  ▸ run_command {"command":"rg -n TODO --stats"}   ask    ▶ approved (once)

  You have 14 TODOs across 6 files. The bulk are in `src/store.rs`...

  ▸ cron_add {"schedule":"0 9 * * 1","prompt":"..."} ask   ▶ approved (always)

  Created job "weekly-todo-nag" — next run Mon 09:00 America/Mexico_City.

› /cron
  weekly-todo-nag   0 9 * * 1   next Mon 09:00   last —      new session
```

Interaction details:

| Input | Behavior |
|---|---|
| Free text | New turn in the current session |
| `!<cmd>` | Run locally in the shell, output to the terminal only (never enters model context) |
| `/help`, `/?` | Slash command help |
| `/model [name]` | Show or switch the model for the session |
| `/new` | Start a fresh conversation, keeping the same session object |
| `/sessions` | List stored conversations, newest first |
| `/resume <id>` | Continue a stored conversation; accepts a full id, a unique prefix, or a position from `/sessions` |
| `/rename <title>` | Set this conversation's title |
| `/clear` | Forget the messages, keep the system prompt and the session id so the transcript stays resumable |
| `/tools` | Enabled tools with risk classes and approval status |
| `/session` | Show the conversation id sent to the provider in `${session}` headers |
| `/where` | Show the database path |
| `/cost` | Token and estimated cost for the session |
| `/cron`, `/jobs` | Job table with next-run times (M4) |
| `/allow <tool> <pattern>` / `/deny …` / `/approvals` | Inspect and edit policy (M2) |
| `/compact` | Force history summarization |
| `/quit`, Ctrl-D | Exit |
| Ctrl-C | Cancel current turn; twice in a row exits |
| Ctrl-L | Clear screen (local only; scrollback intact) |

Resuming keeps the stored conversation's id, so the `${session}` header stays identical across the
restart — that is the point of the header, and a fresh id would look like a new conversation to the
gateway.

Rendering rules:
- Assistant text streams to stdout; tool activity goes to **stderr** as single dimmed lines, so `minion run ... > out.txt` yields clean output.
- On a terminal, assistant text is rendered as markdown: headings, bold/italic/strikethrough, inline and fenced code, lists, blockquotes, rules, and pipe tables with box-drawing borders and column alignment. Rendering is **per block**, not per token — a block is drawn as soon as it is complete and nothing already printed is ever revised, which is what keeps the scrollback intact. A table cannot be laid out until its last row arrives, so it waits for one.
- **Piped output is not rendered.** When stdout is not a terminal the raw markdown is emitted, because the source is the more useful thing to capture and reformat later. `--markdown` does not override this.
- Layout and colour are separate. `NO_COLOR` and `--no-color` suppress the ANSI escapes but keep table borders and list markers, since alignment carries meaning that colour does not. `--no-markdown` turns rendering off entirely.
- Width comes from `--width`, then `$COLUMNS`, then 80. A table that cannot fit the width is emitted as plain rows rather than drawn into a mangled grid.
- `--json` emits newline-delimited JSON events (`{"type":"text"…}`, `{"type":"tool_call"…}`, `{"type":"done"…}`) for scripting, and never renders markdown: a JSON consumer wants the model's own text, not a drawn table.
- Piped stdin: `echo "..." | minion run -` reads the prompt from stdin; combined with a non-TTY, policy is enforced as non-interactive.
- Colors via a tiny ANSI helper honoring `NO_COLOR` and `--no-color`; no truecolor dependency.

### 5.12 CLI surface

```
minion [OPTIONS] [PROMPT]              # REPL, or one-shot if PROMPT given
minion run [OPTIONS] <PROMPT|->        # one-shot, non-interactive-friendly
minion session list|show <id>|rm <id>|resume <id>
minion cron add --schedule <CRON> --prompt <TEXT> [--name N] [--cwd P]
minion cron list [--json]
minion cron remove <id|name>
minion mcp serve [--stdio]             # run as MCP server
minion mcp list                        # configured servers + discovered tools
minion mcp tools <server>              # inspect one server
minion init [--check] [--project] [--force] [--non-interactive]
            [--header 'Name: value']… [--credentials-file <PATH>] [--no-store-token]
                                       # first-run setup for the model backend (§5.1.1)
minion config show|path                # read-only; `config init` aliases `minion init`
minion doctor                          # env, config, db, provider reachability
minion update [--check] [--yes] [--force] [--rollback]
                                       # check a release channel and replace the installed
                                       # binary (§9); `--check` writes nothing and exits 1
                                       # when an update is available

Global options:
  --model <NAME>        --base-url <URL>      --api-key-env <VAR>
  --yes                 # auto-approve policy.default == ask (dangerous; prints a warning)
  --deny                # force deny for everything not allowlisted
  --json                --no-color            --verbose|-v  --quiet|-q
  --markdown  --no-markdown      --width <n>
  --cwd <DIR>           --resume <SESSION>    --max-iterations <N>
  --config <FILE>       --db <FILE>
```

**Exit codes:** `0` success · `1` turn failed, or `minion update --check` found an update available · `2` usage error · `3` provider/auth error, or an update could not reach its release channel · `4` refused (policy denied in non-interactive mode, or an operation declined to proceed — e.g. `init` against an existing config without `--force`, an update whose checksum or commit did not verify, or one cancelled at the prompt) · `5` internal error.

---

## 6. Security and threat model

### 6.1 Assets

API keys; filesystem contents; shell access; the SQLite store (may contain sensitive transcripts); the MCP channel (may be driven by an untrusted model).

### 6.2 Threats and mitigations

| ID | Threat | Mitigation |
|---|---|---|
| T1 | Prompt injection in fetched content drives `run_command` | Approval gate + risk classifier + never auto-`--yes` in a TTY; fetched content is tagged as untrusted in the system prompt |
| T14 | The System One guard is steered by the same injected content it exists to catch | The guard may only narrow prompts, never widen permissions (FR-43); the static category list is a floor the model cannot cross (FR-44), so an ineligible command is never sent at all; `state` is the command string alone (FR-46); every failure falls back to prompting (FR-45) |
| T2 | Path traversal / symlink escape | Canonicalize and prefix-check against workspace roots; `follow_symlinks = false`; re-check after open where feasible |
| T3 | SSRF via `http_fetch` | Domain allowlist, private-IP block, no cross-domain redirects, response cap, scheme restriction |
| T4 | Secret exfiltration via tools or logs | Keys never in the model context unless a tool explicitly returns them; env redaction in logs; audit stores digests |
| T5 | MCP server exposes shell to an untrusted model | `expose_exec`/`expose_write` default false; read-only default surface; startup banner states enabled capabilities |
| T6 | Malicious external MCP server | Per-server allowlists, per-server approval, no implicit trust; third-party output treated as untrusted |
| T7 | Resource exhaustion | Per-tool timeouts, output caps, iteration/token budgets, job concurrency cap, process-group kill |
| T8 | Cron job runs with escalated intent | Jobs always execute under non-interactive policy (fail closed) |
| T9 | TOCTOU between approval and execution | Approval binds to the exact argv being executed; any mutation re-triggers policy |
| T10 | Store tampering / corruption | WAL + integrity check at startup; migrations versioned; corrupted DB refuses to start with a clear message |
| T11 | `init` writes a config that leaks a credential | The value goes only to the separate `0600` credentials file; the config records env var *names* and paths, so `--project` output stays committable; the token is never printed, never logged, and never appears in `--json` |
| T12 | `init` silently overwrites a hand-tuned config | Existing target is diffed and requires `--force`; writes go through a temp file and rename, so no partial config is left behind |
| T13 | The API key leaks into terminal scrollback or a screen share | Echo is disabled *before* the prompt is printed, not after, closing the window in which a fast paste would be echoed; the guard restores the previous termios on drop, including on panic |

### 6.3 Principle of least exposure

For each capability, an operator should be able to answer "who can trigger this?" from the config alone. The SDD therefore treats every entry point as a separate trust boundary: REPL user, piped stdin, cron, and each MCP client are distinct.

---

## 7. Observability

| Signal | Mechanism |
|---|---|
| Structured logs | `tracing` spans: `turn`, `provider.request`, `tool.invoke`, `approval`, `cron.fire` |
| Log sinks | stderr (human), file (text or JSON), `--verbose` raises level to `debug` |
| Audit trail | `audit_log` table for every tool decision and outcome |
| Cost | `usage` per message; `/cost` aggregates per session |
| Cron history | `job_runs` with status and durations; `/cron log <job>` |
| Redaction | A `tracing` layer scrubs values matching configured env var names and `Bearer *` |

---

## 8. Testing strategy

| Layer | Approach | Tools |
|---|---|---|
| Unit | Policy resolution, risk classifier, cron next-run, path guard, SSE parser, tool-call fragment assembly, diff output | `#[test]`, `proptest` |
| Provider | Fake HTTP server with scripted SSE streams, including malformed frames, mid-stream disconnect, 429 | `wiremock`, fixtures |
| Loop | Scripted provider emitting tool calls; assert message sequence, stop reasons, budget enforcement | in-memory store + mock provider |
| Tools | Real filesystem in temp dirs; process kill on timeout; output cap boundaries | `tempfile`, `assert_cmd` |
| Store | Migration up/down, concurrent writers, WAL recovery after simulated crash | `rusqlite` temp DBs |
| Cron | Virtual clock; assert catch-up policies, overlap handling, DST boundaries | injected clock |
| MCP | Our server driven by `rmcp` client; external server driven by a stub; golden tool lists | `insta` snapshots |
| REPL | Approval prompt flows, non-TTY fail-closed, `--json` event stream, `!` escape | `assert_cmd`, `insta` |
| E2E | Temp workspace + stub provider + real binary; scripted multi-turn scenario | shell-driven harness |

**Determinism:** all time is injected via a `Clock` trait; all randomness (jitter, UUIDs) is seedable in tests. No test requires network access.

---

## 9. Packaging and distribution

- `cargo build --release` produces one binary; a release workflow builds the platform targets and publishes them as assets of a tagged GitHub release, with a `checksums.txt` alongside. (The workflow itself is a separate deliverable; this section states the contract it must meet.)
- **Release pipeline.** `.github/workflows/release.yml` runs on a push to `main`. A single `gate` job derives the next `vX.Y.Z` from the newest tag, refuses to publish unless that equals `[workspace.package].version`, and runs `cargo +1.89.0 test --workspace --locked`; a `build` matrix then produces the six assets — `{linux, macOS, Windows} × {amd64, arm64}` — each carrying the commit and date embedded (`MINION_GIT_SHA`/`MINION_GIT_DATE`), and a `publish` job hashes all six into one `checksums.txt` and attaches the seven files to a GitHub Release, verifying the release is not left as a draft and that the uploaded manifest is the one hashed. Each target is built on a runner that can also *run* it and the built binary is executed with `--version` to assert both stamps before it is published, so no architecture goes out unverified (D28). The six asset names are exactly the ones `minion-core::update::asset_for` looks up (§9.1): a name published differently is a 404 to `minion update`, not a fallback, and a unit test pins the table so the two cannot drift. The tag, the version the binary reports, and the assets are the three things `minion update` consumes; the workflow fails rather than publishing a set that disagrees. The procedure, and how to cut a tag by hand, are in README → Releasing. The Linux assets are gnu rather than musl (D24); the per-target runner choices are D28.
- Homebrew tap, `cargo install minion-cli`, and a curl installer script.
- **Version reporting.** `minion --version` prints the workspace version, the git SHA and commit date the binary was built from, and the capability families compiled in:

  ```
  minion 0.1.0 (f7581dae470a 2026-10-07) [features: cron,guard,mcp,update]
  ```

  The stamps are embedded at compile time (`minion-cli/build.rs`), so `--version` needs no git, no network and no config, and it degrades to `unknown` rather than failing when git is absent. `minion update` reads the same line back out of a downloaded binary to tie it to a commit.
- Config/db paths created on first run with `0600`/`0700` permissions.

### 9.1 `minion update`

`update` is the self-update path: it asks a release channel what the latest version is and, when it is newer, downloads the prebuilt binary for the current platform, verifies it, and replaces the installed one.

**Channel.** A GitHub-compatible releases API, configured under `[update]`:

```toml
[update]
api_url = "https://api.github.com"   # or a mirror; https, or http only on loopback
repo = "Az107/minion"                # owner/name
asset_prefix = "minion"              # assets are `<prefix>-<os>-<arch>` and `checksums.txt`
```

No token is sent: the repository is public. A `401`/`403`/`404` is reported as "this works without a token only for a public repository" rather than retried, and authentication is deliberately not implemented (D23).

**What it installs.** Only the asset whose name is `<prefix>-<os>-<arch>` for the running platform (`linux`/`darwin`/`windows` × `amd64`/`arm64`). A release without that asset is a refusal, not a best-effort choice among the others.

**Verification, in order, before anything is replaced:**

1. The `checksums.txt` asset of the *same release* is fetched and the downloaded binary's SHA-256 must match its entry; a mismatch aborts with the binary untouched. The hash is computed in-tree (`minion-core::sha256`) so the check adds no dependency.
2. The commit the release declares (`target_commitish` as a hex SHA, or a `build-commit:` line in the notes) must match the SHA the downloaded binary reports for `--version`. A release that declares no commit is refused rather than trusted, and a binary reporting a different commit is not installed.

**Replacement.** The verified bytes are written to a staging file beside the target and moved over it with `rename()`, which is atomic and works for a running binary on Linux. The previous binary is kept as `minion.old-<version>`, which `--rollback` restores (keeping the replaced one in turn). The target is `current_exe()`, canonicalized, or `$MINION_UPDATE_BINARY` when set. Nothing else is touched: not the config, not the database, not the credentials file.

**When the directory is not writable**, minion does not elevate. The verified binary is staged under the temp directory and the operator is handed the two literal commands to run with `sudo` (back up first, then install), exiting `4`.

**Consent.** Without `--yes` and without a terminal, nothing is installed: the same fail-closed rule as the rest of the tool. On a terminal the release notes are shown and one `y`/`yes` proceeds.

**Flags.** `--check` reports installed/remote/asset and writes nothing (`0` up to date, `1` update available); `--force` reinstalls the current version; `--rollback` restores the most recent backup without contacting the channel; `--json` emits one machine-readable object. `--yes` is the global consent flag.

---

## 10. Milestones

| Milestone | Status | Scope | Exit criteria |
|---|---|---|---|
| M0 — Skeleton | done | Workspace, config loading, provider client, `run` one-shot | A prompt returns streamed text from a compatible endpoint |
| M0.5 — Provider init | done | `minion init` wizard, presets, `/models` discovery, `--check`, `--project`/`--force`, `--header`, session-id headers | A fresh machine reaches a working config in one command, an existing config is never clobbered, and a gateway requiring `${session}` headers works unmodified |
| M1 — Session core | done | Store, migrations, sessions/messages, REPL with streaming | `/resume` restores a conversation |
| M2 — Tools + policy | done | Registry, `read_file`/`write_file`/`edit_file`, `run_command`, approval engine, allowlist | A risky command cannot run without consent |
| M3 — Memory + HTTP | done | `remember`/`recall`, `http_fetch` with allowlist | FTS recall works; SSRF guard tested |
| M3.5 — System One guard | done | Optional `/v1/systemone` judge for flagged `run_command`, two thresholds, category floor, audited verdicts | An ineligible command never reaches the model; every failure path prompts; a guard that returns `Err` cannot produce an allow |
| M4 — Cron | done | Scheduler, job CRUD, run history, catch-up | A weekly job fires on a virtual clock test |
| M5 — MCP client | done | External servers, namespaced tools, per-server policy | External tool callable with approval |
| M6 — MCP server | done | `mcp serve` with read-only default surface and opt-in exec/write | Another model drives `agent_ask` end to end |
| M7 — Hardening | | `--json`, `doctor`, audit log, redaction, packaging, docs | NFR targets met; installers published |
| M8 — Self-update | done | `minion update`: release channel, checksum and commit verification, atomic replace, `--rollback`, and `--version` carrying the git SHA | An installed binary fetches, verifies and replaces itself from a published release; a bad checksum or an unverifiable release changes nothing; `--check` writes nothing |
| M9 — Release pipeline | done | GitHub Actions workflow: version derivation from tags, tag/`Cargo.toml` agreement gate, test gate, `linux/amd64` + `linux/arm64` via gcc cross, `checksums.txt`, GitHub Release | A push to `main` publishes a coherent release (tag, embedded version and assets agree) that `minion update` installs; a mismatched version or a failing test publishes nothing |
| M9.1 — Six targets | done | The release pipeline covers `{linux, macOS, Windows} × {amd64, arm64}`: macOS and Windows on their own runners, Windows/arm64 native, every built binary run to assert both stamps, and one `checksums.txt` over the six named assets (D24, D28) | A push to `main` publishes all six assets under the names `minion update` looks up, each carrying the version and commit it was built from, and the uploaded `checksums.txt` covers the six |

---

## 11. Risks and open questions

| ID | Risk / question | Proposed handling |
|---|---|---|
| R1 | Command risk classification will both over- and under-block | Keep the classifier small, documented, and testable; make it easy to override with allowlist entries; log every classification |
| R9 | A model-based judge inherits the injection surface it guards (T14) | Treat the guard as a noise filter inside a boundary that static code drew without it: it resolves prompts, never permissions. Disabled by default, and a vendor-documented adversarial-content failure mode is assumed rather than disproved |
| R2 | `run_command` is inherently unsandboxable in-process | Ship v1 with approval + allowlist; add optional `sandbox-exec`/`bwrap` execution mode later behind config |
| R3 | History summarization can lose critical detail | Mark summaries explicitly; keep the full transcript in SQLite; allow `/compact off` |
| R4 | Provider drift across OpenAI-compatible endpoints | Keep the client thin, expose quirk flags, and add a compatibility test matrix |
| R5 | MCP server + REPL contend for stdio | Mutually exclusive modes; `mcp serve` never starts a REPL |
| R6 | Cron in-process means jobs don't run when minion is closed | Documented; optional `--system` crontab/launchd integration deferred to v2 |
| R7 | `always` allowlist could persist an over-broad pattern | Show the exact pattern before persisting; require explicit confirmation; cap pattern length and forbid wildcards alone |
| R8 | Token/cost accounting differs per provider | Treat usage as advisory; never block a turn solely on a missing usage field |

**Open questions for review:**
1. ~~Should `http_fetch` support an explicit `http://` for local dev servers via a named allowlist
   entry, or require HTTPS unconditionally?~~ **Resolved — a *named, exact* allowlist entry.**
   `https` is required unless the exact host (no wildcard) appears in `allowed_domains`, so a local
   dev server is reachable without weakening the default; a wildcard entry grants `https` only.
   Matches §5.5's guard row. Implemented (D17, which also records the separate, stricter handling of
   §5.5's approval row).
2. ~~Should job prompts be able to opt into the interactive policy when minion is attached to a TTY, or always fail closed?~~ **Resolved — always fail closed.** A job's prompt is not a person at a keyboard, even when the scheduler happens to be ticking inside a REPL that has one. Its gate is built with `interactive = false` and no approval UI at all, so a tool the policy would `ask` about is decided by `policy.noninteractive` (`deny` by default) and a job cannot promote it to `auto`. A prompt that appeared at an unrelated moment would be approved by whoever was mid-keystroke, which is exactly the consent that means nothing. Deny rules and allowlists still run first, so an allowlisted command runs unattended; the guard is not consulted either, because it only ever resolves a prompt and there is none. See D19.
3. ~~Is `edit_file`'s exact-match semantics sufficient, or is a patch-based tool wanted for large
   edits?~~ **Resolved — both are provided.** `edit_file` stays for a single surgical replacement;
   `apply_patch` handles multi-site and multi-file edits in one atomic call. See §5.5 and D13.
4. ~~Should memory be scoped per-workspace (proposed) or global with a namespace parameter?~~
   **Resolved — per-workspace.** The `namespace` column is a hash of the canonical workspace root, so
   no parameter is needed on either tool and facts cannot leak between projects. Matches §5.5.
   Implemented.

---

## Appendix A — Tool schema on the wire

`ToolSchema` is a domain value — `{name, description, parameters}`. It is **not** what goes on the
wire. Each entry of the request's `tools` array is wrapped in the function envelope, and a compliant
provider answers a flat entry with `400 invalid request`:

```json
{
  "type": "function",
  "function": {
    "name": "run_command",
    "description": "Run a shell command in the workspace. Requires approval unless the command matches an allowlisted pattern. Output is captured and truncated.",
    "parameters": {
      "type": "object",
      "properties": {
        "command":    { "type": "string", "description": "Shell command line to execute." },
        "cwd":        { "type": "string", "description": "Working directory, relative to a workspace root." },
        "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 3600000, "default": 120000 },
        "stdin":      { "type": "string", "description": "Optional stdin for the process." }
      },
      "required": ["command"],
      "additionalProperties": false
    }
  }
}
```

`schemars` supplies `parameters` from the Rust argument struct. Note that it also emits `$schema`,
`title`, `format`, and — for `Option<T>` — a union such as `"type": ["integer", "null"]`. The
OpenAI endpoint accepts these, but providers that translate the schema into another vendor's format
may not; if a gateway ever rejects a tool, normalize the schema at the registry boundary rather
than editing individual tools.

## Appendix B — Request/response shape used against the provider

```jsonc
// POST {base_url}/chat/completions
{
  "model": "gpt-4.1-mini",
  "stream": true,
  "stream_options": { "include_usage": true },
  "temperature": 0.2,
  "tools": [ /* Appendix A shape */ ],
  "tool_choice": "auto",
  "messages": [
    { "role": "system", "content": "You are minion…\nWorkspace: /Users/me/proj (read-write)\nMode: interactive\nTools: …\nDenied: run_command *sudo*" },
    { "role": "user", "content": "list the largest files here" },
    { "role": "assistant", "tool_calls": [
        { "id": "call_1", "type": "function",
          "function": { "name": "run_command", "arguments": "{\"command\":\"du -sh * | sort -h | tail\"}" } } ] },
    { "role": "tool", "tool_call_id": "call_1", "content": "{\"exit_code\":0,\"stdout\":\"…\"}" }
  ]
}
```

## Appendix C — Decision log

| # | Decision | Rationale |
|---|---|---|
| D1 | Rust over Zig | Mature async, SQLite, and MCP ecosystem; Zig's async and DB story would dominate the schedule |
| D2 | Line REPL over full-screen TUI | Matches fx and Unix composability; less code; screen readers and pipes work |
| D3 | Approval + allowlist over sandbox-first | Sandboxing is not portable; approval policy is auditable and testable on all platforms |
| D4 | SQLite over JSONL | Cron needs queries and transactional writes; single file stays portable |
| D5 | In-process scheduler over system crontab | Jobs stay portable and inspectable; no OS-specific install/uninstall |
| D6 | MCP both directions | Serving lets other models use minion; consuming lets minion use the wider MCP ecosystem |
| D7 | Thin hand-rolled provider client | Full control over base URLs, streaming, quirks; avoids SDK lock-in |
| D8 | Read-only MCP surface by default | Exec and write over MCP are remote code execution surfaces; opt-in only |
| D9 | `init` writes only env var names and targets the user config by default | Keeps credentials out of the repo; `--project` is opt-in for shared, secret-free setups. Extended, not replaced, by D11, which adds the credentials file for the *value* while keeping this rule for the config |
| D10 | One generic `[provider.headers]` table with a `${session}` placeholder, instead of a built-in OpenCode-Go-specific header | Any gateway requirement is expressible in config; no provider-specific branches in the client. The placeholder is the only way an id can reach a header, so a stable per-conversation id cannot be accidentally re-generated per request |
| D11 | The API key is collected by prompt and stored in a separate `0600` credentials file, while the config keeps the `api_key_env` path | Removes the "find a variable name and export it yourself" step, which was the whole friction of the old flow, without putting a secret in a file people commit. A `netrc`/AWS-CLI style split: portable config, local secret. Resolution order keeps env var → file so a shell or CI job can still override |
| D12 | Terminal echo is disabled before the prompt is printed rather than by the library that reads the secret | `rpassword`-style helpers print first and disable second, which leaves a window where a pasted key is echoed into scrollback. Owning the termios guard is ~20 lines and closes it; `--no-store-token` keeps an escape hatch for anyone who does not want a secret on disk at all |
| D13 | `apply_patch` takes structured JSON operations, and sits *alongside* `edit_file` rather than replacing it | Anchored operations are unambiguous: "anchor matched 2 times" is an exact, actionable error, where a fuzzy hunk match silently edits the wrong lines. `edit_file` stays because for a one-line change it is the smaller, less failure-prone request |
| D14 | Assistant text is rendered as markdown on a terminal, per block, with the parser from `pulldown-cmark` and the renderer written in-tree | A terminal has no font sizes, so hierarchy is bold plus a colour that steps with the heading level; the scrollback rule is kept by emitting whole blocks and never revising text already on screen, which streaming token-by-token cannot do for a table. Rendering only when stdout is a terminal keeps `minion run ... > out.md` yielding markdown rather than ASCII art, and the split of layout from colour means `NO_COLOR` still gets aligned tables. The renderer is ours so the output is exactly the intended one and the dependency stays a single parser, which is what keeps the binary inside NFR-3 |
| D15 | The non-interactive rule governs tools that *change* something; `ReadOnly` is allowed unattended and `Network` counts as a change | §5.6 lists `ReadOnly → Auto` above the `!stdin.is_tty()` branch, but the implementation checked the TTY first, so with the default `noninteractive = "deny"` a read was refused whenever stdin was piped — which breaks the `minion run ... > out.md` invocation this document advertises, for `read_file` as much as for `recall`. A missing terminal says something about *consent*, and a read cannot consume consent it never asks for; what it genuinely blocks is an unattended mutation. `Network` is deliberately on the mutation side: the request leaves the machine and can be induced by untrusted content in a transcript, so `http_fetch` is gated like a write and still needs `--yes` or an allowlist entry in a pipe. Deny rules, allowlists and the classifier are untouched and still run first, so this widens nothing that was explicitly refused. Recorded because it touches the rule order that the invariants call a security property, even though it only restores what §5.6 already specified |
| D16 | A System One model may resolve a flagged `run_command` prompt, but only for categories the static classifier found *reversible*; it can narrow prompts and never widen permissions | The static classifier cannot distinguish a plain `curl` from a `git push --force`: both are one tag, so both always ask. That is safe but noisy, and noise trains a user into approving without reading. A typed-probability model separates them — `POST {base}/v1/systemone` returns calibrated numbers rather than text, so the decision is a threshold applied in our code and cannot be talked into by a model that simply asserts it is safe. The model is explicitly *not* a new authority. TypeSafe's own notes for `jev-1.13` state that adversarial content in `state` "can move the answer", and the command being judged may itself have come from injected content (T1, T14), so the boundary is inverted: the heuristic category list is a floor that ineligible commands never cross, and the model only ever operates strictly inside it. `privilege`, `remote-execution` and `destructive` are resolved before any network call, so `sudo rm -rf /` has no code path to a model verdict. Two thresholds rather than one, because a single cut discards the middle band where a sceptical model is saying something worth hearing, and every failure — timeout, `429`, withdrawn free tier, unparseable body — falls back to the existing prompt. Only the command string is sent as `state`, both because the vendor documents context rot on unrelated detail and because a minimal `state` shrinks the injection surface. Disabled by default: it is a network call carrying command text to a third party, and it is opt-in until it has been seen behaving. Specified against the `/v1/systemone` protocol rather than one vendor, since Jev, Laya, Kev, Decider, Von and OpenThai-SystemOne all speak it and a client only changes `base_url` — noting that Laya's state budget is ~320–512 tokens against Jev's 32k and its `confidence` is computed differently, so thresholds do not transfer between them. Implemented in M3.5: `minion-core::guard` holds the static category floor, the two thresholds and the `SystemOneGuard` trait; `minion-guard` is the `/v1/systemone` client (`{"state": command}` in, `{"unsafe": p}` out); the engine consults it only where a prompt would otherwise be shown, so a non-interactive run still resolves to `policy.noninteractive` |
| D17 | `[http_fetch].allowed_domains` is the reachability guard, kept separate from the approval allowlist — a deliberate deviation from §5.5's approval row, pending the spec owner's confirmation | §5.5 states the guard ("domain must match `allowed_domains`") and the approval rule ("`auto` if domain allowlisted, otherwise `ask`") as if one list drove both. Read as one list, the approval row's `otherwise ask` clause can never fire — the guard refuses a host outside the list before approval is reached — so the only operative rule would be "`auto` if allowlisted". This implementation keeps the two mechanisms apart: `allowed_domains` is the reachability boundary and the SSRF mitigation T3 names, while `[policy.allow]` is what skips the prompt, matched against the URL host because the tool reports the host as its approval subject. The result is stricter than §5.5's row, not looser: an allowlisted domain still prompts on a TTY and is still refused in a pipe (D15) until it is given a policy allow rule or `--yes`, and nothing added for reachability is silently auto-approved. The alternative — letting the guard's list also drive approval — would either reduce the SSRF boundary to a mere prompt or auto-approve every domain added to `allowed_domains`, including in a non-interactive run. Recorded as a deviation from §5.5's approval row for the spec owner to confirm or overrule. |
| D18 | A job's `schedule` is a five-field **Vixie** cron expression, translated into the parser's six-field Quartz grammar before it is parsed | The `cron` crate's grammar is Quartz-flavoured in two ways that silently change what an expression means. It puts seconds first, so a five-field string is prefixed with `0 ` — every SDD expression fires on the zeroth second of its minute, which is what a five-field cron means everywhere else, and a user who types six fields is told so rather than quietly getting a different schedule. And it numbers the days of the week `1` = Sunday, where every other cron numbers them `0`/`7` = Sunday and `1` = Monday: left alone, `0 9 * * 1` fires on Sunday, a day out from what the user wrote. The day-of-week field is therefore rewritten element by element (plain numbers, ranges, lists, and stepped forms), a bare `*` or `*/n` is left alone because a wildcard's offset is uniform so the set of days is identical in either numbering, and names (`mon`) pass through because both conventions agree on them. A range that would wrap (`6-1`) is refused instead of being silently reordered. One further deviation is documented rather than fixed: when *both* day-of-month and day-of-week are restricted, the parser ANDs them where Vixie ORs them, so `0 9 1 * 1` means "the 1st, if it is a Monday" — an expression that rare is better left explicit, and pretending otherwise would need a scheduler of our own |
| D19 | A cron run always takes the non-interactive policy, and a fire consumes its occurrence | §11's open question 2 is settled as fail-closed, and the reason is the same one D15 gives: consent requires a person who is being asked. A job prompt has no terminal even when the scheduler is ticking inside a REPL that has one, so its gate is built with `interactive = false` **and no approval UI**, which makes `policy.noninteractive` the deciding rule for anything that changes something and leaves deny rules and allowlists in front of it untouched. A job cannot promote an `ask` tool to `auto`, cannot reach the System One guard (there is no prompt for it to resolve), and cannot create or delete other jobs unless an allow rule names it — the three properties the milestone's tests pin. The fire path is the other half: the occurrence is consumed when it fires, not when it finishes, because `next_run_at` recomputed only on completion would leave a job whose run outlasts its own interval looking due on every tick, and every tick would then record an overlap. Completion recomputes from the cron expression and the completion instant, which can only move the value later, so the "on completion" sentence in §5.7 and the fire-time advance agree instead of fighting. `missed_run_policy` stays a `[cron]` setting rather than a per-job column, since the schema has none and inventing one means a migration for a knob nobody has asked to vary per job; catch-up replays occurrences without the overlap check, because a replayed occurrence never ran and there is no live run to collide with, while `max_concurrent_jobs` and `missed_run_cap` still bound the burst. A `reuse` job's session is adopted in a *separate* statement from the run's terminal status, because `jobs.session_id` is a foreign key and an adoption that fails must not roll back the record that the run finished |
| D20 | The tool catalogue the model sees is read live from a `ToolCatalog`, and `lazy` defers a server's spawn to the first turn rather than hiding its tools | §5.10 asks for two things that a registry frozen at session assembly cannot both have: the servers are "spawned at startup", and a server that failed is "retried on the next turn". A `Vec<Box<dyn Tool>>` built once can do neither — a retry that cannot re-add a tool is not a retry. So the registry keeps its registered tools and *consults* a catalogue on every read of the tool list; an MCP failure adds or removes entries in a structure that changes under the frozen `Arc<ToolRegistry>` every caller already holds. That also settles shadowing by construction rather than by convention: registered tools are enumerated first and a name already present is skipped, so a server cannot publish a name a built-in owns, and two servers cannot fight over one. `lazy` is the one place where a literal reading had to be traded for a useful one. "Spawned lazily on first use" cannot mean "hidden until a tool is called", because a tool cannot be called before it is listed and a server cannot list tools before it is spawned — the two requirements would deadlock. It therefore means the spawn is deferred from session assembly to the first turn: opening a session (or `minion mcp list`, which is an explicit ask) does not start a process, and the tools reach the catalogue from that point rather than being invisible. The alternative — lazy servers reachable only through a generic `mcp_call` — was rejected because it moves the policy decision off the tool name the per-server policy keys on. `mcp_call` itself is deferred for that reason; the flattened tools are the surface §5.10 describes as primary |
| D21 | An external tool is always `Risk::Network`, and a server's `policy` substitutes the global default *and* the non-interactive decision for its own tools | §5.5 says an external tool inherits the target's declared risk with `Network` as a floor. MCP declares risk only in `ToolAnnotations`, and the protocol's own documentation says a client must not make tool-use decisions from them — they come from a server the operator has not vouched for (T6). Since the floor is `Network`, an inherited hint could only ever *lower* the class, never raise it, so the floor is the whole rule and the annotations are ignored. The consequence is deliberate: every external call is gated at least as strictly as an outbound HTTP request, and D15 puts `Network` on the side of the line that a missing terminal refuses. The per-server policy then has to mean something for an unattended run, or "a trusted local server can be `auto`" would be false the moment a pipe is involved. It therefore substitutes both fallbacks — `policy.default` and `policy.noninteractive` — and nothing else: deny rules, allow rules and the command classifier still run first and still win, so a family marked `auto` cannot resurrect a refusal that already happened, and a family marked `deny` refuses its tools outright. This is the trust boundary §6.3 describes applied literally: each MCP client is its own entry point, and the config states its policy in one place |
| D22 | The MCP server's disabled capabilities are *listed and refused*, not absent, and the exposed shell tool is decided under the name `run_command` | §5.9's table says `agent_run_command` and `agent_write_file` are "denied unless" the flag is set, and T5's mitigation is a startup banner — both describe a capability that exists and is refused, not one that is missing. A hidden tool would leave the host unable to tell "minion cannot do this" from "I mistyped the name", and it would make the denial invisible in the audit trail. So the two tools are always in the list, and without their flag the engine carries a deny rule that refuses every call and records the refusal. The name the *gate* sees is the real tool's — `run_command`, `write_file` — because the policy engine is not a second, weaker world: an operator's existing `run_command` deny rules keep protecting the MCP surface, the classifier still sees the command it is judging, and `subject_for`/`pattern_for` still extract the right subject. The `agent_` prefix is a naming convention of this surface, not a second identity, and the audit row therefore names the tool that actually ran. The gate is non-interactive (`stdin` is the protocol pipe, so no prompt can be shown), and an enabled family substitutes the non-interactive fallback through `ToolPolicy` (D21) — deny and allow rules still run first, so turning a flag on grants nothing that was refused. `agent_ask`'s inner agent is restricted to the read-only subset of the built-in registry, filtered by `Risk::is_observation` rather than by a list of names, so the default surface cannot drift wider than the read/write line itself. The scheduler is deliberately not started here: a job created over MCP is stored and runs under the non-interactive cron gate the next time a process with a scheduler opens the database, which keeps `mcp serve` a protocol server rather than a daemon |
| D23 | `minion update` installs a **prebuilt binary from a release channel**, never compiles on the target, and verifies it with two independent checks before replacing anything | The alternative — `git pull` and `cargo build` in place — was rejected because it turns an update into a build environment problem: it needs a toolchain on every machine, it needs a writable checkout, and a failure halfway leaves a half-built tree rather than a known-good binary. A prebuilt asset is one file that either matches or does not. The channel is a GitHub-compatible releases API with prebuilt per-platform assets and a `checksums.txt`, which is the shape `Az107/space-elevator` already publishes, so there is a working precedent to mirror. Two checks, not one, because each closes a different hole: the **checksum** binds the bytes to the release, and the **commit** binds the release to a revision, so a swapped asset that comes with a swapped manifest still fails the commit check. The commit is read from `target_commitish` when it is a hex SHA, or from a `build-commit:` line in the notes otherwise; a release that declares neither is refused rather than installed on a guess, which is the fail-closed reading of §5.12's "refused" exit. The hash is SHA-256 implemented in-tree (`minion-core::sha256`) rather than taken from `sha2`: the algorithm is 60 lines, the tests pin the NIST vectors, and it keeps `Cargo.lock` free of a new external crate, which matters for a binary whose whole selling point is a small dependency surface. The replacement is a staging file plus `rename()` so the path is never a half-written file, and the previous binary is kept as `minion.old-<version>` — which is also what `--rollback` restores, so undo does not need the network. Nothing else is touched: not the config, not the database, not the credentials file, because an update replaces exactly one file and anything more would make it an installer with opinions. When the directory is not writable, minion does not elevate and does not silently skip: it stages the verified bytes where it can write and prints the two literal `sudo` commands, matching the rule that Hermes never runs `sudo`. Two smaller decisions are recorded here too. `https` is required, with plain `http` permitted only to a literal loopback host — the exception exists for a local mirror and a test fixture, and it mirrors the rule `http_fetch` already uses (D17). And the target defaults to `current_exe()` canonicalized, overridable with `$MINION_UPDATE_BINARY`, so an update can replace an installation other than the one currently running without becoming a privileged operation: whoever can set the environment already runs the code. `--check` stays read-only, which is why it exits `1` on "update available" rather than `0`: a script asking "should I update?" needs the answer in the status, not in the prose |
| D24 | The release pipeline publishes **gnu Linux binaries for amd64 and arm64** (amd64 native, arm64 cross-compiled with `gcc-aarch64-linux-gnu` on the runner) alongside the four macOS and Windows assets, derives the tag from the newest `vX.Y.Z` tag, and refuses to publish unless that tag equals `[workspace.package].version` | §9 asks for musl static on Linux, and this deviates: the binaries are gnu and dynamically linked. The reason is the dependency tree. `minion-store` bundles SQLite and rustls pulls `aws-lc-sys`, both of which compile C through `cc` and cmake, so a static musl build is a cross-toolchain problem — a musl C compiler for each architecture — rather than the one-file build musl is for a pure-Rust crate. The gnu path builds with the runner's own toolchain and only adds `gcc-aarch64-linux-gnu` for the arm64 link, which is the option the milestone named; musl is deferred until it can be exercised end to end rather than guessed at. The cost is a glibc floor: the job is pinned to the `ubuntu-22.04` runner (glibc 2.35) instead of `ubuntu-latest`, so the floor is stable rather than moving when GitHub rotates the image, and the failure mode is bounded — a binary that cannot start fails the `--version` commit check inside `minion update` and the install is refused, not half-applied. The Linux pair is published gnu; the four macOS and Windows assets are built by the same pipeline on the runners D28 describes, and the six names together — `minion-{linux,darwin,windows}-{amd64,arm64}` — are what `minion update` can consume. The tag is derived from the newest tag rather than from the manifest because that is the rule the `Az107/space-elevator` precedent publishes with, but the manifest is what the binary reports and `minion update` compares that to the tag, so the two must agree or the workflow fails: a release whose tag and embedded version disagree would make `minion update` offer the same release forever. |
| D28 | The six-target release matrix builds each target on a runner that can also **execute** it — macOS on macOS runners (`macos-15-intel` for x86_64, `macos-15` for arm64), Windows/arm64 on the native `windows-11-arm` runner rather than cross-compiled with `cargo-xwin`, Linux/arm64 cross-linked and run under `qemu-user-static` — so every binary's two stamps are checked before it is published | Three constraints decide it. macOS cannot be cross-linked from Linux: the linker and SDK are Apple's and not redistributable, so the two `*-apple-darwin` targets are built on macOS runners, which is also where they can be run. Windows/arm64 is a real choice — cross-compile an arm64 PE on an x64 runner with `cargo-xwin`, or build natively on the GA `windows-11-arm` image — and the milestone's own requirement picks for it: the asset must be executed to verify its stamps, and a Windows x64 host cannot run an arm64 PE (Windows on ARM emulates x64, not the reverse), so the cross path would ship an arm64 binary that no runner in the matrix can check. `cargo-xwin` would also pull a third-party tool and a downloaded MSVC CRT/SDK into a pipeline whose point is a small, legible surface, and the C in the tree (`aws-lc-sys` through rustls, and the SQLite bundled by `minion-store`) compiles against the native runner's own arm64 MSVC toolchain with no extra setup. Linux/arm64 stays the one cross link — the distro aarch64 gcc is also the C compiler those crates need — and `qemu-user-static` is installed so it too is run rather than trusted. The other cost is a C toolchain per runner, which the GitHub images already carry: NASM and Perl for `aws-lc-sys` on Windows, Xcode CLT for the bundled SQLite on macOS, so the job adds an install step only for the Linux cross. The Windows assets are the bytes of the built `.exe` under the suffix-free published name, because the updater replaces `current_exe()` and the installed file keeps its real name; the stamp check runs the `.exe` itself, whose bytes are the asset. Runner labels are pinned rather than `-latest`, for the same reason the Ubuntu one is (D24): the image must not move under a published binary; the six asset names are the client contract a unit test pins (`minion-core::update::asset_for`). |
