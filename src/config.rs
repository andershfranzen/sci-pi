use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

pub const DAEMON_PORT: u16 = 7433;
pub const HUB_PORT: u16 = 7430;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// Daemon config, `~/.config/outpost/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    /// e.g. "https://ntfy.sh/my-secret-topic" – push on approvals and finished turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ntfy_url: Option<String>,
    #[serde(default)]
    pub tailscale: TailscaleConfig,
    /// outpost's own agent.
    #[serde(default)]
    pub native: NativeConfig,
    #[serde(default = "default_agents")]
    pub agents: BTreeMap<String, AgentSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TailscaleConfig {
    /// Also listen on this machine's Tailscale IPs when tailscaled is running.
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Tailnet logins that may connect without a token. Empty = the node's owner
    /// (tagged nodes have none, so `outpost add` fills this in).
    #[serde(default)]
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeConfig {
    /// `claude-*` for Anthropic, or `<provider>/<model>` for an entry in `providers`.
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_effort")]
    pub effort: String,
    /// OpenAI-compatible endpoints (OpenAI, OpenRouter, llama.cpp, vLLM, Ollama, …).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// OpenAI-compatible chat completions; `base_url` ends in `/v1`.
    #[default]
    Openai,
    /// Anthropic Messages API at `base_url` (`/v1/messages` is appended).
    Anthropic,
    /// A CLIProxyAPI server (root URL): `claude-*` over its Anthropic endpoint, everything else
    /// over its OpenAI endpoint. Models are discovered from `/v1/models`.
    Cliproxy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub kind: ProviderKind,
    pub base_url: String,
    /// Env var holding the key; otherwise `outpost auth set <name>`. Local servers need none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Empty = discover from the endpoint's `/models`.
    #[serde(default)]
    pub models: Vec<String>,
}

impl Default for NativeConfig {
    fn default() -> Self {
        NativeConfig { model: default_model(), effort: default_effort(), providers: BTreeMap::new() }
    }
}

fn default_model() -> String {
    "claude-opus-5-5".into()
}

fn default_effort() -> String {
    "xhigh".into()
}

/// The native agent: this binary in ACP mode.
pub const NATIVE_AGENT: &str = "outpost";

fn native_agent() -> AgentSpec {
    AgentSpec { name: "outpost".into(), command: vec!["@self".into(), "acp".into()], env: BTreeMap::new() }
}

impl Default for TailscaleConfig {
    fn default() -> Self {
        TailscaleConfig { enabled: true, port: DAEMON_PORT, allow: vec![] }
    }
}

fn yes() -> bool {
    true
}

fn default_port() -> u16 {
    DAEMON_PORT
}

fn default_bind() -> String {
    format!("127.0.0.1:{DAEMON_PORT}")
}

fn default_agents() -> BTreeMap<String, AgentSpec> {
    let agent = |name: &str, cmd: &[&str]| AgentSpec {
        name: name.into(),
        command: cmd.iter().map(|s| s.to_string()).collect(),
        env: BTreeMap::new(),
    };
    BTreeMap::from([
        (NATIVE_AGENT.into(), native_agent()),
        ("claude".into(), agent("Claude Code", &["npx", "-y", "@agentclientprotocol/claude-agent-acp"])),
        ("codex".into(), agent("Codex", &["npx", "-y", "@zed-industries/codex-acp"])),
        ("opencode".into(), agent("OpenCode", &["opencode", "acp"])),
        ("omp".into(), agent("oh-my-pi", &["omp", "acp"])),
    ])
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: default_bind(),
            ntfy_url: None,
            tailscale: TailscaleConfig::default(),
            native: NativeConfig::default(),
            agents: default_agents(),
        }
    }
}

impl Config {
    /// Loads the config, writing the defaults on first run so they're easy to edit.
    pub fn load_or_init() -> Result<Self> {
        let path = config_dir().join("config.toml");
        if !path.exists() {
            let cfg = Config::default();
            write_private(&path, &toml::to_string_pretty(&cfg)?)?;
            return Ok(cfg);
        }
        let text = std::fs::read_to_string(&path)?;
        let mut cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        // The native agent is always available, even in configs written before it existed.
        cfg.agents.entry(NATIVE_AGENT.into()).or_insert_with(native_agent);
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        write_private(&config_dir().join("config.toml"), &toml::to_string_pretty(self)?)
    }
}

/// `OUTPOST_HOME` overrides both dirs (handy for running several daemons on one box).
fn home_override() -> Option<PathBuf> {
    std::env::var_os("OUTPOST_HOME").map(PathBuf::from)
}

pub fn config_dir() -> PathBuf {
    home_override()
        .unwrap_or_else(|| dirs::config_dir().expect("no config dir").join("outpost"))
}

pub fn data_dir() -> PathBuf {
    home_override()
        .map(|h| h.join("data"))
        .unwrap_or_else(|| dirs::data_dir().expect("no data dir").join("outpost"))
}

pub fn write_private(path: &std::path::Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    std::io::Write::write_all(&mut f, contents.as_bytes())?;
    Ok(())
}

/// The daemon's bearer token, generated on first use.
pub fn token() -> Result<String> {
    let path = config_dir().join("token");
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let t = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    write_private(&path, &t)?;
    Ok(t)
}

/// Client-side list of remote machines, `~/.config/outpost/hosts.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Hosts {
    #[serde(default)]
    pub hosts: BTreeMap<String, Host>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    /// Anything `ssh` accepts: an alias from ~/.ssh/config, user@host, …
    pub ssh: String,
    pub token: String,
    /// Direct URL on the tailnet; when set the hub skips the SSH tunnel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailnet_url: Option<String>,
    #[serde(default = "default_remote_port")]
    pub remote_port: u16,
    pub local_port: u16,
}

fn default_remote_port() -> u16 {
    DAEMON_PORT
}

impl Hosts {
    fn path() -> PathBuf {
        config_dir().join("hosts.toml")
    }

    pub fn load() -> Result<Self> {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Hosts::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> Result<()> {
        write_private(&Self::path(), &toml::to_string_pretty(self)?)
    }

    pub fn next_local_port(&self) -> u16 {
        (7501..).find(|p| !self.hosts.values().any(|h| h.local_port == *p)).unwrap()
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

pub fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) => dirs::home_dir().unwrap().join(rest.trim_start_matches('/')),
        None => PathBuf::from(path),
    }
}

/// Provider credentials stored on this host (`~/.config/outpost/credentials.toml`, 0600).
fn credentials_path() -> PathBuf {
    config_dir().join("credentials.toml")
}

fn load_credentials() -> BTreeMap<String, String> {
    std::fs::read_to_string(credentials_path()).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
}

pub fn set_credential(provider: &str, secret: &str) -> Result<()> {
    let mut creds = load_credentials();
    creds.insert(provider.to_string(), secret.to_string());
    write_private(&credentials_path(), &toml::to_string_pretty(&creds)?)
}

/// A provider's key: its env var first (e.g. ANTHROPIC_API_KEY), then the credentials file.
pub fn credential(provider: &str, env_var: Option<&str>) -> Option<String> {
    let default_env = format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"));
    let env = env_var.unwrap_or(&default_env);
    std::env::var(env).ok().filter(|k| !k.is_empty()).or_else(|| load_credentials().remove(provider))
}

pub fn credential_providers() -> Vec<String> {
    load_credentials().into_keys().collect()
}
