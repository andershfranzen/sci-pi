//! The daemon's HTTP + WebSocket API (docs/PROTOCOL.md) and the embedded web UI.

use crate::config::{self, expand_tilde, Config};
use crate::model::WsMsg;
use crate::session::{CreateReq, Manager};
use crate::store::Store;
use crate::tailscale::{self, Gate};
use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tower_http::cors::{Any, CorsLayer};

#[derive(rust_embed::RustEmbed)]
#[folder = "web/dist"]
pub struct Assets;

#[derive(Clone)]
struct AppState {
    mgr: Arc<Manager>,
    token: Arc<str>,
    tailnet: Arc<RwLock<Option<Tailnet>>>,
}

#[derive(Clone)]
struct Tailnet {
    url: String,
    gate: Arc<Gate>,
}

/// The authenticated caller's tailnet login, when they came in over Tailscale without a token.
#[derive(Clone)]
struct Viewer(Option<String>);

pub async fn serve() -> Result<()> {
    let cfg = Config::load_or_init()?;
    let data_dir = config::data_dir();
    std::fs::create_dir_all(&data_dir)?;
    let store = Arc::new(Store::open(&data_dir.join("outpost.db"))?);
    let token: Arc<str> = config::token()?.into();
    let mgr = Manager::new(cfg.clone(), store, &data_dir)?;
    let state = AppState { mgr, token, tailnet: Arc::default() };
    let app = router(state.clone());

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!("listening on http://{}", cfg.bind);
    if cfg.tailscale.enabled {
        tokio::spawn(tailnet_listeners(cfg.clone(), app.clone(), state.tailnet.clone()));
    }
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

/// Waits for tailscaled (it may come up after us at boot), then listens on every tailnet IP,
/// with a Tailscale-issued cert when the host is allowed to fetch one.
async fn tailnet_listeners(cfg: Config, app: Router, slot: Arc<RwLock<Option<Tailnet>>>) {
    let node = loop {
        if let Some(node) = tailscale::self_node().await {
            break node;
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    };
    let port = cfg.tailscale.port;
    let tls = if node.can_cert {
        match tailscale::cert_pair(&node.dns_name).await {
            Ok((cert, key)) => match axum_server::tls_rustls::RustlsConfig::from_pem(cert, key).await {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!("tailscale cert unusable: {e}");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(
                    "no tailscale cert ({e:#}); serving plain HTTP on the tailnet. \
                     For HTTPS run: sudo tailscale set --operator=$USER"
                );
                None
            }
        }
    } else {
        None
    };
    let scheme = if tls.is_some() { "https" } else { "http" };
    let url = format!("{scheme}://{}:{port}", node.dns_name);
    let gate = Arc::new(Gate::new(cfg.tailscale.allow.clone(), node.owner.clone()));
    if gate.allowed().is_empty() {
        tracing::warn!(
            "tailnet: this node is tagged and tailscale.allow is empty, so tailnet callers need the token"
        );
    }
    *slot.write().unwrap() = Some(Tailnet { url: url.clone(), gate });
    tracing::info!("tailnet: {url}");

    for ip in node.ips {
        let addr = SocketAddr::new(ip, port);
        let svc = app.clone().into_make_service_with_connect_info::<SocketAddr>();
        let tls = tls.clone();
        tokio::spawn(async move {
            let res = match tls {
                Some(tls) => axum_server::bind_rustls(addr, tls).serve(svc).await,
                None => axum_server::bind(addr).serve(svc).await,
            };
            if let Err(e) = res {
                tracing::warn!("tailnet listener {addr}: {e}");
            }
        });
    }

    // Tailscale certs last 90 days; refresh daily.
    if let Some(tls) = tls {
        loop {
            tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
            match tailscale::cert_pair(&node.dns_name).await {
                Ok((cert, key)) => {
                    if let Err(e) = tls.reload_from_pem(cert, key).await {
                        tracing::warn!("reloading tailscale cert: {e}");
                    }
                }
                Err(e) => tracing::warn!("refreshing tailscale cert: {e:#}"),
            }
        }
    }
}

fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/info", get(info))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/sessions/{id}/events", get(events))
        .route("/sessions/{id}/prompt", post(prompt))
        .route("/sessions/{id}/cancel", post(cancel))
        .route("/sessions/{id}/permission", post(permission))
        .route("/sessions/{id}/mode", post(set_mode))
        .route("/sessions/{id}/stop", post(stop))
        .route("/sessions/{id}/diff", get(diff))
        .route("/inbox", get(inbox))
        .route("/fs/list", get(fs_list))
        .route("/ws", get(ws))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .route("/ping", get(ping));
    Router::new()
        .nest("/api", api)
        .fallback(static_asset)
        .layer(CorsLayer::new().allow_origin(Any).allow_headers(Any).allow_methods(Any))
        .with_state(state)
}

async fn auth(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
    mut req: Request,
    next: Next,
) -> Response {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let query_token = uri.query().and_then(|q| {
        q.split('&').find_map(|kv| kv.strip_prefix("token=")).map(str::to_string)
    });
    let token_ok = [bearer, query_token].into_iter().flatten().any(|t| !t.is_empty() && *t == *st.token);
    let gate = st.tailnet.read().unwrap().as_ref().map(|t| t.gate.clone());
    let viewer = match gate {
        Some(gate) => gate.check(peer).await,
        None => None,
    };
    if !token_ok && viewer.is_none() {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" }))).into_response();
    }
    req.extensions_mut().insert(Viewer(viewer));
    next.run(req).await
}

struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = format!("{:#}", self.0);
        let code = if msg == "no such session" { StatusCode::NOT_FOUND } else { StatusCode::BAD_REQUEST };
        (code, Json(json!({ "error": msg }))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

type ApiResult = std::result::Result<Json<Value>, ApiError>;

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

async fn ping(State(st): State<AppState>) -> Json<Value> {
    let tailnet_url = st.tailnet.read().unwrap().as_ref().map(|t| t.url.clone());
    Json(json!({
        "outpost": true,
        "version": env!("CARGO_PKG_VERSION"),
        "host": hostname(),
        "tailnet_url": tailnet_url,
    }))
}

async fn info(State(st): State<AppState>, Extension(viewer): Extension<Viewer>) -> ApiResult {
    let agents: Vec<Value> =
        st.mgr.cfg.agents.iter().map(|(id, a)| json!({ "id": id, "name": a.name })).collect();
    let tailnet_url = st.tailnet.read().unwrap().as_ref().map(|t| t.url.clone());
    Ok(Json(json!({
        "host": hostname(),
        "version": env!("CARGO_PKG_VERSION"),
        "home": dirs::home_dir().map(|h| h.to_string_lossy().into_owned()),
        "agents": agents,
        "last_event_id": st.mgr.store.last_event_id()?,
        "tailnet_url": tailnet_url,
        "viewer": viewer.0,
    })))
}

async fn list_sessions(State(st): State<AppState>) -> ApiResult {
    Ok(Json(serde_json::to_value(st.mgr.list())?))
}

async fn create_session(State(st): State<AppState>, Json(req): Json<CreateReq>) -> ApiResult {
    Ok(Json(serde_json::to_value(st.mgr.create(req).await?)?))
}

async fn get_session(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let s = st.mgr.get(&id).ok_or_else(|| anyhow::anyhow!("no such session"))?;
    Ok(Json(serde_json::to_value(s)?))
}

#[derive(Deserialize)]
struct DeleteQuery {
    remove_worktree: Option<String>,
}

async fn delete_session(State(st): State<AppState>, Path(id): Path<String>, Query(q): Query<DeleteQuery>) -> ApiResult {
    let remove = matches!(q.remove_worktree.as_deref(), Some("1" | "true"));
    st.mgr.delete(&id, remove).await?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct After {
    after: Option<i64>,
}

async fn events(State(st): State<AppState>, Path(id): Path<String>, Query(q): Query<After>) -> ApiResult {
    let events = st.mgr.store.events_after(q.after.unwrap_or(0), Some(&id), i64::MAX)?;
    Ok(Json(serde_json::to_value(events)?))
}

#[derive(Deserialize)]
struct PromptReq {
    text: String,
}

async fn prompt(State(st): State<AppState>, Path(id): Path<String>, Json(req): Json<PromptReq>) -> ApiResult {
    if req.text.trim().is_empty() {
        return Err(anyhow::anyhow!("empty prompt").into());
    }
    let queued = st.mgr.prompt(&id, req.text)?;
    Ok(Json(json!({ "queued": queued })))
}

async fn cancel(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    st.mgr.cancel(&id)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct PermissionReq {
    request_id: String,
    option_id: Option<String>,
}

async fn permission(State(st): State<AppState>, Path(id): Path<String>, Json(req): Json<PermissionReq>) -> ApiResult {
    st.mgr.permission(&id, req.request_id, req.option_id)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct ModeReq {
    mode: String,
}

async fn set_mode(State(st): State<AppState>, Path(id): Path<String>, Json(req): Json<ModeReq>) -> ApiResult {
    st.mgr.set_mode(&id, req.mode)?;
    Ok(Json(json!({})))
}

async fn stop(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    st.mgr.stop(&id)?;
    Ok(Json(json!({})))
}

async fn diff(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let s = st.mgr.get(&id).ok_or_else(|| anyhow::anyhow!("no such session"))?;
    Ok(Json(crate::git::diff(std::path::Path::new(&s.cwd), s.base_commit.as_deref()).await?))
}

async fn inbox(State(st): State<AppState>) -> ApiResult {
    Ok(Json(Value::Array(st.mgr.inbox()?)))
}

#[derive(Deserialize)]
struct FsQuery {
    path: Option<String>,
}

async fn fs_list(Query(q): Query<FsQuery>) -> ApiResult {
    let path = expand_tilde(q.path.as_deref().unwrap_or("~"));
    let path = path.canonicalize()?;
    let mut entries: Vec<Value> = std::fs::read_dir(&path)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            let p = e.path();
            Some(json!({ "name": name, "path": p.to_string_lossy(), "is_git": p.join(".git").exists() }))
        })
        .collect();
    entries.sort_by_key(|e| e["name"].as_str().unwrap_or_default().to_lowercase());
    Ok(Json(json!({
        "path": path.to_string_lossy(),
        "parent": path.parent().map(|p| p.to_string_lossy().into_owned()),
        "entries": entries,
    })))
}

async fn ws(State(st): State<AppState>, Query(q): Query<After>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| async move {
        if let Err(e) = stream_events(st, socket, q.after.unwrap_or(0)).await {
            tracing::debug!("websocket closed: {e:#}");
        }
    })
}

/// Replays the log after `after`, then forwards live messages. Subscribing before replaying
/// (and skipping already-sent ids) means nothing falls in the gap between the two.
async fn stream_events(st: AppState, mut socket: WebSocket, after: i64) -> Result<()> {
    let mut live = st.mgr.tx.subscribe();
    let mut last = after;
    loop {
        let batch = st.mgr.store.events_after(last, None, 1000)?;
        if batch.is_empty() {
            break;
        }
        for event in batch {
            last = event.id;
            socket.send(Message::Text(serde_json::to_string(&WsMsg::Event { event })?.into())).await?;
        }
    }
    let mut keepalive = tokio::time::interval(Duration::from_secs(25));
    loop {
        tokio::select! {
            msg = live.recv() => {
                let msg = match msg {
                    Ok(m) => m,
                    // Fell too far behind: drop the socket so the client reconnects with `after`.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => return Ok(()),
                    Err(e) => return Err(e.into()),
                };
                if let WsMsg::Event { event } = &msg {
                    if event.id <= last {
                        continue;
                    }
                    last = event.id;
                }
                socket.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Ok(()),
                _ => {}
            },
            _ = keepalive.tick() => socket.send(Message::Ping(Vec::new().into())).await?,
        }
    }
}

pub async fn static_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let (path, file) = match Assets::get(path).filter(|_| !path.is_empty()) {
        Some(f) => (path, f),
        None => match Assets::get("index.html") {
            Some(f) => ("index.html", f),
            None => return (StatusCode::NOT_FOUND, "web UI not built (cd web && bun run build)").into_response(),
        },
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let cache = if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    ([(header::CONTENT_TYPE, mime.as_ref()), (header::CACHE_CONTROL, cache)], file.data).into_response()
}
