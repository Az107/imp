# AGENTS.md

## Status

Rust workspace implementing `SDD.md`. Milestones **M0 through M2 are complete**: config and layered
credential resolution, an OpenAI-compatible streaming provider with `${session}` header support, the
agent loop, the read/write/patch/exec tool set, the approval engine, a SQLite store with resumable
sessions, and `minion init`. Branch is `main`.

**M3 is half done — memory only.** `remember`/`recall` are built, registered, and tested: the store
upsert and FTS query in `minion-store/src/memory.rs`, the tools in
`minion-tools/src/memory.rs`, and the namespace helper in `minion-core/src/memory.rs`. `http_fetch`
and the SSRF guard are **not started**, so M3 is not finished and the milestone table in `SDD.md`
still correctly reads "next". The `[http_fetch]` config section does not exist yet either.

`minion-cron` and `minion-mcp` are **empty stubs** for M4–M6. Their manifests list real dependencies
(`rmcp`, `cron`), but there is no code behind them yet. In `minion-store`, the `jobs`, `job_runs` and
`audit_log` tables exist in the schema but nothing writes to them until M4. The `memory` table is now
written and read.

`default_registry` takes a second argument, an `Arc<Store>`, because the memory tools need one. This
is why `minion-tools` depends on `minion-store`; the store is opened *before* the registry in
`setup::build` so the system prompt can still be built from the finished tool list.

Assistant text is rendered as markdown on a terminal (`crates/minion-cli/src/markdown.rs`):
headings, emphasis, code, lists, quotes, rules, and pipe tables. It is rendered per block, so
nothing already on screen is ever revised.

## Commands

```sh
cargo +1.89.0 build                # whole workspace
cargo +1.89.0 test --workspace     # all tests; offline, no network, no API key needed
cargo +1.89.0 test -p minion-core   # one crate
cargo +1.89.0 test -p minion-core agent::tests::runs_a_tool_and_feeds_the_result_back   # one test
cargo +1.89.0 clippy --all-targets -- -D warnings    # lint gate; currently clean
cargo +1.89.0 fmt --all
./target/debug/minion --help
```

**Invoke cargo as `cargo +1.89.0`.** The installed `default` toolchain on this machine is 1.88.0,
which is below the workspace's `rust-version = "1.89"` and cannot build it at all — cargo refuses
before compiling a crate. 1.89.0 is installed as a side toolchain rather than as `default`, so
other projects keep the version they expect. If `rustup default 1.89.0` is ever set, the `+1.89.0`
prefix becomes redundant but stays harmless.

Toolchain floor is **Rust 1.89 / edition 2024** — the code uses let-chains
(`if let Some(x) = a && let Ok(y) = b`), which need edition 2024.

## Non-obvious structure

- Package `minion-cli` produces a binary named **`minion`** via an explicit `[[bin]]`. Adding a
  second bin, or relying on the package name, will surprise you.
- `minion-core` has **no terminal, network, or database dependencies** on purpose: the agent loop
  is tested against a scripted `MockProvider` in `crates/minion-core/src/agent.rs`. Keep it that
  way — push I/O into `minion-provider`, `minion-tools`, or `minion-cli`.
- Assistant text goes to **stdout**; tool activity, logs, and errors go to **stderr**. This is what
  makes `minion run ... > answer.txt` clean. Don't print diagnostics to stdout.
- **Piped output is never rendered.** `run::style_for` checks `IsTerminal` *before* honouring
  `--markdown`, so `minion run ... > out.md` still yields markdown source. If you make the flag win,
  every user piping output gets ASCII tables.
- **Layout and colour are separate.** `NO_COLOR` and `--no-color` drop the escapes but keep table
  borders and list markers, because column alignment carries meaning and colour does not.
- Config overlays are deep-merged on `toml::Value` trees, so a project `minion.toml` only restates
  the keys it changes. **Unknown keys are ignored on purpose** (forward compatibility) — do not add
  `deny_unknown_fields`.
- Every session gets a `session_id` (`minion_core::new_session_id()`, a v7 UUID). It is expanded
  into any configured header containing `${session}`. Some gateways require this: OpenCode Go
  rejects requests without `x-opencode-session`. The id must stay **stable for the whole
  conversation** — regenerating it per request would break the prompt-cache routing it exists for.
- `minion init` deliberately runs *before* config loading in `main.rs`, so a broken or missing
  config can still be repaired by it. Keep that ordering.
- **Markdown rendering is a presentation concern and lives in `minion-cli`, not `minion-core`.**
  `minion-core` still emits raw `TextDelta`. The parser is `pulldown-cmark`; the renderer is ours.

## Invariants that must not be relaxed

- **Every `Write` and `Execute` tool must go through the gate.** `default_registry()` now registers
  `read_file`, `edit_file`, `apply_patch`, `write_file`, and `run_command`, and `setup::build_gate`
  attaches a `PolicyEngine` to the agent. A registry built without a gate is only safe for read-only
  use — if you add a tool, confirm the gate is wired, not that the tool "seems harmless".
- Approval is fail-closed: non-TTY never prompts and defaults to `deny`; deny rules always beat
  allowlists.
- The MCP server is read-only by default. `expose_exec`/`expose_write` stay `false`; `run_command`
  is never exposed over MCP by default.
- Cron job runs always use the non-interactive policy and cannot prompt for approval.
- Path guard: `ToolCtx::resolve` canonicalizes and confines to `workspace_root`. Use it for every
  path-taking tool; do not reimplement it.
- **The config file must never contain a secret.** `Plan::to_toml` has no field that can hold the
  API key — the token is collected separately and written to the `0600` credentials file. Keep it
  that way; `--project` configs get committed.
- Credential resolution is env var first, then credentials file (`Config::api_key`). Both keys are
  `String`, not `Option`, because empty must *override* the default rather than fall back to it — a
  keyless backend sets **both** to `""`.
- The `Authorization` value is wrapped in `ApiKey`, which has a redacting `Debug`. Never log it.
- Terminal echo is disabled **before** the secret prompt is printed (see Toolchain gotchas).

## Toolchain gotchas

- **`tools[]` must be the function envelope.** Each entry is
  `{"type":"function","function":{"name","description","parameters"}}`, not the flat `ToolSchema`
  struct. Serializing `ToolSchema` directly 400s on any compliant provider. A permissive test double
  accepts the flat form happily, so assert the envelope in a test — this bug shipped once because
  every test used a stub that ignored the request body.
- **Config tests must not read the real user config.** Use `Config::load_with(user, explicit, cwd)`
  and pass `None` for the user path. `Config::load` picks up
  `~/Library/Application Support/minion/config.toml`, so a developer's own `minion init` breaks tests
  on any machine that has one.
- **A gateway saying "upstream request failed" is rejecting the request body.** Bisect the fields
  against the live endpoint instead of guessing: capture the exact JSON, replay it, then remove one
  key at a time. Cloudflare also blocks python `urllib`'s default signature with
  `403 / error code 1010`, so a diagnostic script must send a normal `User-Agent`.
- **reqwest 0.13**: TLS feature is `rustls`, not `rustls-tls`. We build it with
  `default-features = false`.
- **Provider streams use `async_stream::stream!`, not `try_stream!`.** `?` inside `tokio::select!`
  does not compile under `try_stream!`; the stream yields `Result<ChatEvent>` explicitly.
- **schemars 1.x**: tool argument schemas are derived with `schema_for!` and converted via
  `serde_json::to_value`. That requires the `derive` feature. It emits `$schema`, `title`, `format`,
  and `"type": ["integer","null"]` for `Option<T>` — accepted by OpenAI, not by every gateway.
- Retries happen **only before the first stream event** (in `OpenAiProvider::open`), so a retry can
  never duplicate text or re-run a tool. Don't move retry logic downstream.
- **`${session}` cannot be written inside a `println!`/`format!` format string.** `{session}` is
  parsed as a format argument and the compiler suggests implementing `Display` on your type. Pass it
  as `{}` with a variable, or escape the braces as `${{session}}`.
- **Do not use `rpassword` for the secret prompt.** It prints the prompt and *then* disables echo,
  which leaves a window where a pasted key is echoed into terminal scrollback. `ask_secret` in
  `init.rs` owns the termios guard so echo is off before anything is printed.
- **`Options::empty()` in pulldown-cmark disables GFM tables.** The default is `Options::all()`, so
  name the extensions you want explicitly. A table that renders as a paragraph of pipes is this.
- **pulldown-cmark's `TableHead` has no `TableRow` wrapper.** Body rows do. Assign the header on
  `End(TagEnd::TableHead)` or you get a blank heading line and column widths driven by the body.
- **Match-arm order in `collect_blocks` is load-bearing.** `Start(Strong)` is both an inline mark
  and a `Tag`, so the inline arm must come *before* the generic `Start(tag)` arm. Reversed, match
  tries arms in order, `start_block` claims the emphasis, returns `None`, and the style is dropped.
- **`collect_inlines` takes an explicit `consume_end`.** It stops at the *first* `End` it sees. A
  block caller wants that consumed; a list-item caller must not, because the event it stops on is
  the item's own `End` and the enclosing list is what has to see it. Get this wrong and the next
  item is silently swallowed. `transparent` covers the same problem one level down for links and
  images, whose `End` must not end the surrounding sentence.
- `std::env::set_var` is `unsafe` on edition 2024 and racy under `cargo test`'s threads. To test
  env-dependent behaviour, extract a pure function taking the value as a parameter, or set the
  variable on the *process* from the shell in a manual test.

## Policy rules

- **Rule order in `PolicyEngine::check` is the security property.** Deny → allow → classifier →
  ReadOnly → non-interactive → default. Reordering lets a later rule skip an earlier refusal.
- **`ReadOnly` is checked *before* the non-interactive rule; `Network` is not.** D15, and it
  deliberately does not match `Risk::requires_consent`. A piped `minion run ... > out.md` can read
  and recall, but a write or an `http_fetch` still needs `--yes` or an allowlist entry. Use
  `Risk::is_observation()` for the read/write line rather than matching `ReadOnly` — it keeps the
  side that `Network` sits on stated once. If `Network` ever moves to the read side, that helper is
  the only thing that needs changing, and it needs a new decision-log entry.
- **`policy.default = "auto"` does not silence the classifier.** `rm -rf`, `sudo`, and `curl | sh`
  always prompt. That is the whole reason the classifier exists.
- **Allowlist patterns are anchored at the start and exact unless they end in `*`.** `pattern = "echo"`
  matches only the bare command `echo`; `pattern = "echo *"` matches any echo. It fails closed, so
  a mistake is a refusal, never a leak.
- **`--yes` replaces the *default decision*, not the engine.** Deny rules and the classifier still
  apply, and a non-TTY still cannot prompt.
- **Persisted allows store the verb**, not the whole command, so approving one `cargo build` does
  not approve every `cargo` invocation.
- The approval prompt matches exact input only. First-character matching used to turn `sure` into
  session scope; keep the strict match.

## Memory rules

- **`recall` quotes every search term.** Model-written text reaches FTS5's `MATCH` directly, and
  FTS5 reads a bareword `OR`/`AND`/`NOT` as an operator. Unquoted, a query like `alpha OR beta`
  silently *widens* from "both words" to "either word" and returns rows the caller never asked for.
  `search_terms` reduces each word to alphanumerics and wraps it in quotes; the phrases are then
  implicitly `AND`ed. Don't "simplify" that away.
- **A `Transaction` must be committed explicitly** — it rolls back on drop. Omitting the `commit()`
  after an upsert makes `remember` report success while `recall` matches nothing, which is exactly
  the silent failure the FTS triggers exist to prevent. `crates/minion-cli/tests/memory_gate.rs`
  and the store tests catch this; if you touch the write path, expect them to fail first.
- **The namespace is a hash, not a path.** `namespace_for` uses FNV-1a rather than a digest
  dependency, and `the_label_is_sixteen_hex_digits` pins the algorithm. A test asserts known
  constants on purpose: if the hash changes, every workspace's memory becomes unreachable.
  Canonicalize the root before hashing, or `/ws` and `/ws/` become two namespaces.
- **A no-match `recall` is a plain answer, not a tool error.** The model asks vague questions; an
  error there reads as a broken tool. Same for a query with nothing searchable in it.
- `remember` is `Risk::Write` even though it cannot touch the workspace. The class describes what
  changes, and that is what puts it behind the gate. `crates/minion-cli/tests/memory_gate.rs`
  asserts a denied `remember` leaves the database untouched.

## Store rules

- **Migrations are a forward-only list.** Append a new `&str` to `MIGRATIONS`; never edit an applied
  one. The loop must run *that entry's* SQL, not the baseline.
- **A `query_map` closure may only return `rusqlite::Result`.** Decode columns into a small local
  struct and convert after `collect()`, or the `?` on a domain error fails to compile.
- **All store I/O goes through `Store::blocking`** (a `spawn_blocking` hop). Calling rusqlite
  inline would stall the async runtime for the duration of the query.
- **A turn is persisted as one transaction.** Never append messages one at a time across statements:
  a tool result without its assistant call is an invalid transcript.
- **Trimming must never split a turn.** `apply_history_window` keeps index 0 and advances the cut
  off a `tool` role. The invariant to test is "every `tool_calls` entry is answered", not "there
  are no tool messages" — a paired result is legitimate.
- `memory_fts` is an external-content FTS table; its triggers are what make `recall` work in M3.
  Don't drop them.

## Known gaps (don't mistake these for bugs)

- The CLI has `run`, `init`, `session`, and the default REPL. There is still no `doctor`, `cron`,
  `mcp`, or `config` subcommand, so §5.12's CLI surface is only partly built.
- **M3.5 (the System One guard, D16) is specified but not written.** `SDD.md` has FR-43–FR-47,
  the D16 entry, a T14 threat row and an R9 risk row for it, and no code exists. If you are reading
  this expecting a `[guard]` block in `config.rs` or a `minion-guard` crate, neither is there. The
  design constraints that matter when it gets written: the guard may only narrow prompts, never
  widen permissions; `privilege`/`remote-execution`/`destructive` never reach the model; every
  failure path resolves to the existing prompt; only the command string is sent as `state`.
- **The interactive approval keystroke path is not machine-tested.** The prompt renders correctly and
  `parse_choice` plus all four engine outcomes are unit-tested, but a pty harness kept
  desynchronising, so no test drives a real keypress end to end. Treat it as unverified.
- Tool calls execute **sequentially**; bounded concurrency is tracked as `FR-8` in `agent.rs`.
- `usage` is advisory and may be zero; no turn is blocked on it.
- History trimming / summarization (`history_window`, `summarize_on_truncate`) is configured but not
  yet applied by the loop.
- The provider is only smoke-tested against a stub server. There is no recorded provider fixture
  suite yet (SDD §8).
- **The markdown renderer is verified by unit tests and one manual pty pass, not fixtures.** Column
  widths are measured with `chars().count()`, so CJK and emoji will be misaligned in tables. A
  `unicode-width` dependency would fix it and would cost binary size; it is a deliberate deferral,
  not an oversight.

## `SDD.md` is the source of truth

Normative, not prose. When implementing, follow it rather than substituting defaults.

- **Appendix C (decision log) is binding.** D1–D8 are settled: Rust (not Zig), line REPL (not
  full-screen TUI), approval+allowlist (not sandbox-first), SQLite, in-process cron, MCP in both
  directions, thin hand-rolled provider client, read-only-by-default MCP surface. To deviate, add a
  new decision-log entry — don't silently diverge.
- **§11 open questions are unresolved.** Four items are marked *for review*. Don't pick answers
  without confirming first.
- **Keep IDs stable.** `G*`, `FR-*`, `NFR-*`, `T*`, `R*` are cross-referenced across sections.
  Renumbering breaks references; append instead.
- §10 milestones are the roadmap. §2 requirements are the acceptance criteria for `minion-core` and
  the tools layer; §5 is the implementation detail.

## Non-goals for v1 — do not build these

No subagents or recursive task spawning, no embeddings/vector search, no full-screen TUI panes, no
browser automation, no multi-user server or auth, no local inference server. Exactly six tool
families (§5.5): `run_command`, `cron_*`, `read_file`/`write_file`/`edit_file`, `http_fetch`,
`mcp_call`, `remember`/`recall`. Don't add tools beyond these without a decision-log entry.

## Manual smoke test

The end-to-end check is a throwaway Python stub that speaks SSE like a compatible provider: a
`read_file` tool call on turn 1, prose on turn 2, `GET /v1/models` for `init --check` and discovery,
and every request's headers appended to a log. It is intentionally **not** committed as a test
dependency. Point `--base-url` at such a stub rather than at a real provider.

The stub doubles as a check on the parts unit tests cannot reach: inspect its header log to confirm
`${session}` produced one stable id across a conversation, and that `Authorization` carries the key
from the credentials file.

**Interactive `init` needs a real pty.** The token prompt reads `/dev/tty` with echo disabled, so it
cannot be driven by piping stdin — that path takes the non-interactive branch. Drive it with
`pty.fork()` and write one answer per prompt; feeding all lines at once desynchronises the prompts
and produces a confusing failure. Always rebuild the binary (`cargo build`) before a manual test:
`cargo test` does not refresh `target/debug/minion`.
