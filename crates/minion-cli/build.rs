//! Stamp the build with the commit it came from (SDD §9).
//!
//! `minion --version` reports the workspace version, the git SHA, the commit
//! date and the capability families compiled in. Two reasons it is done here and
//! not read at run time: the number has to be inside the binary that `update`
//! replaces (so a downloaded binary can be checked against the commit its
//! release declares), and `--version` must work with no git, no network and no
//! config.
//!
//! Every stamp degrades to `unknown` rather than failing the build. A release
//! workflow that already knows the commit can set `MINION_GIT_SHA` /
//! `MINION_GIT_DATE`; otherwise `git` is asked, and `GITHUB_SHA` is honoured so
//! the GitHub Actions default works unmodified.

use std::process::Command;

fn main() {
    // A rebuild after a commit must not keep the old SHA.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=build.rs");
    for var in [
        "MINION_GIT_SHA",
        "MINION_GIT_DATE",
        "GITHUB_SHA",
        "SOURCE_DATE_EPOCH",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    let sha = non_empty("MINION_GIT_SHA")
        .map(|value| short_sha(&value))
        .or_else(|| non_empty("GITHUB_SHA").map(|value| short_sha(&value)))
        .or_else(|| git(&["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());

    let date = non_empty("MINION_GIT_DATE")
        .or_else(|| git(&["log", "-1", "--format=%cs"]))
        .unwrap_or_else(|| "unknown".to_string());

    emit("MINION_GIT_SHA", &sha);
    emit("MINION_GIT_DATE", &date);
    emit("MINION_FEATURES", &features());
}

/// Capability families the binary is always built with, plus any cargo features.
///
/// §9's example is `(mcp, cron)`: these name what the binary can do, not cargo
/// features, because the families are compiled in unconditionally. A cargo
/// feature is appended when one exists, so the string stays truthful if a
/// capability ever becomes optional.
fn features() -> String {
    let mut names = vec![
        "cron".to_string(),
        "guard".to_string(),
        "mcp".to_string(),
        "update".to_string(),
    ];
    for (key, _) in std::env::vars() {
        if let Some(feature) = key.strip_prefix("CARGO_FEATURE_")
            && !feature.is_empty()
        {
            names.push(feature.to_ascii_lowercase().replace('_', "-"));
        }
    }
    names.sort();
    names.dedup();
    names.join(",")
}

/// `abc123…` from a possibly full SHA.
fn short_sha(value: &str) -> String {
    let value = value.trim();
    value.chars().take(12).collect()
}

/// A set environment variable that is not blank.
fn non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Run `git` in the package directory and return the first line of stdout.
fn git(args: &[&str]) -> Option<String> {
    let dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Emit a `rustc-env` stamp. Newlines are not allowed in an environment value,
/// so they are flattened rather than allowed to break the build.
fn emit(name: &str, value: &str) {
    let value: String = value
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect();
    println!("cargo:rustc-env={name}={}", value.trim());
}
