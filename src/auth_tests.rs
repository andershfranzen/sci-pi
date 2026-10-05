//! Behavioral auth tests use isolated stores, real HTTP and raw WebSocket clients.
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Fixture {
    dir: std::path::PathBuf,
    state: AppState,
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
    client: reqwest::Client,
}

impl Fixture {
    async fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("sci-pi-server-auth-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(&dir.join("state.db")).unwrap());
        let session: crate::model::Session = serde_json::from_value(json!({
            "id": "session", "title": "Auth test", "agent": "sci-pi", "project": dir,
            "cwd": dir, "status": "stopped", "created_at": 1, "updated_at": 1,
        })).unwrap();
        store.save_session(&session).unwrap();
        let state = AppState {
            mgr: Manager::new(Config::default(), store, &dir).unwrap(),
            token: "test-admin-token".into(),
            devices: Arc::new(Devices::open(dir.join("devices.json")).unwrap()),
            tailnet: Arc::default(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
        });
        Self { dir, state, addr, task, client: reqwest::Client::builder().no_proxy().build().unwrap() }
    }
    fn url(&self, path: &str) -> String { format!("http://{}/api{path}", self.addr) }
    async fn pair(&self, name: &str) -> Value {
        let issued: Value = self.client.post(self.url("/auth/pair")).bearer_auth(&*self.state.token)
            .json(&json!({ "name": name })).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        let url = reqwest::Url::parse(issued["url"].as_str().unwrap()).unwrap();
        assert!(url.query().is_none());
        let code = url.fragment().unwrap().strip_prefix("pair=").unwrap();
        self.client.post(self.url("/auth/redeem")).header(header::ORIGIN, format!("http://{}", self.addr))
            .json(&json!({ "code": code })).send().await.unwrap().error_for_status().unwrap().json().await.unwrap()
    }
    async fn socket(&self, path: &str, token: &str) -> tokio::net::TcpStream {
        let mut stream = tokio::net::TcpStream::connect(self.addr).await.unwrap();
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token);
        let request = format!("GET /api{path} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: sci-pi, bearer.{encoded}\r\n\r\n", self.addr);
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(stream.read_u8().await.unwrap());
            assert!(headers.len() < 8192);
        }
        assert!(String::from_utf8(headers).unwrap().starts_with("HTTP/1.1 101"));
        stream
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
        self.state.mgr.kill_terminal("session");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn frame(stream: &mut tokio::net::TcpStream) -> (u8, Vec<u8>) {
    let first = stream.read_u8().await.unwrap();
    let second = stream.read_u8().await.unwrap();
    assert_eq!(second & 0x80, 0);
    let len = match second & 0x7f {
        126 => stream.read_u16().await.unwrap() as usize,
        127 => stream.read_u64().await.unwrap() as usize,
        n => n as usize,
    };
    assert!(len < 1024 * 1024);
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes).await.unwrap();
    (first & 0x0f, bytes)
}

async fn closes(stream: &mut tokio::net::TcpStream) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop { if frame(stream).await.0 == 8 { break; } }
    }).await.expect("revoked connection did not close");
}

async fn ping_alive(stream: &mut tokio::net::TcpStream) {
    let payload = b"alive";
    let mask = [1, 2, 3, 4];
    let mut bytes = vec![0x89, 0x80 | payload.len() as u8];
    bytes.extend(mask);
    bytes.extend(payload.iter().enumerate().map(|(i, b)| *b ^ mask[i % 4]));
    stream.write_all(&bytes).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (opcode, body) = frame(stream).await;
            assert_ne!(opcode, 8, "another device was disconnected");
            if opcode == 10 && body == payload { break; }
        }
    }).await.expect("other device did not respond");
}

#[tokio::test]
async fn pairing_is_admin_only_local_and_one_time() {
    let f = Fixture::new().await;
    assert_eq!(f.client.post(f.url("/auth/pair")).json(&json!({})).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let issued = f.client.post(f.url("/auth/pair")).bearer_auth(&*f.state.token)
        .json(&json!({ "name": "Laptop" })).send().await.unwrap();
    assert_eq!(issued.headers()[header::CACHE_CONTROL], "no-store");
    let issued: Value = issued.json().await.unwrap();
    let url = reqwest::Url::parse(issued["url"].as_str().unwrap()).unwrap();
    let code = url.fragment().unwrap().strip_prefix("pair=").unwrap();
    let body = json!({ "code": code });
    assert_eq!(f.client.post(f.url("/auth/redeem")).header(header::ORIGIN, "https://hostile.example")
        .json(&body).send().await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(f.client.post(f.url("/auth/redeem")).header(header::HOST, "hostile.example")
        .header(header::ORIGIN, "http://hostile.example").json(&body).send().await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(f.client.post(f.url("/auth/redeem")).json(&body).send().await.unwrap().status(), StatusCode::FORBIDDEN);
    let response = f.client.post(f.url("/auth/redeem")).header(header::ORIGIN, format!("http://{}", f.addr))
        .json(&body).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let paired: Value = response.json().await.unwrap();
    let token = paired["token"].as_str().unwrap();
    assert_eq!(f.client.post(f.url("/auth/redeem")).header(header::ORIGIN, format!("http://{}", f.addr))
        .json(&body).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(f.client.get(f.url("/auth/devices")).bearer_auth(token).send().await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(f.client.post(f.url("/auth/pair")).bearer_auth(token).json(&json!({})).send().await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(f.client.get(f.url("/info")).bearer_auth(token).send().await.unwrap().status(), StatusCode::OK);
    for secret in [token, &*f.state.token] {
        assert_eq!(f.client.get(format!("{}?token={secret}", f.url("/info"))).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    }
    assert_eq!(f.client.get(f.url("/info")).bearer_auth("unknown").send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let listed: Value = f.client.get(f.url("/auth/devices")).bearer_auth(&*f.state.token)
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(listed[0]["name"], "Laptop");
    assert!(listed[0].get("token_hash").is_none());
    assert_eq!(f.client.delete(f.url("/auth/me")).bearer_auth(token).send().await.unwrap().status(), StatusCode::OK);
    assert_eq!(f.client.get(f.url("/info")).bearer_auth(token).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(f.client.get(f.url("/info")).bearer_auth(&*f.state.token).send().await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn revocation_closes_both_socket_kinds_only_for_that_device() {
    let f = Fixture::new().await;
    let a = f.pair("A").await;
    let b = f.pair("B").await;
    let mut a_api = f.socket("/ws", a["token"].as_str().unwrap()).await;
    let mut a_terminal = f.socket("/sessions/session/terminal", a["token"].as_str().unwrap()).await;
    let mut b_api = f.socket("/ws", b["token"].as_str().unwrap()).await;
    let mut b_terminal = f.socket("/sessions/session/terminal", b["token"].as_str().unwrap()).await;
    let path = format!("/auth/devices/{}", a["device"]["id"].as_str().unwrap());
    assert_eq!(f.client.delete(f.url(&path)).bearer_auth(b["token"].as_str().unwrap()).send().await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(f.client.delete(f.url(&path)).bearer_auth(&*f.state.token).send().await.unwrap().status(), StatusCode::OK);
    closes(&mut a_api).await;
    closes(&mut a_terminal).await;
    ping_alive(&mut b_api).await;
    ping_alive(&mut b_terminal).await;
    assert_eq!(f.client.get(f.url("/info")).bearer_auth(a["token"].as_str().unwrap()).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(f.client.get(f.url("/info")).bearer_auth(b["token"].as_str().unwrap()).send().await.unwrap().status(), StatusCode::OK);
}
