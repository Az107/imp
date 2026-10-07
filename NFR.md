# Non-functional requirements — measured (M7)

Every number below was measured on this host (Linux aarch64, 6 vCPU), against
the **release** binary, not estimated. Where a target is not met the real figure
is recorded next to it rather than the target being adjusted.

The binary measured is the static musl artifact, because that is what §9 ships:

```sh
cargo +1.89.0 build --release --locked --target aarch64-unknown-linux-musl
# -> target/aarch64-unknown-linux-musl/release/minion
```

That binary names the commit it was built from, so a figure can be traced back
to its source: `minion --version` prints `<version> (<git sha>; features: ...)`.

## The table

| ID | Target | Measured | Meets? |
|---|---|---|---|
| NFR-1 | cold start < 60 ms | min 45.7 ms · **median 54.8 ms** · p90 67.2 ms (n=90, pinned to one core) | yes (median), tight |
| NFR-2 | idle RSS < 40 MB | **9.2 MB** (VmHWM, max over 90 runs) | yes |
| NFR-3 | release binary < 15 MB (stripped, musl) | **14,579,936 B = 14.58 MB (13.90 MiB)**, static, stripped | yes |
| NFR-4 | tool output capped by default | **262,144 B (256 KiB)** — `[exec] output_cap_bytes` | yes |
| NFR-5 | default command timeout | **120 s** — `[exec] default_timeout_secs` | yes |
| NFR-6 | no lost job/message on `SIGKILL` | WAL + `synchronous=NORMAL`; a committed message survives a reopen; a run is durable before dispatch | yes |
| NFR-7 | offline test suite | **427 tests, 0 failures**, no network — the only socket is a refused loopback connection | yes |
| NFR-8 | no colour dependence | `NO_COLOR`/`--no-color` drop escapes, keep layout; piped output is never rendered | yes |
| NFR-9 | secrets never logged | API key, the configured env var's value, secret-shaped header values and `Bearer <token>` are masked at the sink | yes |
| NFR-10 | `init` offline unless `--check` | default `init` writes a config against an unreachable backend | yes |

## How each was measured

### NFR-1 and NFR-2

The binary is started as the REPL with stdin on a pipe; the time to its first
stdout line is "cold start to prompt", and `VmHWM` taken while it blocks is idle
RSS. This python is the harness used (one reused database, one discarded warmup
run, `n` samples):

```python
import os, subprocess, time
BIN = ".../target/aarch64-unknown-linux-musl/release/minion"
env = dict(os.environ, OPENAI_API_KEY="test-key")
def run(db, cwd):
    t = time.perf_counter()
    p = subprocess.Popen([BIN, "--db", db, "--cwd", cwd],
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, env=env)
    p.stdout.readline()                      # the banner = at the prompt
    ms = (time.perf_counter() - t) * 1000
    hwm = None
    time.sleep(0.2)
    with open(f"/proc/{p.pid}/status") as fh:      # idle RSS
        for row in fh:
            if row.startswith("VmHWM:"): hwm = int(row.split()[1]); break
    p.stdin.close(); p.wait()
    return ms, hwm
```

Run pinned to one core (`taskset -c 3 python3 …`) the median is **54.8 ms**
(n=90); unpinned on the same host it is **61.2 ms** (n=90), and a single 7-sample
pass ranged 54–64 ms. The spread is scheduler noise — the harness forks,
execs and migrates — not the binary: pinning removes it. The measurement still
includes python's `fork`/`exec`, so the binary's own cold start is lower. NFR-1
is met at the median but with little headroom, and the p90 is over.

### NFR-3

```sh
stat -c '%s bytes' target/aarch64-unknown-linux-musl/release/minion   # 14579936
file target/aarch64-unknown-linux-musl/release/minion                 # statically linked, stripped
```

### NFR-4, NFR-5

Defaults in `minion-core::config`, asserted by `config::tests::defaults_match_the_spec`:

```sh
cargo +1.89.0 test -p minion-core config::tests::defaults_match_the_spec
```

### NFR-6

```sh
cargo +1.89.0 test -p minion-store tests::the_store_runs_in_wal_mode
cargo +1.89.0 test -p minion-store tests::a_committed_message_survives_a_reopen
cargo +1.89.0 test -p minion-cron   # the durable-before-dispatch test
```

### NFR-7

```sh
cargo +1.89.0 test --workspace --locked    # 427 passed, 0 failed, no network
```

### NFR-8

```sh
cargo +1.89.0 test -p minion-cli run::tests
```

### NFR-9

```sh
cargo +1.89.0 test -p minion-cli logging::tests
cargo +1.89.0 test -p minion-cli --test cli_surface \
    config_show_masks_a_credential_in_a_header
```

### NFR-10

```sh
cargo +1.89.0 test -p minion-cli --test cli_surface init_without_check_writes_offline
```

## Building for musl on this host

`rustup target add aarch64-unknown-linux-musl` supplies the Rust standard library,
but `libsqlite3-sys` (bundled SQLite) still needs a musl **C** compiler, and the
host has none. Without root, one was assembled from unpacked Debian packages:

```sh
apt-get download musl musl-dev musl-tools          # no root needed
for d in musl musl-dev musl-tools; do dpkg-deb -x ${d}_*_arm64.deb /tmp/muslpkg/root; done
# patch the specs to point inside /tmp/muslpkg/root, then a wrapper of the same
# name cc-rs looks for:
CC_aarch64_unknown_linux_musl=/tmp/muslbin/aarch64-linux-musl-gcc \
AR_aarch64_unknown_linux_musl=aarch64-linux-gnu-ar \
  cargo +1.89.0 build --release --locked --target aarch64-unknown-linux-musl
```

The clean alternative, if a root shell is available: `sudo apt-get install -y
musl-tools`, then build with `CC_aarch64_unknown_linux_musl=musl-gcc`.
