//! Bake the git SHA and the enabled cargo features into `minion --version` (§9).
//!
//! A released binary should say which commit it came from and which optional
//! capabilities it was built with, without the operator having to guess.

use std::path::Path;
use std::process::Command;

fn main() {
    // Re-run when the checkout moves: the SHA is part of the output.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=GIT_DIR");
    // `.git/HEAD` alone is not enough: on a branch it holds a *ref name*, not a
    // commit, so a new commit on that branch leaves it untouched and the baked
    // SHA goes stale on every incremental build. The ref file, the reflog and
    // the packed refs all move with the commit; cargo re-scans a directory, so
    // watching `.git/refs` catches a ref that lives in its own file.
    for path in [
        "../../.git/HEAD",
        "../../.git/refs",
        "../../.git/logs/HEAD",
        "../../.git/packed-refs",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }

    let mut features: Vec<&str> = Vec::new();
    if std::env::var_os("CARGO_FEATURE_MCP").is_some() {
        features.push("mcp");
    }
    if std::env::var_os("CARGO_FEATURE_CRON").is_some() {
        features.push("cron");
    }
    let features = if features.is_empty() {
        "none".to_string()
    } else {
        features.join(", ")
    };

    let version = format!(
        "{} ({}; features: {features})",
        env!("CARGO_PKG_VERSION"),
        git_sha()
    );
    println!("cargo:rustc-env=MINION_VERSION={version}");
}

/// The short commit SHA, or `unknown` outside a git checkout.
fn git_sha() -> String {
    // `CARGO_MANIFEST_DIR` is `crates/minion-cli`; the repository is two levels
    // up, which is where `.git` lives.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let root = Path::new(&manifest)
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Path::new(".").to_path_buf());

    let output = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "--short=12", "HEAD"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        // A source tarball has no history; say so rather than invent a hash.
        _ => "unknown".to_string(),
    }
}
