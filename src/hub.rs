//! `outpost ui`: the laptop side. Finds every reachable daemon – this machine, SSH hosts from
//! hosts.toml (through supervised tunnels), and outpost daemons on tailnet peers – and serves
//! the web UI in multi-host mode.

use crate::config::{self, Hosts, DAEMON_PORT};
use crate::server::static_asset;
use crate::tailscale;
use anyhow::Result;
use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

#[derive(Debug, Clone, Serialize)]
struct HubHost {
    name: String,
    url: String,
    token: String,
    transport: &'static str,
    discovered: bool,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone)]
struct Hub {
    hosts: Arc<Mutex<BTreeMap<String, HubHost>>>,
    reconnect: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
    http: reqwest::Client,
}

impl Hub {
    fn set(&self, name: &str, f: impl FnOnce(&mut HubHost)) {
        if let Some(h) = self.hosts.lock().unwrap().get_mut(name) {
            f(h);
        }
    }

    async fn ping(&self, base: &str) -> Option<Value> {
        let res = self.http.get(format!("{base}/api/ping")).send().await.ok()?;
        let v: Value = res.json().await.ok()?;
        (v["outpost"] == true).then_some(v)
    }
}

pub async fn run(port: u16, open: bool) -> Result<()> {
    let hub = Hub {
        hosts: Arc::default(),
        reconnect: Arc::default(),
        http: reqwest::Client::builder().timeout(Duration::from_secs(3)).build()?,
    };

    // This machine, if it runs a daemon too.
    let local_url = format!("http://127.0.0.1:{DAEMON_PORT}");
    if let Some(p) = hub.ping(&local_url).await {
        let name = p["host"].as_str().unwrap_or("local").to_string();
        hub.hosts.lock().unwrap().insert(
            name.clone(),
            HubHost {
                name,
                url: local_url,
                token: config::token()?,
                transport: "local",
                discovered: false,
                status: "connected",
                error: None,
            },
        );
    }

    for (name, h) in Hosts::load()?.hosts {
        // Prefer the tailnet: direct, no tunnel, survives the laptop changing networks.
        if let Some(url) = &h.tailnet_url {
            if hub.ping(url).await.is_some() {
                hub.hosts.lock().unwrap().insert(
                    name.clone(),
                    HubHost {
                        name,
                        url: url.clone(),
                        token: h.token.clone(),
                        transport: "tailscale",
                        discovered: false,
                        status: "connected",
                        error: None,
                    },
                );
                continue;
            }
        }
        hub.hosts.lock().unwrap().insert(
            name.clone(),
            HubHost {
                name: name.clone(),
                url: format!("http://127.0.0.1:{}", h.local_port),
                token: h.token.clone(),
                transport: "ssh",
                discovered: false,
                status: "connecting",
                error: None,
            },
        );
        let notify = Arc::new(Notify::new());
        hub.reconnect.lock().unwrap().insert(name.clone(), notify.clone());
        tokio::spawn(tunnel(hub.clone(), name, h, notify));
    }

    tokio::spawn(discover(hub.clone()));

    let app = Router::new()
        .route("/hub/hosts", get(list_hosts))
        .route("/hub/hosts/{name}/connect", post(connect))
        .fallback(static_asset)
        .layer(middleware::from_fn(local_only))
        .with_state(hub);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let url = format!("http://{addr}");
    println!("outpost hub on {url}");
    if open {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let _ = std::process::Command::new(opener).arg(&url).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
    axum::serve(listener, app).await?;
    Ok(())
}

/// The hub hands out tokens, so only same-origin pages on loopback may talk to it
/// (the Host check defeats DNS rebinding; no CORS headers means other origins can't read it).
async fn local_only(req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let name = host.rsplit_once(':').map_or(host, |(n, _)| n);
    if !matches!(name, "127.0.0.1" | "localhost" | "[::1]") {
        return (StatusCode::FORBIDDEN, "hub only answers on localhost").into_response();
    }
    next.run(req).await
}

async fn list_hosts(State(hub): State<Hub>) -> Json<Value> {
    let hosts: Vec<HubHost> = hub.hosts.lock().unwrap().values().cloned().collect();
    Json(json!(hosts))
}

async fn connect(State(hub): State<Hub>, Path(name): Path<String>) -> Json<Value> {
    if let Some(n) = hub.reconnect.lock().unwrap().get(&name) {
        n.notify_one();
    }
    Json(json!({}))
}

/// Keeps `ssh -L` up for one host, restarting it with backoff.
async fn tunnel(hub: Hub, name: String, host: config::Host, reconnect: Arc<Notify>) {
    let base = format!("http://127.0.0.1:{}", host.local_port);
    let mut backoff = Duration::from_secs(1);
    loop {
        hub.set(&name, |h| {
            h.status = "connecting";
            h.error = None;
        });
        let forward = format!("127.0.0.1:{}:127.0.0.1:{}", host.local_port, host.remote_port);
        let child = tokio::process::Command::new("ssh")
            .args(["-N", "-T", "-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=15"])
            .args(["-o", "ServerAliveCountMax=3", "-o", "BatchMode=yes", "-L", &forward, &host.ssh])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                hub.set(&name, |h| {
                    h.status = "error";
                    h.error = Some(format!("ssh: {e}"));
                });
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };
        let mut stderr = child.stderr.take().unwrap();

        // Wait for the forward to answer (or ssh to give up).
        let mut up = false;
        for _ in 0..40 {
            if hub.ping(&base).await.is_some() {
                up = true;
                break;
            }
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if up {
            backoff = Duration::from_secs(1);
            hub.set(&name, |h| h.status = "connected");
            tokio::select! {
                _ = child.wait() => {}
                _ = reconnect.notified() => { let _ = child.kill().await; }
            }
        } else {
            let _ = child.kill().await;
        }
        let mut err = String::new();
        let _ = tokio::time::timeout(Duration::from_millis(200), stderr.read_to_string(&mut err)).await;
        let err = err.trim().lines().last().unwrap_or("tunnel closed").to_string();
        hub.set(&name, |h| {
            h.status = "error";
            h.error = Some(if up { format!("disconnected: {err}") } else { err });
        });
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = reconnect.notified() => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Probes online tailnet peers for outpost daemons every 30s and keeps tailnet hosts' status fresh.
async fn discover(hub: Hub) {
    loop {
        if let Ok(peers) = tailscale::peers().await {
            for peer in peers.into_iter().filter(|p| p.online) {
                let via_ssh = hub.hosts.lock().unwrap().values().any(|h| h.transport == "ssh" && h.name == peer.host_name);
                if via_ssh {
                    continue;
                }
                let mut found = None;
                for scheme in ["https", "http"] {
                    let base = format!("{scheme}://{}:{DAEMON_PORT}", peer.dns_name);
                    if let Some(p) = hub.ping(&base).await {
                        found = Some(p["tailnet_url"].as_str().map(str::to_string).unwrap_or(base));
                        break;
                    }
                }
                let mut hosts = hub.hosts.lock().unwrap();
                match (found, hosts.values_mut().find(|h| h.url.contains(&peer.dns_name))) {
                    (Some(_), Some(h)) => {
                        h.status = "connected";
                        h.error = None;
                    }
                    (None, Some(h)) => {
                        h.status = "error";
                        h.error = Some("not answering on the tailnet".into());
                    }
                    (Some(url), None) => {
                        hosts.insert(
                            peer.host_name.clone(),
                            HubHost {
                                name: peer.host_name.clone(),
                                url,
                                token: String::new(),
                                transport: "tailscale",
                                discovered: true,
                                status: "connected",
                                error: None,
                            },
                        );
                    }
                    (None, None) => {}
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}
