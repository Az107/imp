//! Self-update: deciding whether a newer release exists and replacing the
//! installed binary with it.
//!
//! This module is the policy and the filesystem work. It contains **no network
//! code**: a release is obtained through [`UpdateSource`], whose only
//! implementation that opens a socket is `imp_update::HttpUpdateSource`. That
//! is the same split `imp-guard` uses (D16), and it is what lets every rule
//! below be tested against an in-memory source.
//!
//! The invariants, in order of how expensive getting them wrong is:
//!
//! 1. **Nothing is executed before it is verified.** A downloaded asset is
//!    checked against the `checksums.txt` of the *same* release, and then the
//!    staged binary is run with `--version` and its embedded commit must match
//!    the commit the release declares. Either check failing leaves the installed
//!    binary untouched.
//! 2. **The replacement is atomic.** The new bytes are written beside the target
//!    and moved over it with `rename()`, so there is no window in which the path
//!    is a half-written file. The previous binary is kept as
//!    `imp.old-<version>`, which is also what `--rollback` restores.
//! 3. **A directory we cannot write is not worked around.** We do not elevate:
//!    the operator is handed the literal commands to run with `sudo`.
//!
//! Nothing here reads or writes the config, the database, or the credentials
//! file. An update replaces one file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Name of the checksum manifest every release must carry.
pub const CHECKSUMS_ASSET: &str = "checksums.txt";

/// One downloadable file attached to a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    /// File name as published, e.g. `imp-linux-arm64`.
    pub name: String,
    /// Absolute URL to download it from.
    pub url: String,
    /// Size in bytes, when the channel reports one.
    pub size: Option<u64>,
}

/// A published release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseInfo {
    /// Tag exactly as published, e.g. `v0.2.0`.
    pub tag: String,
    /// Version without the leading `v`.
    pub version: String,
    /// Release notes, when the release carries them.
    pub notes: Option<String>,
    /// Commit the release was built from.
    ///
    /// Taken from the channel's `target_commitish` when that is a hex SHA, or
    /// from a `build-commit: <sha>` trailer in the notes. `None` means the
    /// release did not declare one, and the update refuses to install rather
    /// than trust a binary it cannot tie to a commit.
    pub commit: Option<String>,
    /// Attached files.
    pub assets: Vec<Asset>,
}

impl ReleaseInfo {
    /// The asset with this exact name, if the release carries it.
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }
}

/// Where releases come from.
///
/// The only thing this trait hides is the socket. A test implementation serves
/// an in-memory manifest; `imp_update::HttpUpdateSource` serves the real one.
#[async_trait]
pub trait UpdateSource: Send + Sync {
    /// The latest published release.
    async fn latest(&self) -> Result<ReleaseInfo>;

    /// Fetch the bytes at `url` — an asset of the release just returned.
    async fn download(&self, url: &str) -> Result<Vec<u8>>;
}

/// What a version comparison found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionOrder {
    /// The first version is older.
    Older,
    /// They are the same.
    Same,
    /// The first version is newer.
    Newer,
}

/// What the check decided to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The installed version is the latest.
    UpToDate {
        /// Version installed.
        installed: String,
    },
    /// A newer release is available.
    Update {
        /// Version installed.
        from: String,
        /// Version published.
        to: String,
    },
    /// The installed binary is newer than the latest release — a local build, or
    /// a release that was withdrawn. Nothing to do.
    InstalledIsNewer {
        /// Version installed.
        installed: String,
        /// Version published.
        latest: String,
    },
}

impl Decision {
    /// Whether this decision means a newer binary is available.
    pub fn update_available(&self) -> bool {
        matches!(self, Decision::Update { .. })
    }
}

/// A verified download, ready to be installed.
#[derive(Debug, Clone)]
pub struct Download {
    /// Version the bytes are.
    pub version: String,
    /// Asset name the bytes came from.
    pub asset: String,
    /// The bytes themselves, already checked against `checksums.txt`.
    pub bytes: Vec<u8>,
    /// Commit the release declares, to be matched against the staged binary.
    pub expected_sha: String,
}

/// What an installed update did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// The path that now holds the new binary.
    pub target: PathBuf,
    /// Where the previous binary was kept.
    pub backup: PathBuf,
    /// Version installed.
    pub version: String,
}

/// The check/install flow, driven by a source and the installed version.
pub struct Updater<'a, S: UpdateSource> {
    source: &'a S,
    /// Version of the binary we would be replacing.
    installed: String,
    /// Asset prefix from `[update].asset_prefix`.
    asset_prefix: String,
    /// Reinstall even when the versions match.
    force: bool,
}

impl<'a, S: UpdateSource> Updater<'a, S> {
    /// Build an updater for the installed version and asset prefix.
    pub fn new(
        source: &'a S,
        installed: impl Into<String>,
        asset_prefix: impl Into<String>,
    ) -> Self {
        Self {
            source,
            installed: installed.into(),
            asset_prefix: asset_prefix.into(),
            force: false,
        }
    }

    /// Reinstall the same version when it is already current (`--force`).
    pub fn force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Fetch the latest release.
    pub async fn release(&self) -> Result<ReleaseInfo> {
        self.source.latest().await
    }

    /// Compare the installed version with a fetched release.
    pub fn decide(&self, release: &ReleaseInfo) -> Decision {
        match compare_versions(&self.installed, &release.version) {
            VersionOrder::Older => Decision::Update {
                from: self.installed.clone(),
                to: release.version.clone(),
            },
            VersionOrder::Same => Decision::UpToDate {
                installed: self.installed.clone(),
            },
            VersionOrder::Newer => Decision::InstalledIsNewer {
                installed: self.installed.clone(),
                latest: release.version.clone(),
            },
        }
    }

    /// Download and verify the asset for this platform.
    ///
    /// Verification happens here, before anything touches the installed binary:
    /// the manifest is fetched first, the asset is hashed, and a mismatch is an
    /// error rather than a warning.
    pub async fn download(&self, release: &ReleaseInfo) -> Result<Download> {
        let name = platform_asset(&self.asset_prefix)?;
        let Some(asset) = release.asset(&name) else {
            return Err(Error::Refused(format!(
                "release {} has no asset `{name}` for this platform",
                release.tag
            )));
        };
        let Some(manifest) = release.asset(CHECKSUMS_ASSET) else {
            return Err(Error::Refused(format!(
                "release {} has no `{CHECKSUMS_ASSET}`; refusing to install an unverifiable binary",
                release.tag
            )));
        };

        let raw = self.source.download(&manifest.url).await?;
        let manifest_text = String::from_utf8(raw).map_err(|_| {
            Error::Refused(format!(
                "release {} has a `{CHECKSUMS_ASSET}` that is not text",
                release.tag
            ))
        })?;
        let sums = parse_checksums(&manifest_text);
        let Some(expected) = sums.get(&name) else {
            return Err(Error::Refused(format!(
                "`{CHECKSUMS_ASSET}` of {} has no entry for `{name}`",
                release.tag
            )));
        };

        let bytes = self.source.download(&asset.url).await?;
        let actual = crate::sha256::sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(Error::Refused(format!(
                "checksum mismatch for `{name}`: {CHECKSUMS_ASSET} says {expected}, \
                 the download is {actual} — refusing to install"
            )));
        }

        let Some(expected_sha) = release.commit.clone() else {
            return Err(Error::Refused(format!(
                "release {} declares no commit; refusing to install a binary that cannot be \
                 tied to a commit (set `target_commitish` to the SHA or add a `build-commit:` \
                 line to the release notes)",
                release.tag
            )));
        };

        Ok(Download {
            version: release.version.clone(),
            asset: name,
            bytes,
            expected_sha,
        })
    }

    /// Replace `target` with the verified bytes.
    pub fn install(&self, download: &Download, target: &Path) -> Result<InstallReport> {
        stage_and_replace(
            target,
            &download.bytes,
            &download.expected_sha,
            &download.version,
            &self.installed,
        )
    }
}

/// Whether a URL may be fetched by the update channel.
///
/// `https` always; a plain `http://` only to a literal loopback host
/// (`127.0.0.1`, `::1`, `localhost`), mirroring the rule `http_fetch` uses for a
/// local dev server (§5.5, D17). The default `api_url` is GitHub over TLS, so
/// the exception exists for a test fixture and for a mirror on the same machine
/// — not for a release channel on the open network.
pub fn url_is_permitted(url: &str) -> bool {
    let url = url.trim();
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(inner) = hostport.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

/// The asset name for the current platform, given the configured prefix.
pub fn platform_asset(prefix: &str) -> Result<String> {
    asset_for(prefix, std::env::consts::OS, std::env::consts::ARCH)
}

/// The asset name for an explicit platform — the pure half of
/// [`platform_asset`], so the naming rule can be tested without lying about the
/// machine the tests run on.
pub fn asset_for(prefix: &str, os: &str, arch: &str) -> Result<String> {
    let os = match os {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "windows",
        other => {
            return Err(Error::Refused(format!(
                "no prebuilt binary is published for `{other}`"
            )));
        }
    };
    let arch = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => {
            return Err(Error::Refused(format!(
                "no prebuilt binary is published for the `{other}` architecture"
            )));
        }
    };
    Ok(format!("{prefix}-{os}-{arch}"))
}

/// Parse a `sha256sum`-shaped manifest into `name -> hex`.
///
/// Accepts both `hash  name` and `hash *name`, skips blank lines and `#`
/// comments, and ignores anything whose first field is not a 64-character hex
/// digest — a malformed line must not become a checksum.
pub fn parse_checksums(text: &str) -> BTreeMap<String, String> {
    let mut sums = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((hash, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let name = rest.trim().trim_start_matches('*').trim();
        if !is_hex_digest(hash) || name.is_empty() {
            continue;
        }
        sums.insert(name.to_string(), hash.to_ascii_lowercase());
    }
    sums
}

/// A 64-character hex string.
fn is_hex_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Compare two version strings, numerically where it means something.
///
/// Leading `v` is ignored; each dot-separated component is read as a number; a
/// pre-release qualifier (after `-`) sorts *below* the bare version, matching
/// semver's intent without taking a parser dependency. This is deliberately
/// forgiving: it exists to answer "is the release newer than what I run", not to
/// validate a version.
pub fn compare_versions(left: &str, right: &str) -> VersionOrder {
    let (lnum, lpre) = parse_version(left);
    let (rnum, rpre) = parse_version(right);

    let len = lnum.len().max(rnum.len());
    for index in 0..len {
        let l = lnum.get(index).copied().unwrap_or(0);
        let r = rnum.get(index).copied().unwrap_or(0);
        match l.cmp(&r) {
            std::cmp::Ordering::Less => return VersionOrder::Older,
            std::cmp::Ordering::Greater => return VersionOrder::Newer,
            std::cmp::Ordering::Equal => {}
        }
    }

    match (lpre, rpre) {
        (None, None) => VersionOrder::Same,
        (Some(_), None) => VersionOrder::Older,
        (None, Some(_)) => VersionOrder::Newer,
        (Some(l), Some(r)) => match l.cmp(&r) {
            std::cmp::Ordering::Less => VersionOrder::Older,
            std::cmp::Ordering::Greater => VersionOrder::Newer,
            std::cmp::Ordering::Equal => VersionOrder::Same,
        },
    }
}

/// `("1.2.0-rc1" -> ([1, 2, 0], Some("rc1")))`.
fn parse_version(value: &str) -> (Vec<u64>, Option<String>) {
    let value = value.trim().trim_start_matches('v');
    let (core, pre) = match value.split_once('-') {
        Some((core, pre)) => (core, Some(pre.to_string())),
        None => (value, None),
    };
    let numbers = core
        .split('.')
        .map(|part| part.trim().parse::<u64>().unwrap_or(0))
        .collect();
    (numbers, pre)
}

/// The version token out of a `--version` line.
///
/// `imp 0.1.0 (abc1234 2026-10-07) [features: …]` yields `0.1.0`. Finds the
/// first token that looks numeric rather than assuming a position, so extra
/// words before the version do not break it.
pub fn version_from_output(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|token| {
            token.chars().next().is_some_and(|c| c.is_ascii_digit())
                && token.contains('.')
                && token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        })
        .map(|token| token.trim_start_matches('v').to_string())
}

/// The commit SHA out of a `--version` line — the first token inside the
/// parentheses.
///
/// Returns `None` when there is no parenthesised section or the token is not a
/// commit-looking hex string, so "built without git" degrades to "no SHA" rather
/// than to a false match.
pub fn sha_from_output(text: &str) -> Option<String> {
    let open = text.find('(')?;
    let rest = &text[open + 1..];
    let close = rest.find(')')?;
    let token = rest[..close].split_whitespace().next()?;
    is_commit(token).then(|| token.to_ascii_lowercase())
}

/// Whether a token looks like a (short or full) commit SHA.
pub fn is_commit(token: &str) -> bool {
    (7..=40).contains(&token.len()) && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The commit a release notes body declares, via a `build-commit: <sha>` line.
///
/// A release channel that cannot put a SHA in `target_commitish` can still tie
/// its binary to a commit; the trailer is deliberately a plain line so it is
/// readable in the rendered release as well.
pub fn commit_from_notes(notes: &str) -> Option<String> {
    notes.lines().find_map(|line| {
        let rest = line.trim();
        let rest = rest
            .strip_prefix("build-commit:")
            .or_else(|| rest.strip_prefix("Build-Commit:"))?;
        let token = rest.trim().trim_matches('`').trim();
        is_commit(token).then(|| token.to_ascii_lowercase())
    })
}

/// Whether two commit SHAs agree, tolerating one being a prefix of the other.
///
/// The release declares a full SHA and the binary reports a short one, so the
/// comparison is on the shorter length — but both must look like commits, so
/// `unknown` can never match `unknown`.
pub fn sha_matches(reported: &str, expected: &str) -> bool {
    if !is_commit(reported) || !is_commit(expected) {
        return false;
    }
    let reported = reported.to_ascii_lowercase();
    let expected = expected.to_ascii_lowercase();
    let len = reported.len().min(expected.len());
    reported[..len] == expected[..len]
}

/// Write `bytes` to a staging file beside `target`, verify the staged binary
/// reports `expected_sha`, then move it on top of `target` atomically, keeping
/// the previous binary as `imp.old-<old_version>`.
pub fn stage_and_replace(
    target: &Path,
    bytes: &[u8],
    expected_sha: &str,
    new_version: &str,
    old_version: &str,
) -> Result<InstallReport> {
    let dir = target.parent().ok_or_else(|| {
        Error::Refused(format!(
            "cannot determine the directory of `{}`",
            target.display()
        ))
    })?;
    let temp = dir.join(staged_name());

    match write_executable(&temp, bytes) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            // We must not elevate. Leave the verified bytes somewhere readable
            // and hand the operator the exact commands.
            return Err(unwritable(dir, target, bytes, old_version));
        }
        Err(err) => {
            return Err(Error::Io(err));
        }
    }

    // Run the staged binary before it becomes the installed one. A binary that
    // will not report its version, or reports a different commit, does not get
    // to replace anything.
    match reported_sha(&temp) {
        Some(reported) if sha_matches(&reported, expected_sha) => {}
        Some(reported) => {
            let _ = std::fs::remove_file(&temp);
            return Err(Error::Refused(format!(
                "the downloaded binary reports commit `{reported}` but the release declares \
                 `{expected_sha}`; refusing to replace {}",
                target.display()
            )));
        }
        None => {
            let reason = match run_version(&temp) {
                Ok(text) => format!("its `--version` output is `{}`", text.trim()),
                Err(err) => err,
            };
            let _ = std::fs::remove_file(&temp);
            return Err(Error::Refused(format!(
                "the downloaded binary could not be verified ({reason}); refusing to replace {}",
                target.display()
            )));
        }
    }

    let backup = dir.join(format!("imp.old-{old_version}"));
    if target.exists() {
        std::fs::copy(target, &backup)?;
        let _ = set_executable(&backup);
    }

    // `rename` over an existing path is atomic on Unix, which is what lets the
    // running binary replace itself: the inode behind the old name stays alive
    // until the process exits.
    std::fs::rename(&temp, target)?;

    Ok(InstallReport {
        target: target.to_path_buf(),
        backup,
        version: new_version.to_string(),
    })
}

/// Restore the most recent `imp.old-*` beside `target`, keeping the current
/// binary as a backup in turn.
pub fn rollback(target: &Path) -> Result<InstallReport> {
    let dir = target.parent().ok_or_else(|| {
        Error::Refused(format!(
            "cannot determine the directory of `{}`",
            target.display()
        ))
    })?;

    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("imp.old-") && name.len() > "imp.old-".len() {
                let modified = entry.metadata().ok()?.modified().ok()?;
                Some((modified, entry.path()))
            } else {
                None
            }
        })
        .collect();
    candidates.sort_by_key(|(modified, _)| *modified);

    let Some((_, candidate)) = candidates.pop() else {
        return Err(Error::Refused(format!(
            "no previous version is available beside `{}` (expected a `imp.old-*` file)",
            target.display()
        )));
    };

    let Some(version) = reported_version(&candidate) else {
        let reason = run_version(&candidate)
            .err()
            .unwrap_or_else(|| "its `--version` line is unreadable".to_string());
        return Err(Error::Refused(format!(
            "cannot restore `{}`: {reason}",
            candidate.display()
        )));
    };

    // Keep the binary we are about to replace, named for what it is.
    let current = reported_version(target).unwrap_or_else(|| "unknown".to_string());
    let backup = dir.join(format!("imp.old-{current}"));
    if target.exists() {
        std::fs::copy(target, &backup)?;
        let _ = set_executable(&backup);
    }

    std::fs::rename(&candidate, target)?;

    Ok(InstallReport {
        target: target.to_path_buf(),
        backup,
        version,
    })
}

/// The `imp.old-<version>` file a rollback would restore, for reporting.
pub fn rollback_target(target: &Path) -> Option<PathBuf> {
    let dir = target.parent()?;
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("imp.old-") && name.len() > "imp.old-".len() {
                Some((entry.metadata().ok()?.modified().ok()?, entry.path()))
            } else {
                None
            }
        })
        .collect();
    candidates.sort_by_key(|(modified, _)| *modified);
    candidates.pop().map(|(_, path)| path)
}

/// `imp.old-<pid>` — the path is relative because the directory is chosen by
/// the caller.
///
/// The pid alone is not enough: two installs inside one process would stage to
/// the same path, and one could truncate the file the other is executing. A
/// process-wide counter keeps each staging name distinct.
fn staged_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(".imp.new-{}-{seq}", std::process::id())
}

/// Write `bytes` to `path` with the executable bit set.
fn write_executable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    set_executable(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Run `path --version` and return its stdout, or why it could not be read.
///
/// A just-written executable can still be reported as busy by the kernel
/// (`ETXTBSY`) for a moment after its writer closed the file, especially on a
/// loaded machine. That is a transient, so the call is retried briefly before
/// giving up — a real refusal still happens, it just is not a race.
fn run_version(path: &Path) -> std::result::Result<String, String> {
    let mut last = String::new();
    for attempt in 0..10 {
        match std::process::Command::new(path).arg("--version").output() {
            Ok(output) if output.status.success() => {
                return Ok(String::from_utf8_lossy(&output.stdout).to_string());
            }
            Ok(output) => {
                last = format!(
                    "`{}` --version exited with {}",
                    path.display(),
                    output.status
                );
            }
            Err(err) if err.raw_os_error() == Some(26) => {
                // ETXTBSY: the file is momentarily open for writing elsewhere.
                last = format!("`{}` --version could not start: {err}", path.display());
                std::thread::sleep(std::time::Duration::from_millis(5 * (attempt + 1)));
                continue;
            }
            Err(err) => {
                return Err(format!(
                    "could not run `{}` --version: {err}",
                    path.display()
                ));
            }
        }
    }
    Err(last)
}

/// The commit a binary reports, if it runs and prints one.
fn reported_sha(path: &Path) -> Option<String> {
    run_version(path)
        .ok()
        .and_then(|text| sha_from_output(&text))
}

/// The version a binary reports, if it runs and prints one.
fn reported_version(path: &Path) -> Option<String> {
    run_version(path)
        .ok()
        .and_then(|text| version_from_output(&text))
}

/// Stage the verified bytes in a writable directory and return the refusal that
/// hands the operator the literal `sudo` commands.
fn unwritable(dir: &Path, target: &Path, bytes: &[u8], old_version: &str) -> Error {
    let staged = std::env::temp_dir().join(staged_name());
    let _ = write_executable(&staged, bytes);
    let commands = manual_install_commands(target, &staged, old_version, dir);
    Error::Refused(format!(
        "`{}` is not writable, so imp will not replace itself. \
         The verified binary is at `{}`; run these as an administrator:\n  {}\n  {}",
        dir.display(),
        staged.display(),
        commands[0],
        commands[1]
    ))
}

/// The two commands that perform the replacement from outside, with `sudo`.
///
/// Order matters: the previous binary is preserved **before** the new one lands,
/// so an interrupted manual install still has something to roll back to.
pub fn manual_install_commands(
    target: &Path,
    staged: &Path,
    old_version: &str,
    dir: &Path,
) -> [String; 2] {
    let backup = dir.join(format!("imp.old-{old_version}"));
    [
        format!("sudo cp -p '{}' '{}'", target.display(), backup.display()),
        format!(
            "sudo install -m 0755 '{}' '{}'",
            staged.display(),
            target.display()
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use tempfile::TempDir;

    // ------------------------------------------------------------- the source

    /// A release channel backed by memory, so no test touches a socket.
    struct FixtureSource {
        release: ReleaseInfo,
        files: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl FixtureSource {
        fn new(release: ReleaseInfo) -> Self {
            Self {
                release,
                files: Mutex::new(BTreeMap::new()),
            }
        }

        fn with_file(self, url: &str, bytes: Vec<u8>) -> Self {
            self.files.lock().unwrap().insert(url.to_string(), bytes);
            self
        }
    }

    #[async_trait]
    impl UpdateSource for FixtureSource {
        async fn latest(&self) -> Result<ReleaseInfo> {
            Ok(self.release.clone())
        }

        async fn download(&self, url: &str) -> Result<Vec<u8>> {
            self.files
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| Error::Tool {
                    tool: "update".to_string(),
                    message: format!("no fixture file at `{url}`"),
                })
        }
    }

    // ------------------------------------------------------------- fixtures

    /// A shell script that answers `--version` the way a real imp does.
    fn fake_binary(version: &str, sha: &str) -> Vec<u8> {
        format!(
            "#!/bin/sh\necho \"imp {version} ({sha} 2026-10-07) [features: cron,mcp,update]\"\n"
        )
        .into_bytes()
    }

    const NEW_SHA: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    fn release(
        tag: &str,
        version: &str,
        asset: &str,
        checksums: &[u8],
        body: &[u8],
    ) -> ReleaseInfo {
        ReleaseInfo {
            tag: tag.to_string(),
            version: version.to_string(),
            notes: Some("notes for the release".to_string()),
            commit: Some(NEW_SHA.to_string()),
            assets: vec![
                Asset {
                    name: asset.to_string(),
                    url: "https://example.invalid/asset".to_string(),
                    size: Some(body.len() as u64),
                },
                Asset {
                    name: CHECKSUMS_ASSET.to_string(),
                    url: "https://example.invalid/checksums.txt".to_string(),
                    size: Some(checksums.len() as u64),
                },
            ],
        }
    }

    fn source_with(asset_name: &str, body: &[u8], checksums: &[u8]) -> FixtureSource {
        FixtureSource::new(release("v0.2.0", "0.2.0", asset_name, checksums, body))
            .with_file("https://example.invalid/asset", body.to_vec())
            .with_file("https://example.invalid/checksums.txt", checksums.to_vec())
    }

    fn manifest(asset_name: &str, bytes: &[u8]) -> Vec<u8> {
        format!("{}  {asset_name}\n", crate::sha256::sha256_hex(bytes)).into_bytes()
    }

    fn installed_binary(dir: &Path) -> PathBuf {
        let path = dir.join("imp");
        std::fs::write(
            &path,
            fake_binary("0.1.0", "aaaaaaaabbbbbbbbccccccccddddddddeeeeeeee"),
        )
        .unwrap();
        set_executable(&path).unwrap();
        path
    }

    fn name_for_platform() -> String {
        asset_for("imp", std::env::consts::OS, std::env::consts::ARCH).unwrap()
    }

    /// Any `.imp.new-*` file left in `dir`.
    fn staging_files(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with(".imp.new-"))
            .collect()
    }

    // ---------------------------------------------------------------- naming

    #[test]
    fn the_asset_table_is_exactly_the_six_published_names() {
        // The release pipeline publishes these six assets (M9.1). The names are
        // the contract between the two halves: `imp update` asks for
        // `<prefix>-<os>-<arch>` and a release that publishes anything else is a
        // 404 to it, not a fallback. Pinning the whole table here means the
        // workflow and this mapping cannot drift apart unnoticed.
        let table = [
            ("linux", "x86_64", "imp-linux-amd64"),
            ("linux", "aarch64", "imp-linux-arm64"),
            ("macos", "x86_64", "imp-darwin-amd64"),
            ("macos", "aarch64", "imp-darwin-arm64"),
            ("windows", "x86_64", "imp-windows-amd64"),
            ("windows", "aarch64", "imp-windows-arm64"),
        ];
        for (os, arch, expected) in table {
            assert_eq!(
                asset_for("imp", os, arch).unwrap(),
                expected,
                "the asset name for {os}/{arch} moved"
            );
        }

        // Fail closed on everything else — including the near misses: the OS is
        // `macos`, never `darwin`, and the arch is `x86_64`/`aarch64`, never the
        // asset spellings `amd64`/`arm64`.
        for os in ["darwin", "macosx", "win32", "freebsd", "sunos"] {
            assert!(asset_for("imp", os, "x86_64").is_err(), "os={os}");
        }
        for arch in ["amd64", "arm64", "x86", "arm", "riscv64"] {
            assert!(asset_for("imp", "linux", arch).is_err(), "arch={arch}");
        }

        // The prefix is passed through verbatim, so a renamed channel does not
        // silently keep the old names.
        assert_eq!(
            asset_for("other", "linux", "x86_64").unwrap(),
            "other-linux-amd64"
        );
    }

    #[test]
    fn only_https_and_loopback_http_are_permitted() {
        assert!(url_is_permitted("https://api.github.com"));
        assert!(url_is_permitted("http://127.0.0.1:8080"));
        assert!(url_is_permitted("http://localhost:1/repos"));
        assert!(url_is_permitted("http://[::1]:8080"));
        assert!(!url_is_permitted("http://example.com"));
        assert!(!url_is_permitted("ftp://example.com"));
        assert!(!url_is_permitted("api.github.com"));
        // A host that merely contains a loopback-looking prefix is not loopback.
        assert!(!url_is_permitted("http://127.0.0.1.example.com"));
    }

    // ------------------------------------------------------------ checksums

    #[test]
    fn checksums_are_parsed_and_malformed_lines_are_ignored() {
        let sums = parse_checksums(
            "# a comment\n\
             \n\
             2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae  imp-linux-amd64\n\
             nothex  other\n\
             2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae *imp-linux-arm64\n",
        );
        assert_eq!(sums.len(), 2);
        assert!(sums.contains_key("imp-linux-amd64"));
        assert!(sums.contains_key("imp-linux-arm64"));
        assert!(!sums.contains_key("other"));
    }

    // ------------------------------------------------------------- versions

    #[test]
    fn versions_compare_numerically() {
        use VersionOrder::*;
        assert_eq!(compare_versions("0.1.0", "0.2.0"), Older);
        assert_eq!(compare_versions("0.2.0", "0.2.0"), Same);
        assert_eq!(compare_versions("v0.10.0", "0.9.9"), Newer);
        assert_eq!(compare_versions("1.0.0", "1.0.0-rc1"), Newer);
        assert_eq!(compare_versions("1.0.0-rc1", "1.0.0"), Older);
        assert_eq!(compare_versions("0.2", "0.2.0"), Same);
    }

    #[test]
    fn the_version_and_sha_are_read_out_of_a_version_line() {
        let line = "imp 0.1.0 (a1b2c3d 2026-10-07) [features: cron,mcp,update]";
        assert_eq!(version_from_output(line).as_deref(), Some("0.1.0"));
        assert_eq!(sha_from_output(line).as_deref(), Some("a1b2c3d"));
        // No git available: degrade, never a false match.
        assert_eq!(
            sha_from_output("imp 0.1.0 (unknown unknown) [features: x]"),
            None
        );
        assert_eq!(version_from_output("imp"), None);
    }

    // ----------------------------------------------------------- the decision

    #[tokio::test]
    async fn being_up_to_date_is_not_an_update() {
        let source = source_with(&name_for_platform(), b"unused", b"");
        let updater = Updater::new(&source, "0.2.0", "imp");
        let release = updater.release().await.unwrap();
        assert_eq!(
            updater.decide(&release),
            Decision::UpToDate {
                installed: "0.2.0".to_string()
            }
        );
        assert!(!updater.decide(&release).update_available());
    }

    #[tokio::test]
    async fn a_newer_release_is_an_update() {
        let source = source_with(&name_for_platform(), b"unused", b"");
        let updater = Updater::new(&source, "0.1.0", "imp");
        let release = updater.release().await.unwrap();
        assert_eq!(
            updater.decide(&release),
            Decision::Update {
                from: "0.1.0".to_string(),
                to: "0.2.0".to_string()
            }
        );
        assert!(updater.decide(&release).update_available());
    }

    // ---------------------------------------------------- download + verify

    #[tokio::test]
    async fn a_verified_download_carries_the_bytes_and_the_commit() {
        let body = fake_binary("0.2.0", NEW_SHA);
        let name = name_for_platform();
        let source = source_with(&name, &body, &manifest(&name, &body));
        let updater = Updater::new(&source, "0.1.0", "imp");

        let release = updater.release().await.unwrap();
        let download = updater.download(&release).await.unwrap();
        assert_eq!(download.version, "0.2.0");
        assert_eq!(download.asset, name);
        assert_eq!(download.bytes, body);
        assert_eq!(download.expected_sha, NEW_SHA);
    }

    #[tokio::test]
    async fn a_bad_checksum_aborts_before_anything_is_written() {
        let body = fake_binary("0.2.0", NEW_SHA);
        let name = name_for_platform();
        let wrong = format!("{}  {name}\n", "0".repeat(64));
        let source = source_with(&name, &body, wrong.as_bytes());
        let updater = Updater::new(&source, "0.1.0", "imp");

        let dir = TempDir::new().unwrap();
        let target = installed_binary(dir.path());
        let before = std::fs::read(&target).unwrap();

        let release = updater.release().await.unwrap();
        let err = updater.download(&release).await.unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "was: {err}");

        // Nothing was written and no backup exists.
        assert_eq!(std::fs::read(&target).unwrap(), before);
        assert!(rollback_target(&target).is_none());
    }

    #[tokio::test]
    async fn a_release_without_an_asset_for_this_platform_is_refused() {
        let body = b"whatever";
        let source = source_with("imp-plan9-mips", body, b"");
        let updater = Updater::new(&source, "0.1.0", "imp");
        let release = updater.release().await.unwrap();
        let err = updater.download(&release).await.unwrap_err();
        assert!(err.to_string().contains("no asset"), "was: {err}");
    }

    #[tokio::test]
    async fn a_release_without_a_declared_commit_is_refused() {
        let body = fake_binary("0.2.0", NEW_SHA);
        let name = name_for_platform();
        let mut source = source_with(&name, &body, &manifest(&name, &body));
        source.release.commit = None;
        let updater = Updater::new(&source, "0.1.0", "imp");
        let release = updater.release().await.unwrap();
        let err = updater.download(&release).await.unwrap_err();
        assert!(err.to_string().contains("declares no commit"), "was: {err}");
    }

    // ------------------------------------------------------- the replace

    #[tokio::test]
    async fn installing_replaces_atomically_and_keeps_the_previous_binary() {
        let body = fake_binary("0.2.0", NEW_SHA);
        let name = name_for_platform();
        let source = source_with(&name, &body, &manifest(&name, &body));
        let updater = Updater::new(&source, "0.1.0", "imp");

        let dir = TempDir::new().unwrap();
        let target = installed_binary(dir.path());

        let release = updater.release().await.unwrap();
        let download = updater.download(&release).await.unwrap();
        let report = updater.install(&download, &target).unwrap();

        // The target now reports the new version...
        assert_eq!(reported_version(&target).as_deref(), Some("0.2.0"));
        // ...the previous one is beside it, named for its version...
        assert_eq!(report.backup, dir.path().join("imp.old-0.1.0"));
        assert_eq!(
            reported_version(&report.backup).as_deref(),
            Some("0.1.0"),
            "the backup still runs and reports the old version"
        );
        // ...and no staging file is left behind.
        assert!(
            staging_files(dir.path()).is_empty(),
            "no staging file may be left behind"
        );
    }

    #[tokio::test]
    async fn a_staged_binary_reporting_the_wrong_commit_is_not_installed() {
        // Verified by checksum, but built from a different commit than declared.
        let body = fake_binary("0.2.0", "1111111111111111111111111111111111111111");
        let name = name_for_platform();
        let source = source_with(&name, &body, &manifest(&name, &body));
        let updater = Updater::new(&source, "0.1.0", "imp");

        let dir = TempDir::new().unwrap();
        let target = installed_binary(dir.path());
        let before = std::fs::read(&target).unwrap();

        let release = updater.release().await.unwrap();
        let download = updater.download(&release).await.unwrap();
        let err = updater.install(&download, &target).unwrap_err();

        assert!(err.to_string().contains("declares"), "was: {err}");
        assert_eq!(std::fs::read(&target).unwrap(), before);
        assert!(
            staging_files(dir.path()).is_empty(),
            "no staging file may be left behind"
        );
        assert!(rollback_target(&target).is_none());
    }

    #[tokio::test]
    async fn a_read_only_directory_refuses_and_prints_the_manual_commands() {
        let body = fake_binary("0.2.0", NEW_SHA);
        let name = name_for_platform();
        let source = source_with(&name, &body, &manifest(&name, &body));
        let updater = Updater::new(&source, "0.1.0", "imp");

        let dir = TempDir::new().unwrap();
        let target = installed_binary(dir.path());
        let before = std::fs::read(&target).unwrap();

        // Take the write bit off the directory; the process is not root, so
        // creating the staging file must fail with `PermissionDenied`.
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o555);
        std::fs::set_permissions(dir.path(), perms).unwrap();

        // A privileged process ignores the mode bits; the case below is
        // meaningless there, so it is skipped rather than asserted falsely.
        if std::fs::write(dir.path().join("probe"), b"").is_ok() {
            let _ = std::fs::remove_file(dir.path().join("probe"));
            let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(dir.path(), perms).unwrap();
            eprintln!("skipping: running as root, write bits are not enforced");
            return;
        }

        let release = updater.release().await.unwrap();
        let download = updater.download(&release).await.unwrap();
        let result = updater.install(&download, &target);

        // Restore permissions so the TempDir can clean up.
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(dir.path(), perms).unwrap();

        let err = result.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("not writable"), "was: {message}");
        assert!(message.contains("sudo install -m 0755"), "was: {message}");
        assert!(message.contains("sudo cp -p"), "was: {message}");

        // The installed binary is untouched.
        assert_eq!(std::fs::read(&target).unwrap(), before);
    }

    #[tokio::test]
    async fn rollback_restores_the_previous_binary() {
        let body = fake_binary("0.2.0", NEW_SHA);
        let name = name_for_platform();
        let source = source_with(&name, &body, &manifest(&name, &body));
        let updater = Updater::new(&source, "0.1.0", "imp");

        let dir = TempDir::new().unwrap();
        let target = installed_binary(dir.path());

        let release = updater.release().await.unwrap();
        let download = updater.download(&release).await.unwrap();
        updater.install(&download, &target).unwrap();
        assert_eq!(reported_version(&target).as_deref(), Some("0.2.0"));

        let report = rollback(&target).unwrap();
        assert_eq!(reported_version(&target).as_deref(), Some("0.1.0"));
        assert_eq!(report.version, "0.1.0");
        // The binary we rolled away from is kept as well.
        assert_eq!(reported_version(&report.backup).as_deref(), Some("0.2.0"));
    }

    #[test]
    fn rollback_without_a_backup_is_refused() {
        let dir = TempDir::new().unwrap();
        let target = installed_binary(dir.path());
        let err = rollback(&target).unwrap_err();
        assert!(
            err.to_string().contains("no previous version"),
            "was: {err}"
        );
    }
}
