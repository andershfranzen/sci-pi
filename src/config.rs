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
        ("claude".into(), agent("Claude Code", &["npx", "-y", "@agentclientprotocol/claude-agent-acp"])),
        ("codex".into(), agent("Codex", &["npx", "-y", "@zed-industries/codex-acp"])),
        ("opencode".into(), agent("OpenCode", &["opencode", "acp"])),
    ])
}

impl Default for Config {
    fn default() -> Self {
        Config { bind: default_bind(), ntfy_url: None, tailscale: TailscaleConfig::default(), agents: default_agents() }
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
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
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
