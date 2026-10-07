# minion

A minimal, Unix-native AI agent harness. It talks to any OpenAI-compatible
endpoint, gives the model a small set of gated tools, and keeps every
conversation in SQLite so you can resume it later.

The design is in [`SDD.md`](SDD.md), and it is normative — the code follows it
rather than improvising. [`AGENTS.md`](AGENTS.md) records the traps that have
already cost time, for anyone (human or agent) working on this next.

## Status

Milestones M0 through M6 are done. What works today:

- **Streaming agent loop** against any OpenAI-compatible endpoint
- **Eleven built-in tools** — `read_file`, `edit_file`, `apply_patch`, `write_file`,
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
- **MCP client** — external MCP servers over stdio, their tools published as
  `mcp__<server>__<tool>` with the server's own schema untouched, filtered by a
  per-server `tool_allow`, gated by the same approval engine, and carried by a
  per-server policy
- **MCP server** — `minion mcp serve` publishes the agent over stdio: `agent_ask`,
  the introspection tools, the `cron_*` tools, and a read-only surface by default
  (shell execution and file writes are opt-in flags, printed at startup)
- **SQLite sessions** — resumable, with `/resume` by id, prefix, or position
- **`minion init`** — one command from a bare machine to a working config
- **Self-update** — `minion update` checks a release channel, verifies the download against
  `checksums.txt` and the commit it embeds, and replaces the installed binary atomically, keeping the
  previous one for `--rollback`
- **Markdown rendering** on a terminal, including tables

Not built yet: `minion doctor`, `minion config`, and a daemon that runs cron jobs while no session is
open (that is M7).

## Install

Needs Rust 1.89 or newer (the code uses let-chains, so edition 2024).

```sh
cargo install --path crates/minion-cli
```

Or build in place with `cargo build`; the binary lands at `target/debug/minion`.

Prebuilt binaries are published as assets of a tagged GitHub release; `minion update` (below) installs
them, and there is nothing to do by hand once one exists.

`minion --version` reports what a running binary was built from:

```
minion 0.1.0 (f7581dae470a 2026-10-07) [features: cron,guard,mcp,update]
```

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

## MCP client

minion can consume external [MCP](https://modelcontextprotocol.io) servers and give
their tools to the model. Each server is spawned over stdio and its tool list is
republished under a namespaced name:

```toml
[mcp.client.servers.files]
command = "mcp-server-files"
args = ["--root", "/srv"]
tool_allow = ["read_*", "list"]   # empty allows nothing; ["*"] allows everything
approval = "ask"                  # auto | ask | deny — substitutes the global default
lazy = false                      # true: contact it on the first turn instead of at startup
```

The server's tools then reach the model as `mcp__files__read_file` and friends, with
the server's own `inputSchema` passed through verbatim. Its name is the table key, so
a server name may not contain `__`.

```sh
minion mcp list              # configured servers, whether they came up, tools published
minion mcp tools files       # everything one server lists, and what minion publishes of it
```

Three things are worth knowing:

- **`tool_allow` is a filter, not a warning.** A tool that is not listed is absent from
  the schemas the model receives and cannot be called by name. The list is fail-closed:
  an empty one publishes nothing.
- **An external tool is gated exactly like `run_command`.** It goes through the same
  approval engine, and its risk class is always `network` — the protocol's own risk
  annotations are hints from a server you have not vouched for, so they are ignored
  rather than trusted.
- **A server that will not start costs only its tools.** The turn still runs, the tools
  are gone from the catalogue, and a `system` message says why. Every turn retries it,
  so a server that comes back is offered again.

The per-server `approval` replaces the global default *for that server's tools*: a
trusted local server can be `auto` while a server reached over the network stays `ask`.
Deny rules, allowlists and the command classifier still run first.

## MCP server

The other direction: minion can *be* an MCP server, so another model or harness can
drive it. It speaks stdio, and it never starts a REPL — both own stdin/stdout, so
`mcp serve` is a mode of its own.

```sh
minion mcp serve
```

The surface is read-only by default:

| Tool | What it does |
|---|---|
| `agent_ask` | Run one agent turn with minion's own model backend. Params: `prompt`, `session_id?`, `model?`, `max_iterations?`, `allow_tools?` |
| `agent_list_sessions` | Stored conversations, newest first |
| `agent_get_session` | One conversation's transcript |
| `agent_list_tools` | The read-only tools `agent_ask` may call, with risk classes |
| `cron_add` / `cron_list` / `cron_remove` | Manage scheduled jobs (present when `expose_cron_write`) |
| `agent_run_command` | Shell execution — **refused** unless `expose_exec` |
| `agent_write_file` | File writes — **refused** unless `expose_write` |

It also serves the resources `minion://sessions`, `minion://sessions/{id}`,
`minion://jobs` and `minion://config-redacted`, and the `minion_agent` prompt for
hosts that support prompts but not tools.

```toml
[mcp.server]
expose_exec = false        # agent_run_command is listed but refused
expose_write = false       # agent_write_file is listed but refused
expose_cron_write = true   # the cron tools are published
```

Three things are worth knowing:

- **`agent_run_command` and `agent_write_file` are listed and refused, not hidden.**
  Without their flag the call reaches the same policy engine as everything else and is
  denied there, with the refusal written to the audit trail and returned to the caller
  as a readable tool error. Turning a flag on is the operator's consent; deny rules and
  allowlists still run first, so it cannot grant something already refused.
- **The startup banner states the capabilities.** `minion mcp serve` prints one line per
  flag on stderr, and shouts about the enabled ones, so nobody enables remote code
  execution by accident.
- **`agent_ask` is read-only whatever the flags say.** It runs a turn against the
  read-only subset of the built-in tools; `expose_write`/`expose_exec` add the *direct*
  tools, they do not widen the agent. `minion://config-redacted` never carries a secret.

Jobs created over MCP are stored in the same database, but `mcp serve` does not run the
scheduler — they fire the next time a process that does (a REPL or `minion run`) opens it.

## Update

`minion update` checks a release channel and, when a newer version is published, downloads the
prebuilt binary for your platform, verifies it, and replaces the installed one.

```sh
minion update --check     # report only; exits 1 when an update is available
minion update             # prompts, then installs
minion update --yes       # no prompt (required when stdin is not a terminal)
minion update --force     # reinstall even if already current
minion update --rollback  # restore the previous binary
minion update --check --json
```

What it checks before replacing anything:

1. **The checksum.** The downloaded binary must match its entry in the release's `checksums.txt`. A
   mismatch aborts with the installed binary untouched.
2. **The commit.** The downloaded binary is run with `--version`, and the SHA it reports must match the
   commit the release declares. A release that declares no commit is refused rather than trusted.

Only then is the new binary moved over the old one with `rename()`, which is atomic. The previous
binary is kept beside it as `minion.old-<version>`, which is what `--rollback` restores.

Three things are worth knowing:

- **`--check` writes nothing.** It reports the installed version, the latest, and the asset for your
  platform, and exits `0` when up to date or `1` when an update is available.
- **Nothing is installed unattended without `--yes`.** With no terminal there is no prompt, so minion
  refuses rather than guessing — `printf y | minion update` does not update anything.
- **If the install directory is not writable, minion does not call `sudo`.** It verifies and stages the
  binary where it can write, then prints the two commands for you to run as an administrator (back up
  the old binary, then install the new one).

The channel is a GitHub-compatible releases API and needs no token — the repository is public. To point
at a mirror:

```toml
[update]
api_url = "https://api.github.com"   # https, or http only on loopback
repo = "Az107/minion"
asset_prefix = "minion"              # assets are `<prefix>-<os>-<arch>` plus checksums.txt
```

An update replaces exactly one file. It does not touch the config, the database, or the credentials
file.

## Releasing

Releases are cut by `.github/workflows/release.yml`. There is no command to run by hand.

A **push to `main`** does the following, in order:

1. **Derives the next version** from the newest `vX.Y.Z` tag (`scripts/next-version.sh`): no tag yet →
   `v0.1.0`; otherwise the patch is bumped, `vX.Y.Z` → `vX.Y.(Z+1)`. If `HEAD` already carries a tag
   (a re-run of the same commit), the whole job is skipped, so a published commit is never republished.
2. **Refuses to publish** unless that version equals `[workspace.package].version` in `Cargo.toml`.
   The binary embeds that version and `minion update` compares it to the release tag, so the two have
   to agree — a release whose tag and embedded version disagree would make `minion update` offer the
   same release forever.
3. **Runs the tests** (`cargo +1.89.0 test --workspace --locked`). A red tree publishes nothing.
4. **Builds `linux/amd64` and `linux/arm64`** binaries: amd64 natively, arm64 cross-compiled with
   `gcc-aarch64-linux-gnu`, both with the commit and date embedded (`MINION_GIT_SHA`/`MINION_GIT_DATE`).
   The job is pinned to the `ubuntu-22.04` runner so the glibc floor of the binaries stays at 2.35
   instead of moving when `ubuntu-latest` rotates.
5. **Publishes a GitHub Release** at the derived tag with `minion-linux-amd64`, `minion-linux-arm64`
   and `checksums.txt`, then verifies the release is not left as a draft and that the uploaded
   `checksums.txt` matches the one built.

**So a normal release is: bump `[workspace.package].version` to the next patch in a commit and push it
to `main`.** The bump is what step 2 checks; forgetting it fails the run rather than publishing a
binary whose version disagrees with its tag.

**Cutting a tag by hand** (only when the automatic patch bump cannot produce the version you want, or
to re-cut after a failed run). The workflow runs on a push to `main` and skips a commit that is already
tagged, so a hand-cut tag is not picked up on its own — create its Release yourself:

```sh
git switch main && git pull
git tag v0.2.0                       # or the version you need
git push origin v0.2.0
cargo +1.89.0 test --workspace --locked
cargo +1.89.0 build --release --locked
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  cargo +1.89.0 build --release --locked --target aarch64-unknown-linux-gnu
mkdir -p dist
cp target/release/minion                             dist/minion-linux-amd64
cp target/aarch64-unknown-linux-gnu/release/minion   dist/minion-linux-arm64
( cd dist && sha256sum minion-linux-* >checksums.txt )
gh release create v0.2.0 --title v0.2.0 \
  --notes "build-commit: $(git rev-parse --short=12 HEAD)" \
  dist/minion-linux-amd64 dist/minion-linux-arm64 dist/checksums.txt
```

**Verifying a release:**

```sh
gh release view v0.1.0                 # assets: minion-linux-amd64, minion-linux-arm64, checksums.txt
minion update --check                  # exits 0 (up to date) or 1 (update available)
minion update --check --json           # installed, latest, asset and the commit the release declares
```

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
- **External MCP tools are not a second path.** They are ordinary tools behind the same gate, with
  `network` risk, and only the ones a server's `tool_allow` names exist at all. A server is spawned
  directly, never through a shell.
- **The MCP server is read-only by default.** `agent_ask` runs a turn with the read-only tool subset;
  shell execution and file writes over MCP are opt-in flags that are printed at startup, and a
  disabled capability is refused by the policy engine — visibly, and in the audit trail — rather than
  quietly absent. `minion://config-redacted` strips secret-shaped header values.
- Approval is remembered by **verb**, so approving one `cargo build` does not
  approve every `cargo` invocation.

## License

MIT
