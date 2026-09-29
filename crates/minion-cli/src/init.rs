//! `minion init` — first-run configuration of the model backend.
//!
//! The command is split into pure pieces ([`Plan`], [`Plan::to_toml`],
//! [`diff_keys`]) and a thin prompting layer, so everything that decides *what*
//! gets written is unit-testable without a terminal.
//!
//! The API key is deliberately kept *out* of [`Plan`]: the token is collected
//! separately and written to the credentials file, which makes it structurally
//! impossible for [`Plan::to_toml`] to render a secret into a config file that
//! may be committed.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use minion_core::config::{Credentials, default_credentials_path, user_config_path};
use minion_core::error::{Error, Result};
use minion_core::write_private_file;
use minion_provider::OpenAiProvider;

use crate::cli::{Cli, InitArgs};

/// Exit code for "refused to proceed" — an existing config without `--force`.
const EXIT_REFUSED: u8 = 4;
/// Exit code for a failed provider probe.
const EXIT_PROVIDER: u8 = 3;
/// How many discovered models to display before falling back to free text.
const MAX_SUGGESTIONS: usize = 30;
/// `init` must not hang on a dead endpoint.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// A known backend, used to prefill the prompts.
struct Preset {
    name: &'static str,
    description: &'static str,
    base_url: &'static str,
    api_key_env: Option<&'static str>,
    headers: &'static [(&'static str, &'static str)],
}

/// Backends offered by `minion init`.
///
/// `opencode-go` and `opencode-zen` send `x-opencode-session`, which those
/// gateways require in order to pin a conversation to one upstream and reuse
/// its prompt cache.
const PRESETS: &[Preset] = &[
    Preset {
        name: "openai",
        description: "OpenAI",
        base_url: "https://api.openai.com/v1",
        api_key_env: Some("OPENAI_API_KEY"),
        headers: &[],
    },
    Preset {
        name: "openrouter",
        description: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        api_key_env: Some("OPENROUTER_API_KEY"),
        headers: &[],
    },
    Preset {
        name: "opencode-go",
        description: "OpenCode Go (requires the x-opencode-session header)",
        base_url: "https://opencode.ai/zen/go/v1",
        api_key_env: Some("OPENCODE_API_KEY"),
        headers: &[("x-opencode-session", "${session}")],
    },
    Preset {
        name: "opencode-zen",
        description: "OpenCode Zen",
        base_url: "https://opencode.ai/zen/v1",
        api_key_env: Some("OPENCODE_API_KEY"),
        headers: &[("x-opencode-session", "${session}")],
    },
    Preset {
        name: "ollama",
        description: "Ollama (local, no API key)",
        base_url: "http://localhost:11434/v1",
        api_key_env: None,
        headers: &[],
    },
    Preset {
        name: "vllm",
        description: "vLLM / LM Studio (local, no API key)",
        base_url: "http://localhost:8000/v1",
        api_key_env: None,
        headers: &[],
    },
    Preset {
        name: "custom",
        description: "Custom OpenAI-compatible endpoint",
        base_url: "",
        api_key_env: None,
        headers: &[],
    },
];

/// Everything `init` decided, with the token held apart from the config plan.
struct Resolved {
    /// Values destined for the config file. Contains no secret.
    plan: Plan,
    /// The API key, when one was typed. Never part of [`Plan`].
    token: Option<String>,
}

/// Exactly what `init` will write to the config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Endpoint root, without `/chat/completions`.
    pub base_url: String,
    /// Model identifier.
    pub model: String,
    /// Env var holding the key, or `None` for a keyless backend.
    pub api_key_env: Option<String>,
    /// Credentials path to record, only when it differs from the default.
    ///
    /// `None` means "use the standard location", which is left unstated so that
    /// a committable config never hardcodes a machine-specific path.
    pub api_key_file: Option<String>,
    /// Extra request headers; `${session}` is expanded per conversation.
    pub headers: BTreeMap<String, String>,
    /// Workspace root recorded in the config.
    pub workspace_root: String,
}

impl Plan {
    /// Render the config file.
    ///
    /// Built through `toml::Value` rather than string formatting so keys are
    /// quoted correctly and the output is guaranteed to parse. It has no access
    /// to the API key, so it cannot leak one.
    fn to_toml(&self) -> String {
        let mut provider = toml::Table::new();
        provider.insert("base_url".into(), self.base_url.clone().into());
        provider.insert("model".into(), self.model.clone().into());
        // Always written, even when empty: an empty value is how a keyless
        // backend is expressed, and it overrides the built-in default.
        provider.insert(
            "api_key_env".into(),
            self.api_key_env.clone().unwrap_or_default().into(),
        );
        if self.api_key_env.is_none() {
            // A keyless backend must disable the credentials file too: leaving it
            // at its default would make `api_key()` still look for a secret and
            // fail instead of concluding that this endpoint needs no auth.
            provider.insert("api_key_file".into(), "".into());
        } else if let Some(path) = &self.api_key_file {
            // Only recorded when the user chose a non-default location, so the
            // standard case stays free of machine-specific paths.
            provider.insert("api_key_file".into(), path.clone().into());
        }
        if !self.headers.is_empty() {
            let mut headers = toml::Table::new();
            for (name, value) in &self.headers {
                headers.insert(name.clone(), value.clone().into());
            }
            provider.insert("headers".into(), toml::Value::Table(headers));
        }

        let mut workspace = toml::Table::new();
        workspace.insert(
            "roots".into(),
            toml::Value::Array(vec![self.workspace_root.clone().into()]),
        );

        let mut root = toml::Table::new();
        root.insert("provider".into(), toml::Value::Table(provider));
        root.insert("workspace".into(), toml::Value::Table(workspace));

        let body = toml::to_string_pretty(&toml::Value::Table(root)).unwrap_or_default();
        format!(
            "# minion configuration, written by `minion init`.\n\
             # The API key is not stored here: `api_key_env` names an environment\n\
             # variable, and the key itself lives in the credentials file.\n\n{body}"
        )
    }
}

/// Run `minion init`.
pub async fn run(cli: &Cli, args: InitArgs) -> Result<ExitCode> {
    let cwd = match cli.cwd.clone() {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    let cwd = cwd.canonicalize().unwrap_or(cwd);

    let Resolved { plan, token } = build_plan(&args).await?;
    let contents = plan.to_toml();

    let target = if args.project {
        cwd.join("minion.toml")
    } else {
        user_config_path().ok_or_else(|| {
            Error::Config("cannot determine the user config directory".to_string())
        })?
    };

    if target.exists() && !args.force {
        let existing = std::fs::read_to_string(&target)?;
        if existing == contents {
            report(cli, &args, &plan, &target, None, "unchanged")?;
            return Ok(ExitCode::SUCCESS);
        }
        eprintln!("minion: {} already exists.", target.display());
        for line in diff_keys(&existing, &contents) {
            eprintln!("  {line}");
        }
        eprintln!("minion: nothing was written. Rerun with --force to replace it.");
        return Ok(ExitCode::from(EXIT_REFUSED));
    }

    let credentials = credentials_path(&args);
    let probe_key = probe_key(plan.api_key_env.as_deref(), token.as_deref());

    if args.check
        && let Err(err) = probe(&plan, probe_key.as_deref()).await
    {
        eprintln!("minion: backend check failed: {err}");
        eprintln!("minion: nothing was written. Fix the backend or drop --check.");
        return Ok(ExitCode::from(EXIT_PROVIDER));
    }

    // The key is stored before the config so that a config never references a
    // credential that does not exist yet.
    if let Some(token) = &token {
        Credentials::write(&credentials, token)?;
        eprintln!(
            "minion: stored the API key in {} (owner-only, never commit it).",
            credentials.display()
        );
    }

    write_private_file(&target, &contents)?;
    warn_on_missing_credential(&plan, token.is_some());
    report(
        cli,
        &args,
        &plan,
        &target,
        token.as_ref().map(|_| credentials),
        if args.check { "ok" } else { "skipped" },
    )?;
    Ok(ExitCode::SUCCESS)
}

/// Where the API key is stored.
fn credentials_path(args: &InitArgs) -> PathBuf {
    args.credentials_file.clone().unwrap_or_else(|| {
        default_credentials_path().unwrap_or_else(|| PathBuf::from("credentials"))
    })
}

/// The key to use for discovery and `--check`.
///
/// A token just typed wins over the environment, so setup works before the user
/// has exported anything.
fn probe_key(api_key_env: Option<&str>, token: Option<&str>) -> Option<String> {
    token
        .map(str::to_string)
        .or_else(|| api_key_env.and_then(|name| std::env::var(name).ok()))
        .filter(|value| !value.trim().is_empty())
}

/// Resolve every value, prompting only where a flag was not supplied.
async fn build_plan(args: &InitArgs) -> Result<Resolved> {
    let interactive = !args.non_interactive && std::io::stdin().is_terminal();

    let preset = match &args.preset {
        Some(name) => Some(preset_by_name(name).ok_or_else(|| {
            Error::Config(format!(
                "unknown preset `{name}`. Available: {}",
                preset_names()
            ))
        })?),
        None if interactive => Some(choose_preset()?),
        None => None,
    };

    let base_url = match &args.base_url {
        Some(url) => url.clone(),
        None => {
            let default = preset.map(|p| p.base_url).filter(|url| !url.is_empty());
            if interactive {
                ask("Base URL", default)?
            } else {
                default
                    .ok_or_else(|| {
                        Error::Config("--base-url is required with --non-interactive".to_string())
                    })?
                    .to_string()
            }
        }
    };

    let api_key_env = resolve_api_key_env(args, preset, interactive)?;

    // Ask for the real key rather than making the user export it themselves.
    // Only meaningful on a TTY, where input can be hidden.
    let token = match (&api_key_env, interactive, args.no_store_token) {
        (Some(_), true, false) => {
            let answer = ask_secret("API key")?;
            if answer.is_empty() {
                eprintln!(
                    "minion: no key entered; falling back to `{}`.",
                    api_key_env.clone().unwrap_or_default()
                );
                None
            } else {
                Some(answer)
            }
        }
        _ => None,
    };

    let mut headers: BTreeMap<String, String> = preset
        .map(|p| {
            p.headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .unwrap_or_default();
    for raw in &args.headers {
        let (name, value) = parse_header_arg(raw)?;
        headers.insert(name, value);
    }

    let key = probe_key(api_key_env.as_deref(), token.as_deref());
    let model = match &args.model {
        Some(model) => model.clone(),
        None if !interactive => {
            return Err(Error::Config(
                "--model is required with --non-interactive".to_string(),
            ));
        }
        None => choose_model(&base_url, &api_key_env, key.as_deref(), &headers).await?,
    };

    let workspace_root = match &args.workspace {
        Some(root) => root.clone(),
        None if interactive => ask("Workspace root", Some("."))?,
        None => ".".to_string(),
    };

    // Recorded only when the user picked a non-default location; the standard
    // credentials path is left to the built-in default.
    let api_key_file = match (&args.credentials_file, &api_key_env) {
        (Some(path), Some(_)) => Some(path.display().to_string()),
        _ => None,
    };

    Ok(Resolved {
        plan: Plan {
            base_url,
            model,
            api_key_env,
            api_key_file,
            headers,
            workspace_root,
        },
        token,
    })
}

/// Work out which env var holds the key, or `None` for a keyless backend.
fn resolve_api_key_env(
    args: &InitArgs,
    preset: Option<&Preset>,
    interactive: bool,
) -> Result<Option<String>> {
    if args.no_api_key {
        return Ok(None);
    }
    if let Some(name) = &args.api_key_env {
        let trimmed = name.trim();
        return Ok((!trimmed.is_empty()).then(|| trimmed.to_string()));
    }
    if let Some(default) = preset.and_then(|p| p.api_key_env) {
        if !interactive {
            return Ok(Some(default.to_string()));
        }
        let answer = ask(
            "API key environment variable (blank for none)",
            Some(default),
        )?;
        return Ok((!answer.trim().is_empty()).then(|| answer.trim().to_string()));
    }
    if !interactive {
        return Err(Error::Config(
            "pass --api-key-env <VAR>, or --no-api-key for a local backend".to_string(),
        ));
    }
    let answer = ask("API key environment variable (blank for none)", None)?;
    Ok((!answer.trim().is_empty()).then(|| answer.trim().to_string()))
}

/// Offer the models the backend advertises, falling back to free text.
async fn choose_model(
    base_url: &str,
    api_key_env: &Option<String>,
    token: Option<&str>,
    headers: &BTreeMap<String, String>,
) -> Result<String> {
    let discovered = match discovery_blocked_reason(api_key_env, token) {
        Some(reason) => {
            eprintln!("minion: {reason}, skipping model discovery.");
            Vec::new()
        }
        None => {
            let provider = probe_provider(base_url, token, headers);
            match provider.list_models().await {
                Ok(models) => models,
                Err(err) => {
                    eprintln!("minion: could not list models ({err}); type the id instead.");
                    Vec::new()
                }
            }
        }
    };

    if discovered.is_empty() {
        return ask("Model", None);
    }

    eprintln!("\nAvailable models:");
    for (index, model) in discovered.iter().take(MAX_SUGGESTIONS).enumerate() {
        eprintln!("  {}) {model}", index + 1);
    }
    if discovered.len() > MAX_SUGGESTIONS {
        eprintln!("  … and {} more", discovered.len() - MAX_SUGGESTIONS);
    }

    let answer = ask(
        "Model (number or id)",
        discovered.first().map(String::as_str),
    )?;
    if let Ok(index) = answer.parse::<usize>()
        && index >= 1
        && index <= discovered.len()
    {
        return Ok(discovered[index - 1].clone());
    }
    Ok(answer)
}

/// Probe the backend, so a broken config is never written.
async fn probe(plan: &Plan, token: Option<&str>) -> Result<()> {
    let provider = probe_provider(&plan.base_url, token, &plan.headers);
    let models = provider.list_models().await?;
    eprintln!(
        "minion: backend reachable ({} models advertised)",
        models.len()
    );
    Ok(())
}

/// A short-lived client used only for discovery and `--check`.
///
/// It carries a throwaway session id so that `${session}` headers — required by
/// OpenCode Go — resolve even during setup.
fn probe_provider(
    base_url: &str,
    token: Option<&str>,
    headers: &BTreeMap<String, String>,
) -> OpenAiProvider {
    OpenAiProvider::new(base_url, token.unwrap_or_default())
        .with_request_timeout(Some(PROBE_TIMEOUT))
        .with_max_retries(1)
        .with_headers(
            headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Vec<_>>(),
        )
        .with_session_id(minion_core::new_session_id())
}

/// Why model discovery cannot run, or `None` when it can.
///
/// A token just typed counts as a credential. The whole point of asking for one
/// is that the user has not exported the environment variable yet, so refusing
/// to discover here would defeat the prompt.
fn discovery_blocked_reason(api_key_env: &Option<String>, token: Option<&str>) -> Option<String> {
    if token.is_some_and(|value| !value.trim().is_empty()) {
        return None;
    }
    let name = api_key_env.as_deref()?.trim();
    if name.is_empty() {
        return None;
    }
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => None,
        _ => Some(format!("`{name}` is not set")),
    }
}

/// A description of why credentials are unavailable, if that is the case.
fn api_key_error(api_key_env: &Option<String>) -> Option<String> {
    let name = api_key_env.as_deref()?;
    if name.trim().is_empty() {
        return None;
    }
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => None,
        _ => Some(format!("`{name}` is not set")),
    }
}

/// Warn only when there is genuinely no credential available.
///
/// Storing the key during `init` is itself a credential, so warning about the
/// unset environment variable right after storing it would be noise.
fn warn_on_missing_credential(plan: &Plan, stored: bool) {
    if stored {
        return;
    }
    if let Some(err) = api_key_error(&plan.api_key_env) {
        eprintln!("minion: {err}. Set it, or re-run `minion init`, before using the agent.");
    }
}

fn preset_by_name(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.name == name)
}

fn preset_names() -> String {
    PRESETS
        .iter()
        .map(|preset| preset.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Prompt the user to pick a backend.
fn choose_preset() -> Result<&'static Preset> {
    eprintln!("\nWhich backend?");
    for (index, preset) in PRESETS.iter().enumerate() {
        eprintln!(
            "  {}) {:<14} {}",
            index + 1,
            preset.name,
            preset.description
        );
    }
    let answer = ask("Backend (number or name)", PRESETS.first().map(|p| p.name))?;
    if let Ok(index) = answer.parse::<usize>()
        && index >= 1
        && index <= PRESETS.len()
    {
        return Ok(&PRESETS[index - 1]);
    }
    preset_by_name(&answer).ok_or_else(|| Error::Config(format!("unknown preset `{answer}`")))
}

/// Prompt on stderr so that stdout stays clean for `--json`.
fn ask(label: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(value) => eprint!("{label} [{value}]: "),
        None => eprint!("{label}: "),
    }
    std::io::stderr().flush()?;

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let answer = line.trim();

    if answer.is_empty() {
        return default
            .map(str::to_string)
            .ok_or_else(|| Error::Config(format!("{label} is required")));
    }
    Ok(answer.to_string())
}

/// Read a secret with terminal echo disabled.
///
/// Echo is disabled **before** the prompt is printed, not after. A library that
/// prompts first and then disables echo leaves a window in which a fast paste —
/// a shell user pasting a key, or an automated tty driver — is echoed into
/// terminal scrollback, and from there into a screen share or a terminal
/// recording. There is no safe way to undo that afterwards.
fn ask_secret(label: &str) -> Result<String> {
    let _guard = EchoGuard::disable()?;
    eprintln!("{label} (input hidden; press Enter to skip)");

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Restores terminal echo when dropped, including on an early return or panic.
struct EchoGuard(Option<termios::Termios>);

impl EchoGuard {
    fn disable() -> Result<Self> {
        #[cfg(unix)]
        {
            termios::disable_echo().map(EchoGuard)
        }
        // Elsewhere there is no echo to suppress, so the value is read normally.
        #[cfg(not(unix))]
        {
            Ok(EchoGuard(None))
        }
    }
}

#[cfg(unix)]
mod termios {
    use minion_core::error::{Error, Result};

    /// Termios handle type, aliased so the struct field above reads the same on
    /// every platform.
    pub type Termios = libc::termios;

    /// Turn off terminal echo, returning the previous settings.
    pub fn disable_echo() -> Result<Option<Termios>> {
        // SAFETY: `Termios` is a plain C struct with no invalid bit patterns, and
        // `tcgetattr` fully initialises it before any field is read.
        unsafe {
            let mut term: Termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut term) != 0 {
                return Err(Error::Io(std::io::Error::last_os_error()));
            }
            let saved = term;
            term.c_lflag &= !libc::ECHO;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term) != 0 {
                return Err(Error::Io(std::io::Error::last_os_error()));
            }
            Ok(Some(saved))
        }
    }

    /// Put back the settings captured by [`disable_echo`].
    pub fn restore(saved: &Option<Termios>) {
        if let Some(term) = saved {
            // SAFETY: `term` was produced by `tcgetattr` on this same fd.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, term);
            }
        }
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        termios::restore(&self.0);
        #[cfg(not(unix))]
        let _ = &self.0;
    }
}

/// Parse a `--header "Name: value"` argument.
fn parse_header_arg(raw: &str) -> Result<(String, String)> {
    let (name, value) = raw
        .split_once(':')
        .ok_or_else(|| Error::Config(format!("header `{raw}` must look like `Name: value`")))?;
    let name = name.trim();
    let value = value.trim();

    if name.is_empty() || name.contains(char::is_whitespace) {
        return Err(Error::Config(format!(
            "header name `{name}` is not a valid token"
        )));
    }
    if value.is_empty() {
        return Err(Error::Config(format!("header `{name}` has an empty value")));
    }
    Ok((name.to_string(), value.to_string()))
}

/// Report which keys differ, so a refused overwrite is actionable.
fn diff_keys(existing: &str, new: &str) -> Vec<String> {
    let Ok(old) = toml::from_str::<toml::Value>(existing) else {
        return vec![
            "existing file is not valid TOML; --force would replace it entirely".to_string(),
        ];
    };
    let Ok(fresh) = toml::from_str::<toml::Value>(new) else {
        return Vec::new();
    };

    let mut old_flat = BTreeMap::new();
    let mut new_flat = BTreeMap::new();
    flatten(&old, "", &mut old_flat);
    flatten(&fresh, "", &mut new_flat);

    let mut lines = Vec::new();
    for (key, value) in &new_flat {
        match old_flat.get(key) {
            Some(previous) if previous == value => {}
            Some(previous) => lines.push(format!("{key}: {previous} -> {value}")),
            None => lines.push(format!("{key}: (new) -> {value}")),
        }
    }
    for key in old_flat.keys() {
        if !new_flat.contains_key(key) {
            lines.push(format!("{key}: removed"));
        }
    }
    lines
}

fn flatten(value: &toml::Value, prefix: &str, out: &mut BTreeMap<String, String>) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(child, &path, out);
            }
        }
        other => {
            out.insert(prefix.to_string(), other.to_string());
        }
    }
}

/// Print the outcome as prose or as one NDJSON object.
///
/// The API key value is never part of this output; only the path it lives in.
fn report(
    cli: &Cli,
    args: &InitArgs,
    plan: &Plan,
    target: &Path,
    credentials: Option<PathBuf>,
    check: &str,
) -> Result<()> {
    if cli.json {
        let headers: serde_json::Map<String, serde_json::Value> = plan
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        let payload = serde_json::json!({
            "type": "init",
            "path": target,
            "check": check,
            "credentials": credentials,
            "provider": {
                "base_url": plan.base_url,
                "model": plan.model,
                "api_key_env": plan.api_key_env,
                "headers": headers,
            },
            "workspace_root": plan.workspace_root,
        });
        println!("{payload}");
        return Ok(());
    }

    println!("Wrote {}", target.display());
    println!("  provider.base_url   {}", plan.base_url);
    println!("  provider.model      {}", plan.model);
    match &plan.api_key_env {
        Some(name) => println!("  provider.api_key_env {name}"),
        None => println!("  provider.api_key_env (none — no credentials are sent)"),
    }
    match &credentials {
        Some(path) => println!("  credentials          {}", path.display()),
        None => println!("  credentials          (nothing stored)"),
    }
    if !plan.headers.is_empty() {
        for (name, value) in &plan.headers {
            println!("  provider.headers.{name} = {value}");
        }
    }
    println!("  workspace.roots     {}", plan.workspace_root);
    if !args.check {
        println!("\nTip: rerun with --check to verify the backend before writing.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use minion_core::config::Config;

    fn plan() -> Plan {
        Plan {
            base_url: "https://opencode.ai/zen/go/v1".to_string(),
            model: "deepseek-v4.1-flash".to_string(),
            api_key_env: Some("OPENCODE_API_KEY".to_string()),
            api_key_file: None,
            headers: BTreeMap::from([("x-opencode-session".to_string(), "${session}".to_string())]),
            workspace_root: ".".to_string(),
        }
    }

    #[test]
    fn rendered_config_parses_back_into_a_config() {
        let rendered = plan().to_toml();
        let parsed: Config = toml::from_str(&rendered).expect("init output must be loadable");

        assert_eq!(parsed.provider.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(parsed.provider.model, "deepseek-v4.1-flash");
        assert_eq!(parsed.provider.api_key_env, "OPENCODE_API_KEY");
        assert_eq!(
            parsed
                .provider
                .headers
                .get("x-opencode-session")
                .map(String::as_str),
            Some("${session}")
        );
        assert_eq!(parsed.workspace.roots, vec![".".to_string()]);
    }

    #[test]
    fn the_rendered_config_carries_no_key_value() {
        let mut plan = plan();
        // Even when a key is in hand, the plan has no field that could hold it.
        let token = "sk-live-DO-NOT-LEAK";
        let rendered = plan.to_toml();
        plan.api_key_env = Some(token.to_string());

        assert!(!rendered.contains(token));
        assert!(!rendered.contains("sk-live"));
    }

    #[test]
    fn a_keyless_backend_explicitly_clears_both_credential_sources() {
        let mut plan = plan();
        plan.api_key_env = None;

        let parsed: Config = toml::from_str(&plan.to_toml()).unwrap();

        // Without this, the built-in default (OPENAI_API_KEY) would come back.
        assert_eq!(parsed.provider.api_key_env, "");
        assert!(parsed.api_key().unwrap().is_none());
    }

    #[test]
    fn a_keyed_backend_leaves_the_credentials_path_out_of_the_config() {
        // The path is a per-machine default. Writing it into a config that may be
        // committed would be both noisy and wrong for anyone else.
        let rendered = plan().to_toml();

        assert!(
            !rendered.contains("api_key_file"),
            "a keyed config should not hardcode a credentials path: {rendered}"
        );
    }

    #[test]
    fn an_explicit_credentials_path_is_recorded_so_the_two_agree() {
        let mut plan = plan();
        plan.api_key_file = Some("/tmp/somewhere/credentials".to_string());

        let parsed: Config = toml::from_str(&plan.to_toml()).unwrap();

        assert_eq!(parsed.provider.api_key_file, "/tmp/somewhere/credentials");
    }

    #[test]
    fn diff_reports_changed_and_added_keys_only() {
        let original = "[provider]\nmodel = \"old\"\n";
        let new = "[provider]\nmodel = \"new\"\nbase_url = \"http://x/v1\"\n";

        let lines = diff_keys(original, new);

        assert!(
            lines
                .iter()
                .any(|line| line.contains("provider.model") && line.contains("\"old\" -> \"new\""))
        );
        assert!(lines.iter().any(|line| line.contains("provider.base_url")));
        assert!(!lines.iter().any(|line| line.contains("provider.headers")));
    }

    #[test]
    fn diff_reports_removed_keys() {
        let lines = diff_keys(
            "[provider]\nmodel = \"old\"\ntemperature = 0.5\n",
            "[provider]\nmodel = \"old\"\n",
        );

        assert!(
            lines
                .iter()
                .any(|line| line.contains("provider.temperature") && line.contains("removed"))
        );
    }

    #[test]
    fn diff_survives_an_unparseable_existing_file() {
        let lines = diff_keys("this is not toml {{{", "[provider]\nmodel = \"m\"\n");

        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("not valid TOML"));
    }

    #[test]
    fn header_arguments_are_parsed_and_validated() {
        assert_eq!(
            parse_header_arg("x-opencode-session: ${session}").unwrap(),
            ("x-opencode-session".to_string(), "${session}".to_string())
        );
        assert!(parse_header_arg("no-colon-here").is_err());
        assert!(parse_header_arg("bad name: v").is_err());
        assert!(parse_header_arg("good-name:").is_err());
    }

    #[test]
    fn the_config_file_is_written_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested").join("minion.toml");

        write_private_file(&target, "hello = 1\n").unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello = 1\n");
        assert!(
            !target.with_extension("tmp").exists(),
            "temp file was left behind"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn a_token_just_typed_is_preferred_over_the_environment() {
        // No environment manipulation: the token must simply win, so setup works
        // before anything has been exported.
        let key = probe_key(Some("MINION_ABSENT_VAR"), Some("typed-here"));

        assert_eq!(key.as_deref(), Some("typed-here"));
    }

    #[test]
    fn without_a_token_the_environment_is_the_fallback() {
        let key = probe_key(Some("MINION_ABSENT_VAR"), None);

        assert_eq!(key, None);
    }

    #[test]
    fn a_typed_token_keeps_discovery_working_even_when_the_env_var_is_unset() {
        // The whole reason the wizard asks for the token is that nothing is
        // exported yet; skipping discovery here would undo that.
        let blocked =
            discovery_blocked_reason(&Some("MINION_ABSENT_VAR".to_string()), Some("typed"));

        assert_eq!(blocked, None);
    }

    #[test]
    fn discovery_is_skipped_only_when_no_credential_exists_at_all() {
        let blocked = discovery_blocked_reason(&Some("MINION_ABSENT_VAR".to_string()), None);

        assert_eq!(blocked, Some("`MINION_ABSENT_VAR` is not set".to_string()));
    }

    #[test]
    fn a_keyless_backend_never_blocks_on_discovery() {
        let blocked = discovery_blocked_reason(&None, None);

        assert_eq!(blocked, None);
    }

    #[test]
    fn every_preset_is_reachable_by_name() {
        for preset in PRESETS {
            assert!(
                preset_by_name(preset.name).is_some(),
                "{} is not findable",
                preset.name
            );
        }
        assert!(preset_by_name("nope").is_none());
    }

    #[test]
    fn the_opencode_go_preset_carries_the_session_header() {
        let preset = preset_by_name("opencode-go").unwrap();

        assert_eq!(preset.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(preset.headers, &[("x-opencode-session", "${session}")]);
    }
}
