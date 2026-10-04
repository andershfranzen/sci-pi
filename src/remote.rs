//! `outpost add <ssh-host>`: install this very binary on a remote machine as a systemd user
//! service, then remember the host (and its tailnet URL, if it has one) for the hub.

use crate::config::{self, Config, Host, Hosts, DAEMON_PORT};
use crate::tailscale;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const INSTALL: &str = r#"
set -e
mkdir -p ~/.local/bin ~/.config/systemd/user ~/.config/outpost
mv -f ~/.local/bin/outpost.new ~/.local/bin/outpost
# systemd user services get a bare PATH; capture the login shell's so npx/claude/codex resolve.
"${SHELL:-bash}" -lic 'printf "\nPATH=%s\n" "$PATH"' 2>/dev/null </dev/null | grep '^PATH=' | tail -1 > ~/.config/outpost/env || true
if [ -n "$1" ]; then ~/.local/bin/outpost tailscale-allow "$1"; fi
cat > ~/.config/systemd/user/outpost.service <<'UNIT'
[Unit]
Description=outpost coding-agent daemon
After=network-online.target

[Service]
EnvironmentFile=-%h/.config/outpost/env
ExecStart=%h/.local/bin/outpost serve
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
UNIT
systemctl --user daemon-reload
systemctl --user enable outpost >/dev/null 2>&1
systemctl --user restart outpost
if ! loginctl enable-linger "$USER" 2>/dev/null; then
  echo "warning: couldn't enable lingering; outpost will stop when you log out. Fix: sudo loginctl enable-linger $USER" >&2
fi
~/.local/bin/outpost local-info
"#;

async fn ssh(target: &str, cmd: &str) -> Result<String> {
    let out = Command::new("ssh").args(["-o", "BatchMode=yes", target, cmd]).output().await?;
    if !out.status.success() {
        bail!("ssh {target} `{cmd}`: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub async fn add(target: &str, name: Option<String>) -> Result<()> {
    let name = name.unwrap_or_else(|| target.rsplit('@').next().unwrap_or(target).to_string());

    println!("→ checking {target}");
    let remote = ssh(target, "uname -sm").await?;
    let local = format!("{} {}", if cfg!(target_os = "linux") { "Linux" } else { std::env::consts::OS }, std::env::consts::ARCH);
    if remote != local {
        bail!("{target} is `{remote}` but this binary is `{local}`; cross-platform installs aren't supported yet");
    }

    println!("→ uploading outpost");
    ssh(target, "mkdir -p ~/.local/bin").await?;
    let exe = std::env::current_exe()?;
    let status = Command::new("scp")
        .args(["-q", "-o", "BatchMode=yes"])
        .arg(&exe)
        .arg(format!("{target}:.local/bin/outpost.new"))
        .status()
        .await?;
    if !status.success() {
        bail!("scp failed");
    }

    println!("→ installing systemd user service");
    let login = tailscale::local_login().await.unwrap_or_default();
    let mut child = Command::new("ssh")
        .args(["-o", "BatchMode=yes", target, "bash", "-s", "--", &shell_quote(&login)])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(INSTALL.as_bytes()).await?;
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!("remote install failed");
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let info: Value = stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str(l).ok())
        .context("daemon didn't report back after install")?;

    let mut hosts = Hosts::load()?;
    let local_port = hosts.hosts.get(&name).map(|h| h.local_port).unwrap_or_else(|| hosts.next_local_port());
    let tailnet_url = info["tailnet_url"].as_str().map(str::to_string);
    hosts.hosts.insert(
        name.clone(),
        Host {
            ssh: target.to_string(),
            token: info["token"].as_str().unwrap_or_default().to_string(),
            tailnet_url: tailnet_url.clone(),
            remote_port: DAEMON_PORT,
            local_port,
        },
    );
    hosts.save()?;

    println!("✓ {name} is running outpost {}", info["version"].as_str().unwrap_or("?"));
    match tailnet_url {
        Some(url) => {
            println!("  tailnet: {url}");
            if !login.is_empty() {
                println!("  {login} can open it from any tailnet device without a token");
            }
        }
        None => println!("  reached over SSH (no Tailscale on {name})"),
    }
    println!("  open the UI with: outpost ui");
    Ok(())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Runs on the remote right after install: waits for the daemon, prints what the client needs.
pub async fn local_info() -> Result<()> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(2)).build()?;
    let url = format!("http://127.0.0.1:{DAEMON_PORT}/api/ping");
    // Leave time for tailnet setup (cert fetch) before reporting the tailnet URL.
    let mut ping: Option<Value> = None;
    for i in 0..60 {
        if let Ok(res) = http.get(&url).send().await {
            if let Ok(v) = res.json::<Value>().await {
                let has_tailnet = !v["tailnet_url"].is_null();
                ping = Some(v);
                if has_tailnet || i > 30 || tailscale::self_node().await.is_none() {
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let Some(ping) = ping else { bail!("outpost daemon didn't start; see: journalctl --user -u outpost") };
    let out = serde_json::json!({
        "token": config::token()?,
        "version": ping["version"],
        "tailnet_url": ping["tailnet_url"],
    });
    println!("{out}");
    Ok(())
}

pub fn tailscale_allow(login: &str) -> Result<()> {
    let mut cfg = Config::load_or_init()?;
    if !cfg.tailscale.allow.iter().any(|a| a.eq_ignore_ascii_case(login)) {
        cfg.tailscale.allow.push(login.to_string());
        cfg.save()?;
    }
    Ok(())
}
