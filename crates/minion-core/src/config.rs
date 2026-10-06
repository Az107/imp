//! Layered configuration.
//!
//! Precedence, highest first: CLI flags → `MINION_*` environment variables →
//! project `minion.toml` → user config file → built-in defaults.
//!
//! The CLI layer applies flag overrides on top of [`Config::load`]; this module
//! owns everything below flags. Overlaying is done on `toml::Value` trees, so a
//! project file only needs to restate the keys it actually changes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Application directory name used for config, state, and logs.
const APP: &str = "minion";

/// Fully resolved configuration for one run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Model endpoint settings.
    pub provider: ProviderConfig,
    /// Agent loop settings.
    pub agent: AgentSettings,
    /// Filesystem boundary settings.
    pub workspace: WorkspaceConfig,
    /// Process execution settings.
    pub exec: ExecConfig,
    /// Approval policy.
    pub policy: PolicyConfig,
    /// Outbound HTTP limits and the SSRF guard for `http_fetch`.
    pub http_fetch: HttpFetchConfig,
    /// The optional System One guard for the approval gate.
    pub guard: GuardConfig,
    /// Logging settings.
    pub logging: LoggingConfig,
}

/// Settings for the OpenAI-compatible endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Base URL *without* `/chat/completions`.
    pub base_url: String,
    /// Name of the environment variable holding the API key. Never the key itself.
    ///
    /// Empty means "this backend needs no credentials".
    pub api_key_env: String,
    /// Path to the credentials file holding the API key.
    ///
    /// Defaults to the standard location next to the user config, so secrets
    /// never live in a config file. Empty disables the lookup.
    pub api_key_file: String,
    /// Model identifier.
    pub model: String,
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Stream responses.
    pub stream: bool,
    /// Total budget for one HTTP request.
    pub request_timeout_secs: u64,
    /// Retry attempts for pre-stream failures.
    pub max_retries: u32,
    /// Whether the provider reports usage on the final stream chunk.
    pub supports_usage_in_stream: bool,
    /// Whether to request multiple tool calls per assistant turn.
    pub parallel_tool_calls: bool,
    /// Extra headers sent on every request.
    ///
    /// Values may contain `${session}`, which expands to the stable identifier
    /// of the current conversation. Gateways that pin a conversation to one
    /// upstream need this: OpenCode Go, for example, requires
    /// `x-opencode-session` so it can route and cache prompts consistently.
    pub headers: BTreeMap<String, String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
            api_key_file: default_credentials_path()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            model: "gpt-4.1-mini".to_string(),
            temperature: Some(0.2),
            stream: true,
            request_timeout_secs: 120,
            max_retries: 3,
            supports_usage_in_stream: true,
            parallel_tool_calls: true,
            headers: BTreeMap::new(),
        }
    }
}

/// Settings for the agent loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    /// Optional file whose contents replace the built-in system prompt.
    pub system_prompt_file: Option<String>,
    /// Maximum provider round-trips per turn.
    pub max_iterations: u32,
    /// Advisory token ceiling for a turn.
    pub max_tokens_per_turn: u64,
    /// How many trailing messages to send.
    pub history_window: usize,
    /// Summarize dropped history instead of discarding it silently.
    pub summarize_on_truncate: bool,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            system_prompt_file: None,
            max_iterations: 25,
            max_tokens_per_turn: 200_000,
            history_window: 40,
            summarize_on_truncate: true,
        }
    }
}

/// Filesystem boundary the tools may not cross.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    /// Read-write roots. Relative entries resolve against the process cwd.
    pub roots: Vec<String>,
    /// Roots that may be read but never written.
    pub read_only_roots: Vec<String>,
    /// Largest file a single read or write may touch.
    pub max_file_bytes: u64,
    /// Whether path resolution may follow symlinks out of the root.
    pub follow_symlinks: bool,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            roots: vec![".".to_string()],
            read_only_roots: Vec::new(),
            max_file_bytes: 2_097_152,
            follow_symlinks: false,
        }
    }
}

/// Process execution limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecConfig {
    /// Default command timeout.
    pub default_timeout_secs: u64,
    /// Hard ceiling the model may not raise.
    pub max_timeout_secs: u64,
    /// Bytes of stdout/stderr retained per stream.
    pub output_cap_bytes: u64,
    /// Shell used for `run_command`.
    pub shell: String,
    /// Use `-lc` instead of `-c` so login profiles are sourced.
    pub login_shell: bool,
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: 120,
            max_timeout_secs: 3600,
            output_cap_bytes: 262_144,
            shell: "/bin/sh".to_string(),
            login_shell: false,
        }
    }
}

/// Approval policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    /// Outcome when no other rule matches.
    pub default: Decision,
    /// Outcome when stdin is not a TTY. Defaults to deny: fail closed.
    ///
    /// Applies to tools that change something. `ReadOnly` tools are allowed
    /// regardless, per SDD §5.6 and D14; `Network` counts as a change.
    pub noninteractive: Decision,
    /// Patterns that pre-approve a tool call.
    pub allow: Vec<AllowRule>,
    /// Patterns that always refuse a tool call. Always beats `allow`.
    pub deny: Vec<DenyRule>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            default: Decision::Ask,
            noninteractive: Decision::Deny,
            allow: Vec::new(),
            deny: Vec::new(),
        }
    }
}

/// What policy decides about an invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Run without asking.
    Auto,
    /// Prompt the user.
    #[default]
    Ask,
    /// Refuse.
    Deny,
}

/// A pattern that pre-approves matching invocations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllowRule {
    /// Tool this rule applies to.
    pub tool: String,
    /// Command or argument prefix to match. Glob syntax, anchored at the start.
    pub pattern: String,
    /// `once`-like scopes are session-only; `always` persists.
    #[serde(default = "default_scope")]
    pub scope: String,
}

fn default_scope() -> String {
    "session".to_string()
}

/// A pattern that always refuses matching invocations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DenyRule {
    /// Tool this rule applies to.
    pub tool: String,
    /// Glob pattern to match.
    pub pattern: String,
}

/// Limits and the SSRF guard for the `http_fetch` tool (SDD §5.5, threat T3).
///
/// Two separate lists gate an outbound request, and they mean different things:
/// this `allowed_domains` is the *guard* — a host that does not match it cannot
/// be fetched at all, whatever the approval policy says. Approval is the
/// ordinary `[policy.allow]` mechanism, matched against the URL's host because
/// `http_fetch` names the host as its approval subject. Keeping the two apart is
/// what lets `allowed_domains` be a security boundary rather than a convenience.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpFetchConfig {
    /// Hosts the tool may reach, as exact names or globs (`*.example.com`).
    ///
    /// A plain `http://` URL is granted only by an exact, non-glob entry — a
    /// wildcard raises reachability, not the scheme (SDD §5.5, §11.1). Empty
    /// means nothing is fetchable: the guard fails closed.
    pub allowed_domains: Vec<String>,
    /// Refuse a host that resolves to a private, loopback, link-local,
    /// unique-local or otherwise non-public address.
    pub block_private_ips: bool,
    /// Largest response body kept, in bytes.
    pub max_bytes: u64,
    /// Wall-clock budget for one request, in seconds.
    pub timeout_secs: u64,
}

impl Default for HttpFetchConfig {
    fn default() -> Self {
        Self {
            allowed_domains: Vec::new(),
            block_private_ips: true,
            max_bytes: 1_048_576,
            timeout_secs: 20,
        }
    }
}

/// The optional System One guard for the approval gate (SDD §5.6, D16).
///
/// When enabled, a flagged `run_command` in an eligible category may be
/// resolved by a `/v1/systemone` model instead of prompting. The guard is
/// **disabled by default**: it is a network call that carries command text to a
/// third party, so it stays opt-in until it has been seen behaving. It can only
/// narrow prompts, never widen permissions (FR-43), and every failure falls
/// back to the existing prompt (FR-45).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuardConfig {
    /// Whether the guard is consulted at all. Off by default.
    pub enabled: bool,
    /// Server root of the System One model, *without* the `/v1` suffix.
    ///
    /// The request is sent to `{base_url}/v1/systemone`.
    pub base_url: String,
    /// Highest risk that resolves silently. A verdict at or below this allows.
    pub allow_threshold: f32,
    /// Risk above which the verdict is a confident refusal.
    ///
    /// The band between the two thresholds also prompts; the distinction is
    /// what the audit trail records (FR-47).
    pub deny_threshold: f32,
    /// Wall-clock budget for one verdict, in seconds.
    pub timeout_secs: u64,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            allow_threshold: 0.25,
            deny_threshold: 0.75,
            timeout_secs: 10,
        }
    }
}

/// Log destination and verbosity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// Tracing filter directive.
    pub level: String,
    /// Human-readable or JSON lines.
    pub format: LogFormat,
    /// Optional log file. `None` logs to stderr only.
    pub file: Option<String>,
    /// Scrub values of API-key environment variables from logs.
    pub redact_env: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: LogFormat::Text,
            file: None,
            redact_env: true,
        }
    }
}

/// Log rendering format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable lines.
    #[default]
    Text,
    /// Newline-delimited JSON.
    Json,
}

impl Config {
    /// Load the layered configuration for a process started in `cwd`.
    ///
    /// `explicit` is the `--config` path, which wins over everything except
    /// environment variables and CLI flags.
    pub fn load(explicit: Option<&Path>, cwd: &Path) -> Result<Self> {
        Self::load_with(user_config_path().as_deref(), explicit, cwd)
    }

    /// Load with every config path supplied, for tests and embedders.
    ///
    /// Exists so tests can opt out of the developer's real user config instead
    /// of silently inheriting whatever `minion init` last wrote to it.
    pub fn load_with(user: Option<&Path>, explicit: Option<&Path>, cwd: &Path) -> Result<Self> {
        let mut merged = toml::Value::Table(toml::Table::new());

        if let Some(path) = user {
            merge_file(&mut merged, path)?;
        }
        merge_file(&mut merged, &cwd.join("minion.toml"))?;
        if let Some(path) = explicit {
            merge_file(&mut merged, path)?;
        }

        let mut config: Config = match merged.as_table() {
            Some(table) if table.is_empty() => Config::default(),
            _ => merged
                .try_into()
                .map_err(|err| Error::Config(format!("invalid configuration: {err}")))?,
        };
        config.apply_env();
        config.validate()?;
        Ok(config)
    }

    /// Overlay `MINION_*` environment variables.
    fn apply_env(&mut self) {
        if let Ok(value) = std::env::var("MINION_MODEL") {
            self.provider.model = value;
        }
        if let Ok(value) = std::env::var("MINION_BASE_URL") {
            self.provider.base_url = value;
        }
        if let Ok(value) = std::env::var("MINION_API_KEY_ENV") {
            self.provider.api_key_env = value;
        }
        if let Ok(value) = std::env::var("MINION_LOG_LEVEL") {
            self.logging.level = value;
        }
        if let Ok(value) = std::env::var("MINION_MAX_ITERATIONS")
            && let Ok(parsed) = value.parse()
        {
            self.agent.max_iterations = parsed;
        }
    }

    /// Reject configurations that cannot work, before any network call.
    fn validate(&self) -> Result<()> {
        if self.provider.base_url.trim().is_empty() {
            return Err(Error::Config(
                "provider.base_url must not be empty".to_string(),
            ));
        }
        if self.provider.model.trim().is_empty() {
            return Err(Error::Config(
                "provider.model must not be empty".to_string(),
            ));
        }
        if self.agent.max_iterations == 0 {
            return Err(Error::Config(
                "agent.max_iterations must be at least 1".to_string(),
            ));
        }
        if self.workspace.roots.is_empty() {
            return Err(Error::Config(
                "workspace.roots must list at least one root".to_string(),
            ));
        }
        if self.exec.shell.trim().is_empty() {
            return Err(Error::Config("exec.shell must not be empty".to_string()));
        }
        if self.http_fetch.max_bytes == 0 {
            return Err(Error::Config(
                "http_fetch.max_bytes must be at least 1".to_string(),
            ));
        }
        if self.http_fetch.timeout_secs == 0 {
            return Err(Error::Config(
                "http_fetch.timeout_secs must be at least 1".to_string(),
            ));
        }
        if self.guard.timeout_secs == 0 {
            return Err(Error::Config(
                "guard.timeout_secs must be at least 1".to_string(),
            ));
        }
        // Thresholds are probabilities. A NaN fails both range checks, which is
        // the point: an unset threshold must not silently become a comparison
        // that can only ever allow.
        for (name, value) in [
            ("allow_threshold", self.guard.allow_threshold),
            ("deny_threshold", self.guard.deny_threshold),
        ] {
            if !(0.0..=1.0).contains(&value) {
                return Err(Error::Config(format!(
                    "guard.{name} must be a probability between 0 and 1, was {value}"
                )));
            }
        }
        if self.guard.allow_threshold > self.guard.deny_threshold {
            return Err(Error::Config(
                "guard.allow_threshold must not exceed guard.deny_threshold".to_string(),
            ));
        }
        if self.guard.enabled && self.guard.base_url.trim().is_empty() {
            return Err(Error::Config(
                "guard.base_url must be set when guard.enabled is true".to_string(),
            ));
        }
        Ok(())
    }

    /// Read the API key named by `provider.api_key_env`.
    ///
    /// The key is never stored in the config file and never logged.
    /// Read the API key.
    ///
    /// Resolution order, so a single shell or CI job can override the stored
    /// secret without editing anything:
    ///
    /// 1. the environment variable named by `provider.api_key_env`, if set and non-empty;
    /// 2. `provider.api_key` in the file named by `provider.api_key_file`;
    /// 3. otherwise an error, unless *both* sources are unset, which is how a
    ///    keyless local backend is expressed.
    pub fn api_key(&self) -> Result<Option<String>> {
        let env_name = self.provider.api_key_env.trim();
        if !env_name.is_empty()
            && let Ok(value) = std::env::var(env_name)
            && !value.trim().is_empty()
        {
            return Ok(Some(value));
        }

        let file_path = self.provider.api_key_file.trim();
        if !file_path.is_empty() {
            let path = expand_home(file_path);
            let credentials = Credentials::load(&path)?;
            if !credentials.api_key.trim().is_empty() {
                return Ok(Some(credentials.api_key));
            }
        }

        if env_name.is_empty() && file_path.is_empty() {
            return Ok(None);
        }

        let mut sources = Vec::new();
        if !env_name.is_empty() {
            sources.push(format!("the environment variable `{env_name}`"));
        }
        if !file_path.is_empty() {
            sources.push(format!("`provider.api_key` in {}", file_path));
        }
        Err(Error::Auth(format!(
            "no credentials found — set {} (or run `minion init`)",
            sources.join(" or ")
        )))
    }

    /// Headers to send on every provider request.
    ///
    /// `${session}` in a value is replaced by the conversation identifier by the
    /// provider client; it is passed through verbatim here.
    pub fn request_headers(&self) -> Vec<(String, String)> {
        self.provider
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }

    /// Canonical read-write root, resolved against `cwd`.
    pub fn workspace_root(&self, cwd: &Path) -> Result<PathBuf> {
        let raw = self
            .workspace
            .roots
            .first()
            .ok_or_else(|| Error::Config("workspace.roots is empty".to_string()))?;
        let path = cwd.join(raw);
        Ok(path.canonicalize().unwrap_or(path))
    }

    /// Path the session database lives at.
    pub fn database_path(&self) -> PathBuf {
        ProjectDirs::from("", "", APP)
            .and_then(|dirs| dirs.state_dir().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("minion.db")
    }
}

/// `$XDG_CONFIG_HOME/minion/config.toml`, or the macOS equivalent.
pub fn user_config_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", APP).map(|dirs| dirs.config_dir().join("config.toml"))
}

/// The default credentials file, beside the user config.
///
/// Kept separate from the config so that a config may be committed while the
/// secret never is.
pub fn default_credentials_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", APP).map(|dirs| dirs.config_dir().join("credentials"))
}

/// The secret half of the configuration.
///
/// This file is written by `minion init` and is never committed, never logged,
/// and never included in `--json` output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Credentials {
    /// The API key, stored as written.
    pub api_key: String,
}

impl Credentials {
    /// Read the credentials file, treating an absent file as empty.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|err| {
            Error::Auth(format!(
                "cannot read credentials at {}: {err}",
                path.display()
            ))
        })
    }

    /// Write the credentials file with owner-only permissions.
    pub fn write(path: &Path, api_key: &str) -> Result<()> {
        let body = format!(
            "# minion credentials. Written by `minion init`; never commit this.\n\
             # The `Authorization` header is built from this value at run time.\n\n\
             api_key = {}\n",
            toml::Value::String(api_key.to_string())
        );
        crate::fs::write_private_file(path, &body)
    }
}

/// Expand a leading `~` to the user's home directory.
fn expand_home(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix('~') else {
        return PathBuf::from(path);
    };
    let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) else {
        return PathBuf::from(path);
    };
    match rest.trim_start_matches('/') {
        "" => home,
        rest => home.join(rest),
    }
}

/// Merge a TOML file into `base`, ignoring a missing file.
fn merge_file(base: &mut toml::Value, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path)?;
    let value: toml::Value =
        toml::from_str(&text).map_err(|err| Error::Config(format!("{}: {err}", path.display())))?;
    deep_merge(base, value);
    Ok(())
}

/// Recursively overlay tables; scalars and arrays replace wholesale.
fn deep_merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(base_table), toml::Value::Table(over_table)) => {
            for (key, value) in over_table {
                match base_table.get_mut(&key) {
                    Some(slot) => deep_merge(slot, value),
                    None => {
                        base_table.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn defaults_match_the_spec() {
        let config = Config::default();
        assert_eq!(config.agent.max_iterations, 25);
        assert_eq!(config.exec.output_cap_bytes, 262_144);
        assert_eq!(config.policy.default, Decision::Ask);
        assert_eq!(config.policy.noninteractive, Decision::Deny);
        assert!(!config.workspace.follow_symlinks);
    }

    #[test]
    fn http_fetch_defaults_to_a_closed_guard() {
        let config = Config::default();
        assert!(
            config.http_fetch.allowed_domains.is_empty(),
            "an empty allowlist means nothing is fetchable until it is configured"
        );
        assert!(
            config.http_fetch.block_private_ips,
            "the private-IP block must default on; it is the SSRF mitigation"
        );
        assert_eq!(config.http_fetch.max_bytes, 1_048_576);
        assert_eq!(config.http_fetch.timeout_secs, 20);
    }

    #[test]
    fn the_http_fetch_section_is_read_from_a_project_file() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("minion.toml"),
            "[http_fetch]\nallowed_domains = [\"docs.rs\", \"*.github.com\"]\n\
             block_private_ips = false\nmax_bytes = 4096\ntimeout_secs = 3\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();

        assert_eq!(
            config.http_fetch.allowed_domains,
            vec!["docs.rs".to_string(), "*.github.com".to_string()]
        );
        assert!(!config.http_fetch.block_private_ips);
        assert_eq!(config.http_fetch.max_bytes, 4096);
        assert_eq!(config.http_fetch.timeout_secs, 3);
    }

    #[test]
    fn an_unusable_http_fetch_cap_is_rejected_before_use() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("minion.toml"),
            "[http_fetch]\nmax_bytes = 0\n",
        );

        let err = Config::load_with(None, None, dir.path()).unwrap_err();

        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
    }

    #[test]
    fn the_guard_is_disabled_by_default() {
        let config = Config::default();
        assert!(
            !config.guard.enabled,
            "the guard is a network call carrying command text, so it stays opt-in"
        );
        assert!(config.guard.base_url.is_empty());
        assert_eq!(config.guard.allow_threshold, 0.25);
        assert_eq!(config.guard.deny_threshold, 0.75);
        assert_eq!(config.guard.timeout_secs, 10);
    }

    #[test]
    fn the_guard_section_is_read_from_a_project_file() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("minion.toml"),
            "[guard]\nenabled = true\nbase_url = \"http://127.0.0.1:8080\"\n\
             allow_threshold = 0.1\ndeny_threshold = 0.9\ntimeout_secs = 3\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();

        assert!(config.guard.enabled);
        assert_eq!(config.guard.base_url, "http://127.0.0.1:8080");
        assert_eq!(config.guard.allow_threshold, 0.1);
        assert_eq!(config.guard.deny_threshold, 0.9);
        assert_eq!(config.guard.timeout_secs, 3);
    }

    #[test]
    fn an_enabled_guard_needs_a_base_url() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("minion.toml"), "[guard]\nenabled = true\n");

        let err = Config::load_with(None, None, dir.path()).unwrap_err();

        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
        assert!(err.to_string().contains("base_url"), "was: {err}");
    }

    #[test]
    fn guard_thresholds_must_be_ordered_probabilities() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("minion.toml"),
            "[guard]\nallow_threshold = 0.9\ndeny_threshold = 0.1\n",
        );
        let err = Config::load_with(None, None, dir.path()).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");

        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("minion.toml"),
            "[guard]\nallow_threshold = 1.5\n",
        );
        let err = Config::load_with(None, None, dir.path()).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
    }

    #[test]
    fn a_zero_guard_timeout_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("minion.toml"),
            "[guard]\ntimeout_secs = 0\n",
        );

        let err = Config::load_with(None, None, dir.path()).unwrap_err();

        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
    }

    #[test]
    fn project_file_overrides_only_the_keys_it_sets() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(
            &cwd.join("minion.toml"),
            "[provider]\nmodel = \"local-model\"\n",
        );

        let config = Config::load_with(None, None, cwd).unwrap();

        assert_eq!(config.provider.model, "local-model");
        // Untouched keys keep their defaults.
        assert_eq!(config.provider.base_url, ProviderConfig::default().base_url);
        assert_eq!(config.agent.max_iterations, 25);
    }

    #[test]
    fn explicit_config_beats_the_project_file() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(
            &cwd.join("minion.toml"),
            "[provider]\nmodel = \"from-project\"\n",
        );
        let explicit = cwd.join("elsewhere.toml");
        write(&explicit, "[provider]\nmodel = \"from-flag\"\n");

        let config = Config::load_with(None, Some(&explicit), cwd).unwrap();

        assert_eq!(config.provider.model, "from-flag");
    }

    #[test]
    fn nested_tables_merge_rather_than_replace() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(
            &cwd.join("minion.toml"),
            "[policy]\nnoninteractive = \"auto\"\n\n[[policy.allow]]\ntool = \"run_command\"\npattern = \"ls *\"\n",
        );

        let config = Config::load_with(None, None, cwd).unwrap();

        assert_eq!(config.policy.noninteractive, Decision::Auto);
        // Sibling key under the same table survives the merge.
        assert_eq!(config.policy.default, Decision::Ask);
        assert_eq!(config.policy.allow.len(), 1);
        assert_eq!(config.policy.allow[0].scope, "session");
    }

    #[test]
    fn invalid_values_are_rejected_before_use() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(&cwd.join("minion.toml"), "[agent]\nmax_iterations = 0\n");

        let err = Config::load_with(None, None, cwd).unwrap_err();

        assert!(matches!(err, Error::Config(_)), "unexpected error: {err}");
    }

    #[test]
    fn a_user_config_is_layered_under_the_project_file() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let user = cwd.join("user.toml");
        write(
            &user,
            "[provider]\nmodel = \"from-user\"\nbase_url = \"http://user/v1\"\n",
        );
        write(
            &cwd.join("minion.toml"),
            "[provider]\nmodel = \"from-project\"\n",
        );

        let config = Config::load_with(Some(&user), None, cwd).unwrap();

        // The project file wins on the key both set...
        assert_eq!(config.provider.model, "from-project");
        // ...and the user file still supplies the rest.
        assert_eq!(config.provider.base_url, "http://user/v1");
    }

    #[test]
    fn tests_never_read_the_developers_real_user_config() {
        // Regression guard: a test that passes `None` for the user path must not
        // pick up whatever `minion init` last wrote to the real config location.
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("minion.toml"),
            "[provider]\nmodel = \"m\"\n",
        );

        let config = Config::load_with(None, None, dir.path()).unwrap();

        assert_eq!(config.provider.base_url, ProviderConfig::default().base_url);
    }

    #[test]
    fn unknown_keys_are_ignored_for_forward_compatibility() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        write(&cwd.join("minion.toml"), "[future]\nsome_key = true\n");

        assert!(Config::load_with(None, None, cwd).is_ok());
    }

    #[test]
    fn credentials_default_to_a_separate_private_file() {
        let config = Config::default();

        assert!(!config.provider.api_key_file.is_empty());
        assert!(
            config.provider.api_key_file.ends_with("credentials"),
            "unexpected default: {}",
            config.provider.api_key_file
        );
    }

    #[test]
    fn the_credentials_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");

        Credentials::write(&path, "sk-secret-value").unwrap();
        let loaded = Credentials::load(&path).unwrap();

        assert_eq!(loaded.api_key, "sk-secret-value");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "credentials must not be group- or world-readable"
            );
        }
    }

    #[test]
    fn a_missing_credentials_file_is_empty_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();

        let loaded = Credentials::load(&dir.path().join("absent")).unwrap();

        assert_eq!(loaded.api_key, "");
    }

    #[test]
    fn a_malformed_credentials_file_is_reported_as_a_credential_problem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        fs::write(&path, "api_key = ").unwrap();

        let err = Credentials::load(&path).unwrap_err();

        assert!(matches!(err, Error::Auth(_)), "unexpected error: {err}");
    }

    /// A config wired to one specific credentials file and env var.
    fn credential_config(dir: &Path, api_key_file: &str, api_key_env: &str) -> Config {
        let mut config = Config::default();
        config.provider.api_key_file = api_key_file.to_string();
        config.provider.api_key_env = api_key_env.to_string();
        config.provider.base_url = "http://localhost/v1".to_string();
        let _ = dir;
        config
    }

    #[test]
    fn the_stored_key_is_used_when_no_environment_variable_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        Credentials::write(&path, "from-file").unwrap();
        // A name that is deliberately unset in any environment.
        let config = credential_config(dir.path(), path.to_str().unwrap(), "MINION_ABSENT_VAR");

        let key = config.api_key().unwrap();

        assert_eq!(key.as_deref(), Some("from-file"));
    }

    #[test]
    fn a_keyless_backend_declares_itself_by_emptying_both_sources() {
        let mut config = Config::default();
        config.provider.api_key_env = String::new();
        config.provider.api_key_file = String::new();

        assert!(config.api_key().unwrap().is_none());
    }

    #[test]
    fn a_configured_but_absent_credential_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = credential_config(dir.path(), "/nonexistent/credentials", "MINION_ABSENT_VAR");

        let err = config.api_key().unwrap_err();

        assert!(matches!(err, Error::Auth(_)), "unexpected error: {err}");
        let message = err.to_string();
        assert!(
            message.contains("MINION_ABSENT_VAR"),
            "message was: {message}"
        );
        assert!(
            message.contains("minion init"),
            "message should say how to fix it: {message}"
        );
    }
}
