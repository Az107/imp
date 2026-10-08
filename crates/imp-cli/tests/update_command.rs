//! `imp update` end to end, against a loopback release fixture (§9).
//!
//! No test here reaches the network: a `std::net::TcpListener` on `127.0.0.1`
//! serves a release manifest, a `checksums.txt` and a fake binary, and the real
//! `imp` binary is driven as a child process. The fake binary is a shell
//! script that answers `--version` the way a real one does, so the checksum and
//! the commit check have something to verify.
//!
//! The binary under test is `CARGO_BIN_EXE_imp`; the target it replaces is a
//! script in a temp directory, pointed at with `IMP_UPDATE_BINARY` so the
//! test never touches the binary it is running from, and `XDG_CONFIG_HOME` is
//! redirected so the developer's own config is never read.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;

use tempfile::TempDir;

/// A loopback HTTP server that answers by path.
struct Fixture {
    port: u16,
    _thread: thread::JoinHandle<()>,
}

impl Fixture {
    /// Bind first, then build the responses with the real port, then serve.
    fn start<F>(build: F) -> Self
    where
        F: FnOnce(u16) -> BTreeMap<String, (u16, Vec<u8>)>,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        let port = listener.local_addr().expect("addr").port();
        let responses = build(port);
        let thread = thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let _ = handle(stream, &responses);
            }
        });
        Self {
            port,
            _thread: thread,
        }
    }

    fn api_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

fn handle(
    mut stream: TcpStream,
    responses: &BTreeMap<String, (u16, Vec<u8>)>,
) -> std::io::Result<()> {
    // Read the request head; every fixture request is a bodyless GET.
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    let mut path = String::new();
    if reader.read_line(&mut line)? > 0 {
        path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    }
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let (status, body) = responses
        .get(&path)
        .cloned()
        .unwrap_or_else(|| (404, b"not found".to_vec()));
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/octet-stream\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

/// A `--version`-answering script, the shape a real binary has.
fn fake_binary(version: &str, sha: &str) -> Vec<u8> {
    format!(
        "#!/bin/sh\necho \"imp {version} ({sha} 2026-10-07) [features: cron,guard,mcp,update]\"\n"
    )
    .into_bytes()
}

const OLD_SHA: &str = "1111111111111111111111111111111111111111";
const NEW_SHA: &str = "2222222222222222222222222222222222222222";

struct Setup {
    dir: TempDir,
    _fixture: Fixture,
    config: PathBuf,
    target: PathBuf,
    xdg: PathBuf,
    asset: String,
}

impl Setup {
    /// A temp directory with a config, an installed fake binary, and a release
    /// fixture serving `new_body` under `release_tag`. `corrupt` publishes a
    /// wrong checksum.
    fn new(new_body: &[u8], release_tag: &str, target_version: &str, corrupt: bool) -> Self {
        let dir = TempDir::new().expect("tempdir");
        let xdg = dir.path().join("xdg");
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::create_dir_all(dir.path().join("cwd")).unwrap();

        let target = dir.path().join("imp");
        std::fs::write(&target, fake_binary(target_version, OLD_SHA)).unwrap();
        make_executable(&target);

        let asset = asset_name();
        let checksum = if corrupt {
            "0".repeat(64)
        } else {
            imp_core::sha256::sha256_hex(new_body)
        };
        let body = new_body.to_vec();
        let body_len = body.len();
        let asset_for_map = asset.clone();
        let tag = release_tag.to_string();

        let fixture = Fixture::start(move |port| {
            let api = format!("http://127.0.0.1:{port}");
            let release = format!(
                r#"{{"tag_name":"{tag}","body":"A newer imp.","target_commitish":"{NEW_SHA}",
                    "assets":[
                      {{"name":"{asset_for_map}","browser_download_url":"{api}/assets/{asset_for_map}","size":{body_len}}},
                      {{"name":"checksums.txt","browser_download_url":"{api}/assets/checksums.txt","size":1}}
                    ]}}"#
            );
            let mut responses = BTreeMap::new();
            responses.insert(
                "/repos/fixture/imp/releases/latest".to_string(),
                (200, release.into_bytes()),
            );
            responses.insert(format!("/assets/{asset_for_map}"), (200, body.clone()));
            responses.insert(
                "/assets/checksums.txt".to_string(),
                (200, format!("{checksum}  {asset_for_map}\n").into_bytes()),
            );
            responses
        });

        let config = dir.path().join("imp.toml");
        std::fs::write(
            &config,
            format!(
                "[provider]\nbase_url = \"http://127.0.0.1:1/v1\"\napi_key_env = \"\"\napi_key_file = \"\"\nmodel = \"stub\"\n\
                 [cron]\nenabled = false\n\
                 [update]\napi_url = \"{}\"\nrepo = \"fixture/imp\"\nasset_prefix = \"imp\"\n",
                fixture.api_url()
            ),
        )
        .unwrap();

        Self {
            dir,
            _fixture: fixture,
            config,
            target,
            xdg,
            asset,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_stdin(args, Stdio::null())
    }

    fn run_with_stdin(&self, args: &[&str], stdin: Stdio) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_imp"));
        command
            .arg("--config")
            .arg(&self.config)
            .arg("--cwd")
            .arg(self.dir.path().join("cwd"))
            .args(args)
            .env("IMP_UPDATE_BINARY", &self.target)
            .env("XDG_CONFIG_HOME", &self.xdg)
            .env("XDG_STATE_HOME", &self.xdg)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.output().expect("run imp")
    }

    fn target_version(&self) -> String {
        version_of(&self.target)
    }
}

fn asset_name() -> String {
    imp_core::update::platform_asset("imp").expect("a supported platform")
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Run `path --version` and pull the version token out.
fn version_of(path: &Path) -> String {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .expect("run target");
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    imp_core::update::version_from_output(&text).unwrap_or_default()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Any `.imp.new-*` staging file left in `dir`.
fn staging_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with(".imp.new-"))
        .collect()
}

// ---------------------------------------------------------------- the tests

#[test]
fn version_reports_the_commit_the_date_and_the_features() {
    let output = Command::new(env!("CARGO_BIN_EXE_imp"))
        .arg("--version")
        .output()
        .expect("run");
    let line = stdout(&output);
    let line = line.trim();
    assert!(
        line.starts_with(&format!("imp {} (", env!("CARGO_PKG_VERSION"))),
        "was: {line}"
    );
    assert!(
        line.contains("features: cron,guard,mcp,update"),
        "was: {line}"
    );
    let inner = line.split_once('(').unwrap().1.split_once(')').unwrap().0;
    let mut parts = inner.split_whitespace();
    let sha = parts.next().unwrap();
    let date = parts.next().unwrap();
    assert!(
        sha == "unknown" || (sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit())),
        "sha was `{sha}`"
    );
    assert!(
        date == "unknown" || date.starts_with("20"),
        "date was `{date}`"
    );
}

#[test]
fn check_reports_an_available_update_and_writes_nothing() {
    let body = fake_binary("0.2.0", NEW_SHA);
    let setup = Setup::new(&body, "v0.2.0", "0.1.0", false);
    let before = std::fs::read(&setup.target).unwrap();

    let output = setup.run(&["update", "--check"]);
    let out = stdout(&output);

    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(out.contains("installed  0.1.0"), "was: {out}");
    assert!(out.contains("0.2.0"), "was: {out}");
    assert!(out.contains("update available"), "was: {out}");
    assert!(
        out.contains(&setup.asset),
        "the platform asset must be named: {out}"
    );

    // `--check` is read-only: the binary is untouched and no backup appeared.
    assert_eq!(std::fs::read(&setup.target).unwrap(), before);
    assert!(
        !setup.dir.path().join("imp.old-0.1.0").exists(),
        "check must not write a backup"
    );
}

#[test]
fn check_json_is_machine_readable() {
    let body = fake_binary("0.2.0", NEW_SHA);
    let setup = Setup::new(&body, "v0.2.0", "0.1.0", false);

    let output = setup.run(&["update", "--check", "--json"]);
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));

    let out = stdout(&output);
    let value: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON object");
    assert_eq!(value["type"], "update_check");
    assert_eq!(value["installed"], "0.1.0");
    assert_eq!(value["latest"], "0.2.0");
    assert_eq!(value["update_available"], true);
    assert_eq!(value["commit"], NEW_SHA);
    assert_eq!(value["asset"], setup.asset);
}

#[test]
fn check_says_up_to_date_when_the_versions_match() {
    let body = fake_binary("0.1.0", NEW_SHA);
    let setup = Setup::new(&body, "v0.1.0", "0.1.0", false);

    let output = setup.run(&["update", "--check"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(
        stdout(&output).contains("up to date"),
        "was: {}",
        stdout(&output)
    );
}

#[test]
fn installing_replaces_the_binary_and_keeps_the_previous_one() {
    let body = fake_binary("0.2.0", NEW_SHA);
    let setup = Setup::new(&body, "v0.2.0", "0.1.0", false);
    assert_eq!(setup.target_version(), "0.1.0");

    let output = setup.run(&["update", "--yes"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));

    assert_eq!(
        setup.target_version(),
        "0.2.0",
        "the target now reports the new version"
    );
    let backup = setup.dir.path().join("imp.old-0.1.0");
    assert!(backup.exists(), "the previous binary is kept");
    assert_eq!(version_of(&backup), "0.1.0");
    assert!(
        staging_files(setup.dir.path()).is_empty(),
        "no staging file is left behind"
    );
}

#[test]
fn a_bad_checksum_refuses_and_leaves_the_binary_alone() {
    let body = fake_binary("0.2.0", NEW_SHA);
    let setup = Setup::new(&body, "v0.2.0", "0.1.0", true);
    let before = std::fs::read(&setup.target).unwrap();

    let output = setup.run(&["update", "--yes"]);
    let err = stderr(&output);

    assert_eq!(output.status.code(), Some(4), "stderr: {err}");
    assert!(err.contains("checksum mismatch"), "was: {err}");
    assert_eq!(std::fs::read(&setup.target).unwrap(), before);
    assert!(!setup.dir.path().join("imp.old-0.1.0").exists());
}

#[test]
fn a_pipe_without_yes_refuses_rather_than_prompting() {
    let body = fake_binary("0.2.0", NEW_SHA);
    let setup = Setup::new(&body, "v0.2.0", "0.1.0", false);
    let before = std::fs::read(&setup.target).unwrap();

    // A piped stdin is not a terminal, so the prompt cannot be shown.
    let output = setup.run_with_stdin(&["update"], Stdio::piped());

    assert_eq!(output.status.code(), Some(4), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("not a terminal"),
        "was: {}",
        stderr(&output)
    );
    assert_eq!(std::fs::read(&setup.target).unwrap(), before);
}

#[test]
fn a_read_only_target_directory_prints_the_sudo_commands() {
    let body = fake_binary("0.2.0", NEW_SHA);
    let setup = Setup::new(&body, "v0.2.0", "0.1.0", false);
    let before = std::fs::read(&setup.target).unwrap();

    // A directory the process cannot write to stands in for /usr/local/bin.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(setup.dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

    // A privileged process ignores the mode bits; skip rather than assert falsely.
    if std::fs::write(setup.dir.path().join("probe"), b"").is_ok() {
        let _ = std::fs::remove_file(setup.dir.path().join("probe"));
        std::fs::set_permissions(setup.dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipping: running as root, write bits are not enforced");
        return;
    }

    let output = setup.run(&["update", "--yes"]);
    let err = stderr(&output);

    // Restore so the TempDir can be removed.
    std::fs::set_permissions(setup.dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(output.status.code(), Some(4), "stderr: {err}");
    assert!(err.contains("not writable"), "was: {err}");
    assert!(err.contains("sudo cp -p"), "was: {err}");
    assert!(err.contains("sudo install -m 0755"), "was: {err}");
    // Nothing was replaced.
    assert_eq!(std::fs::read(&setup.target).unwrap(), before);
}

#[test]
fn rollback_restores_the_previous_version() {
    let body = fake_binary("0.2.0", NEW_SHA);
    let setup = Setup::new(&body, "v0.2.0", "0.1.0", false);

    assert_eq!(
        setup.run(&["update", "--yes"]).status.code(),
        Some(0),
        "install first"
    );
    assert_eq!(setup.target_version(), "0.2.0");

    let output = setup.run(&["update", "--rollback", "--yes"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(setup.target_version(), "0.1.0");
    assert!(
        stdout(&output).contains("rolled back"),
        "was: {}",
        stdout(&output)
    );
}

#[test]
fn rollback_without_a_backup_is_refused() {
    let body = fake_binary("0.2.0", NEW_SHA);
    let setup = Setup::new(&body, "v0.2.0", "0.1.0", false);

    let output = setup.run(&["update", "--rollback"]);
    assert_eq!(output.status.code(), Some(4), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("no previous version"),
        "was: {}",
        stderr(&output)
    );
}
