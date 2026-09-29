# AGENTS.md

## Status

Rust workspace implementing `SDD.md`. Milestones **M0 and M0.5 are complete**: config and layered
credential resolution, an OpenAI-compatible streaming provider with `${session}` header support, the
agent loop, `read_file`, one-shot `run`, the REPL, and `minion init`. Branch is `main`.

`minion-store`, `minion-cron`, and `minion-mcp` are **empty stubs** for M4–M6. Their manifests
list real dependencies (`rusqlite`, `rmcp`, `cron`), but there is no code behind them yet.

## Commands

```sh
cargo build                       # whole workspace
cargo test --workspace             # all tests; offline, no network, no API key needed
cargo test -p minion-core          # one crate
cargo test -p minion-core agent::tests::runs_a_tool_and_feeds_the_result_back   # one test
cargo clippy --all-targets -- -D warnings    # lint gate; currently clean
cargo fmt --all
./target/debug/minion --help
```

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
- Config overlays are deep-merged on `toml::Value` trees, so a project `minion.toml` only restates
  the keys it changes. **Unknown keys are ignored on purpose** (forward compatibility) — do not add
  `deny_unknown_fields`.
- Every session gets a `session_id` (`minion_core::new_session_id()`, a v7 UUID). It is expanded
  into any configured header containing `${session}`. Some gateways require this: OpenCode Go
  rejects requests without `x-opencode-session`. The id must stay **stable for the whole
  conversation** — regenerating it per request would break the prompt-cache routing it exists for.
- `minion init` deliberately runs *before* config loading in `main.rs`, so a broken or missing
  config can still be repaired by it. Keep that ordering.

## Invariants that must not be relaxed

- **Only read-only tools are registered.** `default_registry()` in `minion-tools` registers just
  `read_file`. Do not add a `Write`/`Execute` tool (`write_file`, `run_command`) before the approval
  engine exists — an unguarded write tool violates the fail-closed rule in the SDD.
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

- **reqwest 0.13**: TLS feature is `rustls`, not `rustls-tls`. We build it with
  `default-features = false`.
- **Provider streams use `async_stream::stream!`, not `try_stream!`.** `?` inside `tokio::select!`
  does not compile under `try_stream!`; the stream yields `Result<ChatEvent>` explicitly.
- **schemars 1.x**: tool argument schemas are derived with `schema_for!` and converted via
  `serde_json::to_value`. That requires the `derive` feature.
- Retries happen **only before the first stream event** (in `OpenAiProvider::open`), so a retry can
  never duplicate text or re-run a tool. Don't move retry logic downstream.
- **`${session}` cannot be written inside a `println!`/`format!` format string.** `{session}` is
  parsed as a format argument and the compiler suggests implementing `Display` on your type. Pass it
  as `{}` with a variable, or escape the braces as `${{session}}`.
- **Do not use `rpassword` for the secret prompt.** It prints the prompt and *then* disables echo,
  which leaves a window where a pasted key is echoed into terminal scrollback. `ask_secret` in
  `init.rs` owns the termios guard so echo is off before anything is printed.
- `std::env::set_var` is `unsafe` on edition 2024 and racy under `cargo test`'s threads. To test
  env-dependent behaviour, extract a pure function taking the value as a parameter, or set the
  variable on the *process* from the shell in a manual test.

## Known gaps (don't mistake these for bugs)

- The CLI has `run` and `init` plus the default REPL. There is still no `doctor`, `session`, `cron`,
  `mcp`, or `config` subcommand, so §5.12's CLI surface is only partly built.
- Tool calls execute **sequentially**; bounded concurrency is tracked as `FR-8` in `agent.rs`.
- `usage` is advisory and may be zero; no turn is blocked on it.
- History trimming / summarization (`history_window`, `summarize_on_truncate`) is configured but not
  yet applied by the loop.
- The provider is only smoke-tested against a stub server. There is no recorded provider fixture
  suite yet (SDD §8).

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
