//! Local one-time pairing and independently revocable, hash-only browser credentials.
use anyhow::{Context, Result};
use axum::http::{header, HeaderMap};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

pub const PAIR_TTL: u64 = 120;

#[derive(Clone)]
pub struct Identity {
    pub admin: bool,
    pub device_id: Option<String>,
    pub revoked: CancellationToken,
}

impl Identity {
    pub fn admin() -> Self {
        Self { admin: true, device_id: None, revoked: CancellationToken::new() }
    }
    pub fn tailnet() -> Self {
        Self { admin: false, device_id: None, revoked: CancellationToken::new() }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub created_at: u64,
    // Only the digest is written to disk. It is never returned by the API.
    #[serde(skip_serializing_if = "String::is_empty")]
    token_hash: String,
}

impl Device {
    pub fn public(&self) -> serde_json::Value {
        serde_json::json!({ "id": self.id, "name": self.name, "created_at": self.created_at })
    }
}

struct Pending {
    expires_at: u64,
    name: String,
}

struct Inner {
    devices: BTreeMap<String, Device>,
    codes: BTreeMap<String, Pending>,
    cancellations: BTreeMap<String, CancellationToken>,
}

pub struct Devices {
    path: PathBuf,
    inner: Mutex<Inner>,
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex(&bytes))
}

fn hash(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 15) as usize] as char);
    }
    text
}

impl Devices {
    pub fn open(path: PathBuf) -> Result<Self> {
        let devices: BTreeMap<String, Device> = match std::fs::read_to_string(&path) {
            Ok(text) => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
                serde_json::from_str(&text).context("reading device credential store")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        for (id, device) in &devices {
            anyhow::ensure!(id == &device.id && device.token_hash.len() == 64 && device.token_hash.bytes().all(|b| b.is_ascii_hexdigit()), "invalid device credential store");
        }
        let cancellations = devices.keys().map(|id| (id.clone(), CancellationToken::new())).collect();
        Ok(Self { path, inner: Mutex::new(Inner { devices, codes: BTreeMap::new(), cancellations }) })
    }

    pub fn authenticate(&self, token: &str) -> Option<Identity> {
        if token.is_empty() { return None; }
        let digest = hash(token);
        let inner = self.inner.lock();
        let device = inner.devices.values().find(|d| d.token_hash == digest)?;
        Some(Identity { admin: false, device_id: Some(device.id.clone()), revoked: inner.cancellations.get(&device.id)?.clone() })
    }

    pub fn issue(&self, name: String, at: u64) -> Result<(String, u64)> {
        anyhow::ensure!(name.chars().count() <= 100, "device name is too long");
        let code = secret()?;
        let expires_at = at.saturating_add(PAIR_TTL);
        let mut inner = self.inner.lock();
        inner.codes.retain(|_, p| p.expires_at > at);
        anyhow::ensure!(inner.codes.len() < 100, "too many pending pairings");
        inner.codes.insert(hash(&code), Pending { expires_at, name });
        Ok((code, expires_at))
    }

    pub fn redeem(&self, code: &str, at: u64) -> Result<(String, Device)> {
        let digest = hash(code);
        let mut inner = self.inner.lock();
        inner.codes.retain(|_, p| p.expires_at > at);
        let pending = inner.codes.get(&digest).context("pairing code is invalid, expired, or already used")?;
        let token = secret()?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let device = Device { id: id.clone(), name: if pending.name.trim().is_empty() { "Browser".into() } else { pending.name.clone() }, created_at: at, token_hash: hash(&token) };
        inner.devices.insert(id.clone(), device.clone());
        if let Err(e) = persist(&self.path, &inner.devices) {
            inner.devices.remove(&id);
            return Err(e);
        }
        inner.codes.remove(&digest);
        inner.cancellations.insert(id, CancellationToken::new());
        Ok((token, device))
    }

    pub fn list(&self) -> Vec<serde_json::Value> {
        self.inner.lock().devices.values().map(Device::public).collect()
    }

    pub fn revoke(&self, id: &str) -> Result<bool> {
        let mut inner = self.inner.lock();
        let Some(device) = inner.devices.remove(id) else { return Ok(false) };
        if let Err(e) = persist(&self.path, &inner.devices) {
            inner.devices.insert(id.to_string(), device);
            return Err(e);
        }
        if let Some(cancel) = inner.cancellations.remove(id) { cancel.cancel(); }
        Ok(true)
    }
}

fn persist(path: &Path, devices: &BTreeMap<String, Device>) -> Result<()> {
    let parent = path.parent().context("device store has no parent")?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".devices-{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        serde_json::to_writer(&mut file, devices)?;
        file.sync_all()?;
        let directory = std::fs::File::open(parent)?;
        std::fs::rename(&tmp, path)?;
        // Once renamed, keep memory and disk in agreement even if directory fsync fails.
        if let Err(e) = directory.sync_all() {
            tracing::warn!("device store directory sync failed: {e}");
        }
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&tmp); }
    result
}

/// Pairing never trusts forwarded headers: require the actual socket to be local,
/// a loopback Host, and (for browser redemption) an exactly same-origin Origin.
pub fn local_origin(peer: SocketAddr, headers: &HeaderMap, require_origin: bool) -> Result<String> {
    anyhow::ensure!(peer.ip().is_loopback(), "pairing is only available on loopback");
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).context("missing Host")?;
    let base = format!("http://{host}");
    let url = reqwest::Url::parse(&base).context("invalid Host")?;
    anyhow::ensure!(url.username().is_empty() && url.password().is_none() && url.path() == "/" && url.query().is_none() && url.fragment().is_none(), "invalid Host");
    let name = url.host_str().context("invalid Host")?;
    let local = name == "localhost" || name == "127.0.0.1" || name == "[::1]";
    anyhow::ensure!(local, "pairing requires a loopback Host");
    let origin = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok());
    if require_origin { anyhow::ensure!(origin.is_some(), "pairing requires Origin"); }
    if let Some(origin) = origin {
        anyhow::ensure!(origin == base, "pairing requires the same local Origin");
    }
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Fixture { path: PathBuf, devices: Arc<Devices> }
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("sci-pi-auth-{}", uuid::Uuid::new_v4().simple()));
            let path = dir.join("devices.json");
            Self { devices: Arc::new(Devices::open(path.clone()).unwrap()), path }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(self.path.parent().unwrap()); }
    }

    #[test]
    fn expiry_and_one_time() {
        let f = Fixture::new();
        let (expired, end) = f.devices.issue("Expired".into(), 100).unwrap();
        assert!(f.devices.redeem(&expired, end).is_err());
        let (code, _) = f.devices.issue("Laptop".into(), 100).unwrap();
        let (token, _) = f.devices.redeem(&code, 101).unwrap();
        assert!(f.devices.authenticate(&token).is_some());
        assert!(f.devices.redeem(&code, 101).is_err());
    }

    #[test]
    fn concurrent_redemption_has_one_winner() {
        let f = Fixture::new();
        let (code, _) = f.devices.issue("Laptop".into(), 100).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles: Vec<_> = (0..2).map(|_| {
            let devices = f.devices.clone(); let code = code.clone(); let barrier = barrier.clone();
            std::thread::spawn(move || { barrier.wait(); devices.redeem(&code, 101).is_ok() })
        }).collect();
        barrier.wait();
        assert_eq!(handles.into_iter().filter_map(|h| h.join().ok()).filter(|b| *b).count(), 1);
        assert_eq!(f.devices.list().len(), 1);
    }

    #[test]
    fn persistent_hash_only_and_independent_revocation() {
        let f = Fixture::new();
        let (a, _) = f.devices.issue("A".into(), 100).unwrap();
        let (b, _) = f.devices.issue("B".into(), 100).unwrap();
        let (a_token, a_device) = f.devices.redeem(&a, 101).unwrap();
        let (b_token, _) = f.devices.redeem(&b, 101).unwrap();
        let a_identity = f.devices.authenticate(&a_token).unwrap();
        let b_identity = f.devices.authenticate(&b_token).unwrap();
        let contents = std::fs::read_to_string(&f.path).unwrap();
        for secret in [&a_token, &b_token, &a, &b] { assert!(!contents.contains(secret)); }
        assert_eq!(std::fs::metadata(&f.path).unwrap().permissions().mode() & 0o777, 0o600);
        let reopened = Devices::open(f.path.clone()).unwrap();
        assert!(reopened.authenticate(&a_token).is_some());
        assert!(reopened.authenticate("unknown").is_none());
        assert!(f.devices.revoke(&a_device.id).unwrap());
        assert!(a_identity.revoked.is_cancelled());
        assert!(!b_identity.revoked.is_cancelled());
        assert!(f.devices.authenticate(&a_token).is_none());
        assert!(f.devices.authenticate(&b_token).is_some());
        assert!(Devices::open(f.path.clone()).unwrap().authenticate(&a_token).is_none());
    }

    #[test]
    fn rejects_hostile_origin_host_and_remote_peer() {
        let peer = "127.0.0.1:5678".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:7433".parse().unwrap());
        headers.insert(header::ORIGIN, "http://127.0.0.1:7433".parse().unwrap());
        assert!(local_origin(peer, &headers, true).is_ok());
        assert!(local_origin("100.64.0.1:5678".parse().unwrap(), &headers, true).is_err());
        headers.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(local_origin(peer, &headers, true).is_err());
        headers.insert(header::HOST, "evil.example".parse().unwrap());
        headers.insert(header::ORIGIN, "http://evil.example".parse().unwrap());
        assert!(local_origin(peer, &headers, true).is_err());
        headers.insert(header::HOST, "localhost:7433".parse().unwrap());
        headers.remove(header::ORIGIN);
        assert!(local_origin(peer, &headers, true).is_err());
        assert!(local_origin(peer, &headers, false).is_ok());
    }
}
