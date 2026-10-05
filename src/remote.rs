//! `sci-pi add <ssh-host>`: install this very binary on a remote machine as a systemd user
//! service, then remember the host (and its tailnet URL, if it has one) for the hub.

use crate::config::{self, Config, Host, Hosts, DAEMON_PORT};
use crate::tailscale;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const INSTALL: &str = r#"
set -e
mkdir -p ~/.local/bin ~/.config/systemd/user ~/.config/sci-pi
mv -f ~/.local/bin/sci-pi.new ~/.local/bin/sci-pi
# systemd user services get a bare PATH; capture the login shell's so npx/claude/codex resolve.
"${SHELL:-bash}" -lic 'printf "\nPATH=%s\n" "$PATH"' 2>/dev/null </dev/null | grep '^PATH=' | tail -1 > ~/.config/sci-pi/env || true
if [ -n "$1" ]; then ~/.local/bin/sci-pi tailscale-allow "$1"; fi
cat > ~/.config/systemd/user/sci-pi.service <<'UNIT'
[Unit]
Description=sci-pi coding-agent daemon
After=network-online.target

[Service]
EnvironmentFile=-%h/.config/sci-pi/env
ExecStart=%h/.local/bin/sci-pi serve
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
UNIT
systemctl --user daemon-reload
systemctl --user enable sci-pi >/dev/null 2>&1
systemctl --user restart sci-pi
if ! loginctl enable-linger "$USER" 2>/dev/null; then
  echo "warning: couldn't enable lingering; sci-pi will stop when you log out. Fix: sudo loginctl enable-linger $USER" >&2
fi
~/.local/bin/sci-pi local-info
"#;

async fn ssh(target: &str, cmd: &str) -> Result<String> {
    let out = Command::new("ssh").args(["-o", "BatchMode=yes", target, cmd]).output().await?;
    if !out.status.success() {
        bail!("ssh {target} `{cmd}`: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub async fn add(target: &str, name: Option<String>, force: bool) -> Result<()> {
    let name = name.unwrap_or_else(|| target.rsplit('@').next().unwrap_or(target).to_string());

    println!("→ checking {target}");
    // Inspect with the uploaded binary below: an older daemon client may swallow auth failures.
    let remote = ssh(target, "uname -sm").await?;
    let local = format!("{} {}", if cfg!(target_os = "linux") { "Linux" } else { std::env::consts::OS }, std::env::consts::ARCH);
    if remote != local {
        bail!("{target} is `{remote}` but this binary is `{local}`; cross-platform installs aren't supported yet");
    }

    println!("→ uploading sci-pi");
    ssh(target, "mkdir -p ~/.local/bin").await?;
    let exe = std::env::current_exe()?;
    let status = Command::new("scp")
        .args(["-q", "-o", "BatchMode=yes"])
        .arg(&exe)
        .arg(format!("{target}:.local/bin/sci-pi.new"))
        .status()
        .await?;
    if !status.success() {
        bail!("scp failed");
    }
    let inspection = Command::new("ssh")
        .args(["-o", "BatchMode=yes", target, "~/.local/bin/sci-pi.new deployment-check"])
        .output().await?;
    if !inspection.status.success() && !force {
        bail!(
            "cannot safely restart {name}:\n{}{}\nwait for active turns or resolve inspection failures; --force explicitly bypasses this check",
            String::from_utf8_lossy(&inspection.stdout).trim(),
            String::from_utf8_lossy(&inspection.stderr).trim(),
        );
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
    verify_identity(&info, &crate::build_info::info())?;

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

    println!("✓ {name} is running sci-pi {}", build_label(&info));
    match tailnet_url {
        Some(url) => {
            println!("  tailnet: {url}");
            if !login.is_empty() {
                println!("  {login} can open it from any tailnet device without a token");
            }
        }
        None => println!("  reached over SSH (no Tailscale on {name})"),
    }
    println!("  open the UI with: sci-pi ui");
    Ok(())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Runs on the remote right after install: waits for the daemon, prints what the client needs.
pub async fn local_info() -> Result<()> {
    let http = deployment_client()?;
    let token = config::token()?;
    let base = local_base()?;
    let expected = crate::build_info::info();
    let mut ping = wait_ready(&http, &base, &token, &expected).await?;
    // The tailnet listener starts after the HTTP listener. Preserve remote discovery without
    // accepting stale/unhealthy build responses while certificates are being fetched.
    let tailnet_present = tokio::time::timeout(Duration::from_secs(2), tailscale::self_node()).await
        .ok().flatten().is_some();
    if tailnet_present {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while ping["tailnet_url"].is_null() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
            ping = read_ping(&http, &base, &token).await?;
            verify_identity(&ping, &expected)?;
        }
    }
    let out = serde_json::json!({
        "token": token,
        "version": ping["version"],
        "build": ping["build"],
        "tailnet_url": ping["tailnet_url"],
    });
    println!("{out}");
    Ok(())
}

/// Upgrades every remembered host to this binary.
pub async fn update_all(force: bool) -> Result<()> {
    let hosts = Hosts::load()?;
    if hosts.hosts.is_empty() {
        bail!("no hosts yet; add one with `sci-pi add <ssh-host>`");
    }
    let mut failed = vec![];
    for (name, h) in hosts.hosts {
        if let Err(e) = add(&h.ssh, Some(name.clone()), force).await {
            eprintln!("✗ {name}: {e:#}");
            failed.push(name);
        }
    }
    if !failed.is_empty() {
        bail!("{} host(s) not updated: {}", failed.len(), failed.join(", "));
    }
    Ok(())
}

/// Exit code 3 (with the titles on stdout) when any local session is mid-turn.
pub async fn busy() -> Result<()> {
    let http = deployment_client()?;
    let sessions = inspect_daemon(&http, &local_base()?, &config::token()?).await?;
    let mut active = sessions.iter().filter(|s| s.active()).peekable();
    if active.peek().is_none() {
        return Ok(());
    }
    for session in active {
        println!("  {} ({:?})", session.title, session.status);
    }
    std::process::exit(3);
}

#[derive(Deserialize)]
struct DeploymentSession {
    title: String,
    status: crate::model::Status,
    #[serde(default)]
    queued: usize,
    #[serde(default)]
    pending_permissions: usize,
}

impl DeploymentSession {
    fn active(&self) -> bool {
        self.queued > 0 || self.pending_permissions > 0 || matches!(self.status,
            crate::model::Status::Starting | crate::model::Status::Running | crate::model::Status::AwaitingPermission)
    }
}

fn deployment_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(2)).build()?)
}

fn local_base() -> Result<String> {
    let mut bind: SocketAddr = Config::load_or_init()?.bind.parse().context("invalid daemon bind address")?;
    if bind.ip().is_unspecified() {
        bind.set_ip(match bind.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        });
    }
    Ok(format!("http://{bind}"))
}

async fn read_ping(http: &reqwest::Client, base: &str, token: &str) -> Result<Value> {
    let ping: Value = http.get(format!("{base}/api/ping")).bearer_auth(token).send().await?
        .error_for_status().context("daemon health request rejected")?.json().await.context("invalid daemon health JSON")?;
    if ping["scipi"] != true || ping["version"].as_str().is_none() {
        bail!("invalid daemon health response");
    }
    Ok(ping)
}

async fn inspect_daemon(http: &reqwest::Client, base: &str, token: &str) -> Result<Vec<DeploymentSession>> {
    read_ping(http, base, token).await?;
    read_sessions(http, base, token).await
}

async fn read_sessions(http: &reqwest::Client, base: &str, token: &str) -> Result<Vec<DeploymentSession>> {
    http.get(format!("{base}/api/sessions")).bearer_auth(token).send().await?
        .error_for_status().context("daemon session inspection rejected")?.json().await.context("invalid daemon session inspection")
}

fn verify_identity(ping: &Value, expected: &Value) -> Result<()> {
    let actual = ping.get("build").context("daemon did not report a build identity")?;
    for field in ["id", "commit", "dirty", "built_at", "version"] {
        if actual.get(field).is_none() || actual.get(field) != expected.get(field) {
            bail!("daemon build mismatch ({field}); expected {}, received {}",
                expected["id"].as_str().unwrap_or("unknown"),
                actual["id"].as_str().unwrap_or("unknown"));
        }
    }
    Ok(())
}

fn build_label(ping: &Value) -> String {
    let build = &ping["build"];
    let commit = build["commit"].as_str().unwrap_or("unknown revision");
    let id = build["id"].as_str().unwrap_or("unknown");
    format!("{} ({}{commit}{}, build {})", build["version"].as_str().unwrap_or("?"),
        if build["commit"].is_null() { "" } else { "commit " },
        if build["dirty"] == true { "+dirty" } else { "" }, &id[..12.min(id.len())])
}

async fn wait_ready(http: &reqwest::Client, base: &str, token: &str, expected: &Value) -> Result<Value> {
    let wait = async {
        // Every unsuccessful probe supplies the useful failure reason.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let last_error = match read_ping(http, base, token).await {
                Ok(ping) => match verify_identity(&ping, expected) {
                    Ok(()) => match read_sessions(http, base, token).await {
                        Ok(_) => return Ok(ping),
                        Err(error) => error,
                    },
                    Err(error) => error,
                },
                Err(error) => error,
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(last_error.context("daemon failed readiness/build verification; see: journalctl --user -u sci-pi"));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(18), wait).await
        .context("daemon readiness timed out; see: journalctl --user -u sci-pi")?
}

fn connection_refused(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionRefused))
}

async fn service_inactive() -> Result<bool> {
    let output = Command::new("systemctl").args(["--user", "show", "sci-pi.service", "--property=LoadState,ActiveState"]).output().await?;
    let stdout = std::str::from_utf8(&output.stdout).context("invalid systemctl state output")?;
    let load = stdout.lines().find_map(|line| line.strip_prefix("LoadState="));
    let active = stdout.lines().find_map(|line| line.strip_prefix("ActiveState="));
    let inspected = output.status.success() || (output.status.code() == Some(1) && load == Some("not-found"));
    Ok(inspected && matches!(load, Some("loaded" | "not-found")) && matches!(active, Some("inactive" | "failed")))
}

fn ensure_idle(sessions: &[DeploymentSession]) -> Result<()> {
    let mut active = sessions.iter().filter(|session| session.active()).peekable();
    if active.peek().is_none() {
        return Ok(());
    }
    let mut titles = String::new();
    for session in active {
        if !titles.is_empty() {
            titles.push_str(", ");
        }
        titles.push_str(&session.title);
    }
    bail!("agents have active or queued turns: {titles}");
}

/// Preflight used locally and by the freshly uploaded remote binary. No replacement happens here.
pub async fn deployment_check() -> Result<()> {
    let http = deployment_client()?;
    match inspect_daemon(&http, &local_base()?, &config::token()?).await {
        Ok(sessions) => ensure_idle(&sessions),
        Err(error) => {
            // Only an explicitly refused loopback connection AND an inactive user unit prove
            // this is a first install/stopped daemon. Auth, malformed JSON, timeouts and unknown
            // systemd state must never be interpreted as an idle service.
            if connection_refused(&error) && service_inactive().await? {
                Ok(())
            } else {
                Err(error.context("cannot determine whether daemon is safe to restart"))
            }
        }
    }
}

/// Installs this binary as the local daemon: ~/.local/bin/sci-pi under a systemd user unit,
/// same as `add` does on a remote host. Re-run after a build to upgrade and restart.
pub async fn install_local(force: bool) -> Result<()> {
    if !force {
        deployment_check().await.context("refusing unsafe install; resolve inspection failure or explicitly pass --force")?;
    }
    let bin = dirs::home_dir().context("home directory unavailable")?.join(".local/bin");
    std::fs::create_dir_all(&bin)?;
    std::fs::copy(std::env::current_exe()?, bin.join("sci-pi.new"))?;
    // The script's local-info checks identity but contains credentials: never print its stdout.
    let mut child = Command::new("bash").args(["-s", "--", ""]).stdin(Stdio::piped()).stdout(Stdio::null()).spawn()?;
    child.stdin.take().unwrap().write_all(INSTALL.as_bytes()).await?;
    if !child.wait().await?.success() {
        bail!("install failed; see: journalctl --user -u sci-pi");
    }
    // Verify from the invoking binary too, rather than trusting the binary just installed.
    let http = deployment_client()?;
    let ping = wait_ready(&http, &local_base()?, &config::token()?, &crate::build_info::info()).await?;
    println!("✓ sci-pi {} installed as a systemd user service (logs: journalctl --user -u sci-pi -f)", build_label(&ping));
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, StatusCode};
    use axum::{Router, routing::get};
    use serde_json::json;

    struct DaemonFixture {
        base: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for DaemonFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn daemon(ping_status: StatusCode, ping: String, sessions_status: StatusCode, sessions: String) -> DaemonFixture {
        let app = Router::new()
            .route("/api/ping", get(move || {
                let ping = ping.clone();
                async move { (ping_status, ping) }
            }))
            .route("/api/sessions", get(move |headers: HeaderMap| {
                let sessions = sessions.clone();
                async move {
                    if headers.get("authorization").and_then(|value| value.to_str().ok()) != Some("Bearer test-token") {
                        return (StatusCode::UNAUTHORIZED, String::new());
                    }
                    (sessions_status, sessions)
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        DaemonFixture { base, task }
    }

    fn healthy_ping() -> String {
        json!({"scipi": true, "version": env!("CARGO_PKG_VERSION"), "build": crate::build_info::info()}).to_string()
    }

    #[tokio::test]
    async fn active_and_queued_turns_block_restart_over_http() {
        for (status, queued, pending_permissions, title) in [
            ("starting", 0, 0, "work in progress"), ("running", 0, 0, "work in progress"),
            ("awaiting_permission", 0, 0, "work in progress"), ("idle", 1, 0, "work in progress"),
            ("idle", 0, 1, "approval pending"), ("running", 0, 0, ""),
        ] {
            let fixture = daemon(StatusCode::OK, healthy_ping(), StatusCode::OK,
                json!([{"title": title, "status": status, "queued": queued, "pending_permissions": pending_permissions}]).to_string()).await;
            let sessions = inspect_daemon(&deployment_client().unwrap(), &fixture.base, "test-token").await.unwrap();
            let error = ensure_idle(&sessions).unwrap_err();
            assert!(error.to_string().contains("active or queued turns"));
        }
    }

    #[tokio::test]
    async fn idle_restart_requires_authentic_parseable_inspection() {
        let fixture = daemon(StatusCode::OK, healthy_ping(), StatusCode::OK,
            json!([{"title": "finished", "status": "idle"}]).to_string()).await;
        let http = deployment_client().unwrap();
        assert!(inspect_daemon(&http, &fixture.base, "wrong-token").await.is_err());
        let sessions = inspect_daemon(&http, &fixture.base, "test-token").await.unwrap();
        ensure_idle(&sessions).unwrap();
        let ping = wait_ready(&http, &fixture.base, "test-token", &crate::build_info::info()).await.unwrap();
        verify_identity(&ping, &crate::build_info::info()).unwrap();
    }

    #[tokio::test]
    async fn uncertain_http_inspection_never_becomes_idle() {
        let cases = [
            (StatusCode::UNAUTHORIZED, healthy_ping(), StatusCode::OK, "[]"),
            (StatusCode::OK, healthy_ping(), StatusCode::UNAUTHORIZED, "[]"),
            (StatusCode::OK, "<html>proxy error</html>".to_owned(), StatusCode::OK, "[]"),
            (StatusCode::OK, "{}".to_owned(), StatusCode::OK, "[]"),
            (StatusCode::OK, healthy_ping(), StatusCode::OK, "{bad json"),
            (StatusCode::OK, healthy_ping(), StatusCode::OK, "{}"),
            (StatusCode::OK, healthy_ping(), StatusCode::OK, "[{}]"),
            (StatusCode::OK, healthy_ping(), StatusCode::OK, r#"[{"title":"unknown state","status":"future_state"}]"#),
        ];
        for (ping_status, ping, sessions_status, sessions) in cases {
            let fixture = daemon(ping_status, ping, sessions_status, sessions.to_owned()).await;
            assert!(inspect_daemon(&deployment_client().unwrap(), &fixture.base, "test-token").await.is_err());
        }
    }

    #[tokio::test]
    async fn matching_package_version_does_not_prove_build_identity() {
        let ping = json!({
            "scipi": true,
            "version": env!("CARGO_PKG_VERSION"),
            "build": {
                "id": "some-other-binary",
                "version": env!("CARGO_PKG_VERSION"),
                "commit": null,
                "dirty": false,
                "built_at": 0,
            },
        });
        let fixture = daemon(StatusCode::OK, ping.to_string(), StatusCode::OK, "[]".to_owned()).await;
        let actual = read_ping(&deployment_client().unwrap(), &fixture.base, "test-token").await.unwrap();
        assert!(verify_identity(&actual, &crate::build_info::info()).is_err());
        assert!(verify_identity(&json!({"version": env!("CARGO_PKG_VERSION")}), &crate::build_info::info()).is_err());
    }
}
