//! The M7 CLI surface, driven through the real binary (§5.12, §9, NFR-9, NFR-10).
//!
//! Everything here is offline: the only socket opened is a connection refused on
//! loopback, which is what lets `doctor` fail fast without a backend.

use std::process::{Command, Output};

fn minion() -> Command {
    Command::new(env!("CARGO_BIN_EXE_minion"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn version_reports_the_git_sha_and_enabled_features() {
    let output = minion().arg("--version").output().expect("run --version");

    let text = stdout(&output);
    assert!(output.status.success(), "was: {text}");
    assert!(
        text.contains("features: mcp, cron"),
        "the default build enables both features; was: {text}"
    );
    // `<version> (<sha>; features: ...)`: the sha must be the commit actually
    // checked out. A release artifact that names the wrong commit is lying
    // about its provenance (§9), and `.git/HEAD`-only invalidation made that
    // happen on every incremental build — so assert against HEAD itself.
    // Outside a checkout (a source tarball) the binary reports `unknown` and
    // there is no HEAD to compare with.
    if let Some(sha) = git_head_sha() {
        assert!(
            text.contains(&sha),
            "the version line must name HEAD ({sha}); was: {text}"
        );
    }
}

/// `git rev-parse --short=12 HEAD` from the repository above this crate, or
/// `None` when this is not a git checkout or git is unavailable.
fn git_head_sha() -> Option<String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)?;
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

/// `--max-iterations 0` is rejected the same way `[agent] max_iterations = 0`
/// in the config is: exit 2, not a silent clamp to 1.
#[test]
fn max_iterations_zero_is_rejected() {
    let dir = tempfile::tempdir().unwrap();

    let output = minion()
        .arg("--cwd")
        .arg(dir.path())
        .args(["--max-iterations", "0", "config", "show"])
        .output()
        .expect("run with --max-iterations 0");

    assert_eq!(
        output.status.code(),
        Some(2),
        "a zero iteration budget is a config error; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("max_iterations must be at least 1"),
        "was: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn config_path_lists_the_paths_as_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");

    let output = minion()
        .arg("--db")
        .arg(&db)
        .arg("--cwd")
        .arg(dir.path())
        .args(["config", "path", "--json"])
        .output()
        .expect("run config path");

    assert!(output.status.success(), "was: {}", stdout(&output));
    let value: serde_json::Value =
        serde_json::from_str(stdout(&output).trim()).expect("valid json");
    let paths = value["paths"].as_array().expect("a paths array");
    let database = paths
        .iter()
        .find(|entry| entry["name"] == "database")
        .expect("a database entry");
    assert_eq!(database["path"], db.display().to_string());
}

#[test]
fn config_show_masks_a_credential_in_a_header() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("minion.toml"),
        "[provider.headers]\nAuthorization = \"Bearer sk-should-not-appear\"\n",
    )
    .unwrap();

    let output = minion()
        .arg("--cwd")
        .arg(dir.path())
        .args(["config", "show"])
        .current_dir(dir.path())
        .output()
        .expect("run config show");

    let text = stdout(&output);
    assert!(output.status.success(), "was: {text}");
    assert!(
        !text.contains("sk-should-not-appear"),
        "the header value leaked into config show: {text}"
    );
    assert!(
        text.contains("[redacted]"),
        "the header value should be masked: {text}"
    );
}

#[test]
fn doctor_fails_with_code_3_when_the_provider_does_not_answer() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");

    let output = minion()
        .env("OPENAI_API_KEY", "test-key-not-real")
        .arg("--db")
        .arg(&db)
        .arg("--cwd")
        .arg(dir.path())
        .args(["--base-url", "http://127.0.0.1:1/v1", "doctor", "--json"])
        .output()
        .expect("run doctor");

    assert_eq!(
        output.status.code(),
        Some(3),
        "an unreachable provider is exit 3; stdout: {}",
        stdout(&output)
    );
    let value: serde_json::Value =
        serde_json::from_str(stdout(&output).trim()).expect("valid json");
    assert_eq!(value["ok"], false);
    let provider = value["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["area"] == "provider")
        .expect("a provider check");
    assert_eq!(provider["ok"], false);
}

/// NFR-10: `init` without `--check` never touches the network. The base URL
/// points at a closed port, so any probe would fail loudly; the write still
/// succeeds.
#[test]
fn init_without_check_writes_offline() {
    let dir = tempfile::tempdir().unwrap();

    let output = minion()
        .args([
            "init",
            "--non-interactive",
            "--project",
            "--force",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--model",
            "offline-model",
            "--api-key-env",
            "MINION_TEST_ABSENT_KEY",
        ])
        .current_dir(dir.path())
        .output()
        .expect("run init");

    assert!(
        output.status.success(),
        "init should succeed offline; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = dir.path().join("minion.toml");
    assert!(config.exists(), "the project config was not written");
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("offline-model"), "was: {text}");

    // A written config never holds the secret, only the env var's name.
    assert!(text.contains("MINION_TEST_ABSENT_KEY"));
    assert!(!text.contains("api_key ="));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&config).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a config with a token name is still private");
    }
}

#[test]
fn an_unknown_resume_target_is_a_clear_failure_not_a_crash() {
    let dir = tempfile::tempdir().unwrap();

    let output = minion()
        .arg("--db")
        .arg(dir.path().join("state.db"))
        .arg("--cwd")
        .arg(dir.path())
        .args(["--resume", "no-such-session", "run", "hi"])
        .output()
        .expect("run with a bad resume");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no session matches"),
        "expected a clear message; was: {stderr}"
    );
}

/// §5.12: a usage error is exit `2`, and `config show` on a default config is
/// exit `0`.
#[test]
fn exit_codes_for_usage_and_success() {
    let usage = minion()
        .arg("--definitely-not-a-flag")
        .output()
        .expect("run with a bad flag");
    assert_eq!(usage.status.code(), Some(2), "a usage error is exit 2");

    let dir = tempfile::tempdir().unwrap();
    let ok = minion()
        .arg("--cwd")
        .arg(dir.path())
        .args(["config", "show"])
        .output()
        .expect("run config show");
    assert_eq!(ok.status.code(), Some(0), "config show succeeds");
}
