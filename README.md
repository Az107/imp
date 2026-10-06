# minion

A minimal, Unix-native AI agent harness. It talks to any OpenAI-compatible
endpoint, gives the model a small set of gated tools, and keeps every
conversation in SQLite so you can resume it later.

The design is in [`SDD.md`](SDD.md), and it is normative — the code follows it
rather than improvising. [`AGENTS.md`](AGENTS.md) records the traps that have
already cost time, for anyone (human or agent) working on this next.

## Status

Milestones M0 through M4 are done. What works today:

- **Streaming agent loop** against any OpenAI-compatible endpoint
- **Eleven tools** — `read_file`, `edit_file`, `apply_patch`, `write_file`,
  `run_command`, `remember`, `recall`, `http_fetch`, `cron_add`, `cron_list`,
  `cron_remove`
- **Approval engine** — every write, exec, and network call is gated by a policy
  engine, with an allowlist, persisted approvals, and a fail-closed
  non-interactive path
- **Optional System One guard** — an opt-in `/v1/systemone` judge may resolve a
  flagged `run_command` prompt when it rates the command reversible; destructive,
  privileged and remote-execution commands are resolved before any network call
  and always prompt
- **Workspace memory** — `remember`/`recall` over SQLite FTS5, scoped to the
  canonical workspace root
- **Guarded HTTP** — `http_fetch` with a domain allowlist (`https` unless an
  exact entry names the host), a private/loopback/metadata address block, no
  cross-domain redirects, and a response cap
- **In-process cron** — jobs on 5-field cron expressions in an IANA timezone,
  with run history, a concurrency cap, overlap detection, and catch-up for
  occurrences missed while minion was closed
- **SQLite sessions** — resumable, with `/resume` by id, prefix, or position
- **`minion init`** — one command from a bare machine to a working config
- **Markdown rendering** on a terminal, including tables

Not built yet: MCP in either direction. `minion-mcp` is still an empty stub.

## Install

Needs Rust 1.89 or newer (the code uses let-chains, so edition 2024).

```sh
cargo install --path crates/minion-cli
```

Or build in place with `cargo build`; the binary lands at `target/debug/minion`.

## Use

```sh
minion init                     # pick a preset, paste a token, done
minion                          # REPL
minion run "summarize the TODOs" # one shot
```

`init` writes a config to your state directory and the API key to a separate
`0600` credentials file. **The config never contains the secret** — project
configs get committed. Credentials resolve from the environment variable first,
then that file.

Presets: `openai`, `openrouter`, `opencode-go`, `opencode-zen`, `ollama`,
`vllm`, `custom`. The `opencode-*` ones set the `x-opencode-session` header,
which those gateways require.

### In the REPL

```
/new  /sessions  /resume <id>  /rename  /clear  /model  /tools  /cost
/cron  /session  /where  /help  /quit
!<command>          run a shell command directly, bypassing the model
```

`/session` shows the conversation id that goes out in `${session}` headers. It
is stable for the whole conversation on purpose — regenerating it per request
would break the prompt-cache routing the header exists for.

### Output

Assistant text goes to **stdout**; tool activity, approvals, and errors go to
**stderr**. So this gives you just the answer:

```sh
minion run "explain this error" > answer.txt
```

On a terminal, assistant text is rendered as markdown — headings, emphasis,
code, lists, and pipe tables with aligned columns. When stdout is a pipe the
raw markdown is emitted instead, because that is the more useful thing to
capture. `NO_COLOR` and `--no-color` drop the escapes but keep table borders:
alignment carries meaning, colour does not.

`--json` emits newline-delimited events for scripting.

## Cron

Jobs are prompts on a schedule. They live in the same SQLite database as the
conversations, and they run **inside** a running minion — there is no daemon and
no crontab entry, so a job does not fire while minion is closed. Missed
occurrences are dealt with at the next start, according to `[cron]
missed_run_policy`.

```sh
minion cron add --schedule '0 9 * * 1' --prompt 'summarize the week' --name weekly
minion cron list
minion cron remove weekly
```

The schedule is a five-field cron expression (`minute hour day-of-month month
day-of-week`) read in an IANA timezone — `--timezone America/Mexico_City`, or
`[cron] timezone`. Day of week is `0` or `7` for Sunday and `1` for Monday, as
in every other cron. `--session-mode reuse` keeps one conversation across runs
instead of starting a fresh one each time, and `--max-runs` retires a job after
a fixed number of runs.

A run's history is in `job_runs`: its status, when it started and finished, and
a summary. A job that is still running when its next occurrence comes due is
recorded as an overlap rather than started twice, unless it was created with
`--allow-overlap`. `/cron` in the REPL shows the job table.

**A job prompt cannot ask for approval.** It has no terminal, so a tool the
policy would `ask` about is refused unless an allowlist entry covers it — a job
cannot promote itself, and it cannot reach the System One guard either. See
`[policy.allow]` and the safety model below.

## Safety model

There is no sandbox. The model can only act through tools, and every tool that
writes or executes goes through a policy engine that decides *before* the call:

- **Deny rules always win**, over any allowlist.
- Some commands always prompt regardless of policy — `rm -rf`, `sudo`,
  `curl | sh`. `default = "auto"` does not silence that classifier.
- A **non-TTY never prompts and defaults to deny.** CI is not a place to
  accidentally run something.
- `--yes` replaces the default *decision*, not the engine. Deny rules and the
  classifier still apply.
- Path-taking tools are confined to the workspace root.
- `http_fetch` reaches only hosts in its `[http_fetch]` allowlist, refuses
  private, loopback, link-local and cloud-metadata addresses, and does not
  follow a redirect off an allowed host.
- The **optional System One guard** (`[guard]`, off by default) is a noise filter
  inside that boundary, not a new authority: it can turn a *prompt* into a silent
  allow and nothing else. `privilege`, `remote-execution` and `destructive`
  commands never reach it, only the command string is ever sent, and any failure
  — network, timeout, rate limit, unreadable body — falls back to the prompt.
- **Cron runs fail closed.** A job's prompt is executed behind a gate with no approval UI at all, so
  anything the policy would `ask` about is denied rather than approved by whoever happened to be at
  the keyboard. Deny rules and allowlists still apply first.
- Approval is remembered by **verb**, so approving one `cargo build` does not
  approve every `cargo` invocation.

## License

MIT
