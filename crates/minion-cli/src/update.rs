//! `minion update`: check a release channel, and replace the installed binary
//! with a verified newer one (§5.12, §9).
//!
//! This file is the operator-facing half: it reads the `[update]` section,
//! prints, prompts, and maps outcomes onto exit codes. The rules — what a
//! release is, how a checksum is verified, how the replacement is done atomically
//! — live in `minion_core::update`, and the socket lives in `minion-update`.
//!
//! Exit codes follow §5.12, with `1` reserved for `--check` finding an update:
//! `0` success, `1` update available, `2` usage, `3` network or auth, `4`
//! refused or cancelled, `5` internal.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use minion_core::config::Config;
use minion_core::error::{Error, Result};
use minion_core::update::{self, Decision, ReleaseInfo, Updater};
use minion_update::HttpUpdateSource;

use crate::cli::{Cli, UpdateArgs};

/// Overrides the binary to replace, which defaults to the running one.
///
/// `current_exe()` is the right default, but not the only useful target: a
/// developer running `target/debug/minion` may want to update the installed copy
/// at `/usr/local/bin/minion`, and a test needs to point at a fixture binary it
/// owns. Setting this is not a privilege — whoever can set the environment
/// already runs the code.
const TARGET_ENV: &str = "MINION_UPDATE_BINARY";

/// Run `minion update`.
pub async fn run(cli: &Cli, config: &Config, args: UpdateArgs) -> Result<ExitCode> {
    let target = target_path()?;
    if args.rollback {
        return Ok(rollback(cli, &target));
    }

    let source = HttpUpdateSource::new(&config.update)?;
    let installed = installed_version(&target);
    let asset = update::platform_asset(&config.update.asset_prefix);
    let updater = Updater::new(
        &source,
        installed.clone(),
        config.update.asset_prefix.clone(),
    )
    .force(args.force);

    let release = match updater.release().await {
        Ok(release) => release,
        Err(err) => return Ok(fail(&err)),
    };
    let decision = updater.decide(&release);

    if args.check {
        return Ok(report_check(
            cli, &installed, &release, &decision, &target, &asset,
        ));
    }

    match &decision {
        Decision::UpToDate { .. } if !args.force => {
            announce_up_to_date(cli, &installed, &release, &target);
            return Ok(ExitCode::SUCCESS);
        }
        Decision::InstalledIsNewer { installed, latest } => {
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "type": "update",
                        "action": "none",
                        "installed": installed,
                        "latest": latest,
                        "target": target.display().to_string(),
                    })
                );
            } else {
                println!(
                    "installed {installed} is newer than the latest release ({latest}) — nothing to do"
                );
            }
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }

    // Consent. A prompt needs a terminal; without one, `--yes` is the only way
    // to say yes — the same fail-closed rule the rest of the repo uses.
    if !cli.yes {
        if !std::io::stdin().is_terminal() {
            eprintln!(
                "minion: stdin is not a terminal; pass --yes to install {} without a prompt",
                release.version
            );
            return Ok(ExitCode::from(4));
        }
        show_notes(cli, &release);
        if !confirm(&format!(
            "Install minion {} over {installed} at {}?",
            release.version,
            target.display()
        )) {
            eprintln!("minion: update cancelled");
            return Ok(ExitCode::from(4));
        }
    } else {
        show_notes(cli, &release);
    }

    let download = match updater.download(&release).await {
        Ok(download) => download,
        Err(err) => return Ok(fail(&err)),
    };
    let report = match updater.install(&download, &target) {
        Ok(report) => report,
        Err(err) => return Ok(fail(&err)),
    };

    if cli.json {
        println!(
            "{}",
            serde_json::json!({
                "type": "update",
                "action": "installed",
                "installed": installed,
                "version": report.version,
                "asset": download.asset,
                "target": report.target.display().to_string(),
                "backup": report.backup.display().to_string(),
            })
        );
    } else {
        println!(
            "installed minion {} at {} (previous kept as {})",
            report.version,
            report.target.display(),
            report.backup.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `--check`: report and write nothing.
fn report_check(
    cli: &Cli,
    installed: &str,
    release: &ReleaseInfo,
    decision: &Decision,
    target: &Path,
    asset: &Result<String>,
) -> ExitCode {
    let available = decision.update_available();
    let asset_json = match asset {
        Ok(name) => serde_json::Value::String(name.clone()),
        Err(_) => serde_json::Value::Null,
    };
    let asset_text = match asset {
        Ok(name) => name.clone(),
        Err(err) => format!("(unavailable: {err})"),
    };

    if cli.json {
        println!(
            "{}",
            serde_json::json!({
                "type": "update_check",
                "installed": installed,
                "latest": release.version,
                "tag": release.tag,
                "update_available": available,
                "asset": asset_json,
                "commit": release.commit,
                "target": target.display().to_string(),
            })
        );
    } else {
        println!("installed  {installed}  ({})", target.display());
        match decision {
            Decision::Update { to, .. } => {
                println!("latest     {to}  ({})  — update available", release.tag);
                println!("asset      {asset_text}");
            }
            Decision::UpToDate { .. } => {
                println!(
                    "latest     {}  ({})  — up to date",
                    release.version, release.tag
                );
            }
            Decision::InstalledIsNewer { latest, .. } => {
                println!(
                    "latest     {latest}  ({})  — installed is newer",
                    release.tag
                );
            }
        }
        show_notes(cli, release);
    }

    if available {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// `--rollback`: restore the most recent `minion.old-*` beside the target.
fn rollback(cli: &Cli, target: &Path) -> ExitCode {
    match update::rollback(target) {
        Ok(report) => {
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "type": "update",
                        "action": "rolled_back",
                        "version": report.version,
                        "target": report.target.display().to_string(),
                        "backup": report.backup.display().to_string(),
                    })
                );
            } else {
                println!(
                    "rolled back to minion {} at {} (the replaced binary is {})",
                    report.version,
                    report.target.display(),
                    report.backup.display()
                );
            }
            ExitCode::SUCCESS
        }
        Err(err) => fail(&err),
    }
}

/// Release notes, when the release carries any.
fn show_notes(cli: &Cli, release: &ReleaseInfo) {
    if cli.json {
        return;
    }
    if let Some(notes) = &release.notes {
        println!("notes for {}:", release.tag);
        for line in notes.lines() {
            println!("  {line}");
        }
    }
}

fn announce_up_to_date(cli: &Cli, installed: &str, release: &ReleaseInfo, target: &Path) {
    if cli.json {
        println!(
            "{}",
            serde_json::json!({
                "type": "update",
                "action": "none",
                "installed": installed,
                "latest": release.version,
                "target": target.display().to_string(),
            })
        );
    } else {
        println!("minion {installed} is up to date (latest {})", release.tag);
    }
}

/// One prompt, fail-closed: anything but `y`/`yes` is "no".
fn confirm(prompt: &str) -> bool {
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Report an error and map it onto the exit codes of §5.12.
fn fail(err: &Error) -> ExitCode {
    eprintln!("minion: {err}");
    ExitCode::from(crate::exit_code(err))
}

/// The binary to replace: the running one, or `$MINION_UPDATE_BINARY`.
fn target_path() -> Result<PathBuf> {
    if let Ok(raw) = std::env::var(TARGET_ENV) {
        let raw = raw.trim();
        if !raw.is_empty() {
            let path = PathBuf::from(raw);
            return Ok(path.canonicalize().unwrap_or(path));
        }
    }
    let exe = std::env::current_exe()?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// The installed version: what the target binary reports, or this build's.
///
/// Reading it from the binary rather than from `CARGO_PKG_VERSION` keeps the
/// number honest when `MINION_UPDATE_BINARY` points at another installation; the
/// running build is the fallback when the target cannot be executed.
fn installed_version(target: &Path) -> String {
    if let Ok(output) = std::process::Command::new(target).arg("--version").output()
        && output.status.success()
        && let Some(version) = update::version_from_output(&String::from_utf8_lossy(&output.stdout))
    {
        return version;
    }
    env!("CARGO_PKG_VERSION").to_string()
}
