//! `imp config show|path` (§5.12) — read-only views of the configuration.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use imp_core::config::{Config, default_credentials_path, user_config_path};
use imp_core::error::Result;

use crate::cli::{Cli, ConfigAction, ConfigArgs};
use crate::logging::{Secrets, redact};

/// Run a `imp config` subcommand. `config init` is handled in `main`, before
/// configuration is loaded, exactly like `imp init`.
pub fn run(cli: &Cli, config: &Config, cwd: &Path, args: ConfigArgs) -> Result<ExitCode> {
    match args.action {
        ConfigAction::Show => show(cli, config),
        ConfigAction::Path => path(cli, config, cwd),
        ConfigAction::Init(_) => unreachable!("handled in main"),
    }
}

/// Print the effective configuration, with any credential value masked.
fn show(cli: &Cli, config: &Config) -> Result<ExitCode> {
    let secrets = Secrets::from_config(config).into_shared();

    if cli.json {
        let body = serde_json::json!({ "type": "config", "config": config });
        let text = serde_json::to_string_pretty(&body)
            .map_err(|err| imp_core::error::Error::Config(err.to_string()))?;
        println!("{}", redact(&text, &secrets));
        return Ok(ExitCode::SUCCESS);
    }

    let text = toml::to_string_pretty(config)
        .map_err(|err| imp_core::error::Error::Config(err.to_string()))?;
    print!("{}", redact(&text, &secrets));
    Ok(ExitCode::SUCCESS)
}

/// Print the paths imp reads and writes, and how they are protected.
fn path(cli: &Cli, config: &Config, cwd: &Path) -> Result<ExitCode> {
    let project = cwd.join("imp.toml");
    let database = cli.db.clone().unwrap_or_else(|| config.database_path());
    let credentials = default_credentials_path();

    let mut entries: Vec<(&str, PathBuf)> = Vec::new();
    if let Some(user) = user_config_path() {
        entries.push(("user config", user));
    }
    entries.push(("project config", project));
    if let Some(explicit) = &cli.config {
        entries.push(("--config", explicit.clone()));
    }
    entries.push(("database", database));
    if let Some(credentials) = &credentials {
        entries.push(("credentials", credentials.clone()));
    }

    if cli.json {
        let items: Vec<serde_json::Value> = entries
            .iter()
            .map(|(name, path)| {
                serde_json::json!({
                    "type": "path",
                    "name": name,
                    "path": path,
                    "exists": path.exists(),
                    "mode": mode_of(path).map(|mode| format!("{mode:04o}")),
                })
            })
            .collect();
        let body = serde_json::json!({ "type": "paths", "paths": items });
        println!("{body}");
        return Ok(ExitCode::SUCCESS);
    }

    for (name, path) in &entries {
        let exists = path.exists();
        let note = if exists {
            match mode_of(path) {
                Some(mode) => format!("  ({mode:04o})"),
                None => String::new(),
            }
        } else {
            "  (missing)".to_string()
        };
        println!("{name:<16} {}{note}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// The POSIX permission bits of `path`, if it exists and the platform has them.
#[cfg(unix)]
pub fn mode_of(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o777)
}

/// No POSIX modes off Unix.
#[cfg(not(unix))]
pub fn mode_of(_path: &Path) -> Option<u32> {
    None
}
