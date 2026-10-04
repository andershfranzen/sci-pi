//! Native Tailscale integration through the local tailscaled's LocalAPI (unix socket):
//! discover our tailnet identity, listen on the tailnet, fetch HTTPS certs, and identify callers
//! with `whois` so tailnet members don't need a token.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const SOCKETS: &[&str] = &["/var/run/tailscale/tailscaled.sock", "/run/tailscale/tailscaled.sock"];

fn socket_path() -> Option<&'static str> {
    SOCKETS.iter().copied().find(|p| std::path::Path::new(p).exists())
}

/// One HTTP/1.0 request to the LocalAPI (1.0 so the body is never chunked).
async fn localapi(path: &str) -> Result<(u16, Vec<u8>)> {
    let sock = socket_path().ok_or_else(|| anyhow!("tailscaled socket not found"))?;
    let mut stream = tokio::time::timeout(Duration::from_secs(5), UnixStream::connect(sock)).await??;
    let req = format!(
        "GET /localapi/v0/{path} HTTP/1.0\r\nHost: local-tailscaled.sock\r\nSec-Tailscale: localapi\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(90), stream.read_to_end(&mut buf)).await??;
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow!("bad LocalAPI response"))?;
    let head = String::from_utf8_lossy(&buf[..split]);
    let code = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    Ok((code, buf[split + 4..].to_vec()))
}

async fn localapi_json(path: &str) -> Result<Value> {
    let (code, body) = localapi(path).await?;
    if code != 200 {
        bail!("LocalAPI {path}: HTTP {code}: {}", String::from_utf8_lossy(&body).trim());
    }
    Ok(serde_json::from_slice(&body)?)
}

#[derive(Debug, Clone)]
pub struct SelfNode {
    /// MagicDNS name without the trailing dot, e.g. "homelab.tail1234.ts.net".
    pub dns_name: String,
    pub ips: Vec<IpAddr>,
    /// Owner's login; None for tagged nodes.
    pub owner: Option<String>,
    pub can_cert: bool,
}

#[derive(Debug, Clone)]
pub struct Peer {
    pub host_name: String,
    pub dns_name: String,
    pub online: bool,
}

/// Our own node, or None when tailscaled isn't installed or isn't logged in.
pub async fn self_node() -> Option<SelfNode> {
    let st = localapi_json("status").await.ok()?;
    if st["BackendState"] != "Running" {
        return None;
    }
    let me = &st["Self"];
    let tagged = me["Tags"].as_array().is_some_and(|t| !t.is_empty());
    let owner = if tagged {
        None
    } else {
        me["UserID"]
            .as_u64()
            .and_then(|uid| st["User"][uid.to_string()]["LoginName"].as_str())
            .map(str::to_string)
    };
    let dns_name = me["DNSName"].as_str()?.trim_end_matches('.').to_string();
    let can_cert = st["CertDomains"].as_array().is_some_and(|d| d.iter().any(|x| x == dns_name.as_str()));
    let ips = me["TailscaleIPs"]
        .as_array()?
        .iter()
        .filter_map(|ip| ip.as_str()?.parse().ok())
        .collect();
    Some(SelfNode { dns_name, ips, owner, can_cert })
}

/// The login of the user running this machine's tailscale (for writing remote allow-lists).
pub async fn local_login() -> Option<String> {
    let st = localapi_json("status").await.ok()?;
    let uid = st["Self"]["UserID"].as_u64()?;
    st["User"][uid.to_string()]["LoginName"].as_str().map(str::to_string)
}

pub async fn peers() -> Result<Vec<Peer>> {
    let st = localapi_json("status").await?;
    let Some(peers) = st["Peer"].as_object() else { return Ok(vec![]) };
    Ok(peers
        .values()
        .filter_map(|p| {
            Some(Peer {
                host_name: p["HostName"].as_str()?.to_string(),
                dns_name: p["DNSName"].as_str()?.trim_end_matches('.').to_string(),
                online: p["Online"].as_bool().unwrap_or(false),
            })
        })
        .collect())
}

/// PEM cert chain and key for our MagicDNS name. Needs HTTPS enabled on the tailnet and,
/// for non-root daemons, `tailscale set --operator=$USER`.
pub async fn cert_pair(domain: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    let (code, body) = localapi(&format!("cert/{domain}?type=pair")).await?;
    if code != 200 {
        bail!("{}", String::from_utf8_lossy(&body).trim());
    }
    let marker = b"-----BEGIN CERTIFICATE-----";
    let at = body.windows(marker.len()).position(|w| w == marker).ok_or_else(|| anyhow!("no certificate in response"))?;
    Ok((body[at..].to_vec(), body[..at].to_vec()))
}

pub fn is_tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (64..128).contains(&o[1]) // 100.64.0.0/10 (CGNAT range Tailscale uses)
        }
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// Decides whether a tailnet caller may use the API without a token.
pub struct Gate {
    allow: Vec<String>,
    cache: Mutex<HashMap<IpAddr, (Instant, Option<String>)>>,
}

impl Gate {
    /// `allow` empty means "the node's owner" (only meaningful for untagged nodes).
    pub fn new(mut allow: Vec<String>, owner: Option<String>) -> Self {
        if allow.is_empty() {
            allow.extend(owner);
        }
        Gate { allow, cache: Mutex::default() }
    }

    pub fn allowed(&self) -> &[String] {
        &self.allow
    }

    /// The caller's login if it's an allowed tailnet user.
    pub async fn check(&self, peer: SocketAddr) -> Option<String> {
        if !is_tailnet_ip(peer.ip()) {
            return None;
        }
        let login = self.whois(peer).await?;
        self.allow.iter().any(|a| a.eq_ignore_ascii_case(&login)).then_some(login)
    }

    async fn whois(&self, peer: SocketAddr) -> Option<String> {
        if let Some((at, login)) = self.cache.lock().unwrap().get(&peer.ip()) {
            if at.elapsed() < Duration::from_secs(60) {
                return login.clone();
            }
        }
        let login = match localapi_json(&format!("whois?addr={peer}")).await {
            Ok(w) => {
                let tagged = w["Node"]["Tags"].as_array().is_some_and(|t| !t.is_empty());
                if tagged { None } else { w["UserProfile"]["LoginName"].as_str().map(str::to_string) }
            }
            Err(e) => {
                tracing::debug!("whois {peer}: {e:#}");
                None
            }
        };
        self.cache.lock().unwrap().insert(peer.ip(), (Instant::now(), login.clone()));
        login
    }
}
