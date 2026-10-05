//! sci-pi's own agent, served over the Agent Client Protocol on stdio (`sci-pi acp`). The
//! daemon runs it like any other ACP agent; editors that speak ACP can use it too.
//!
//! The loop: stream a model response (forwarding text, thinking and tool starts as session
//! updates), run the requested tools (read-only ones in parallel, edits and commands one at a time
//! behind the permission policy), append the results, repeat. Every step is saved, so a session
//! resumes with its full history after the process restarts.

pub mod llm;
mod diagnostics;
pub mod models;
pub mod prompt;
pub mod tools;

use crate::config::{self, Config, NativeConfig, ProviderKind};
use anyhow::{anyhow, bail, Context, Result};
use llm::{Delta, Opts, Provider, Request};
use models::ModelInfo;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const MAX_STEPS: usize = 500;

const MODES: &[(&str, &str, &str)] = &[
    ("default", "Ask", "Ask before editing files or running commands"),
    ("acceptEdits", "Accept edits", "Edit freely, ask before running commands"),
    ("plan", "Plan", "Read-only: investigate and propose, change nothing"),
    ("bypassPermissions", "Autonomous", "Never ask"),
];

#[derive(Clone, Serialize, Deserialize)]
struct Settings {
    cwd: PathBuf,
    model: String,
    effort: String,
    #[serde(default)]
    fast: bool,
    mode: String,
    /// Tool kinds ("edit", "execute") the user allowed for the rest of the session.
    #[serde(default)]
    always_allow: Vec<String>,
    #[serde(default)]
    cost_usd: f64,
    /// Context the main conversation used on its last request.
    #[serde(default)]
    context_tokens: u64,
}

struct Session {
    id: String,
    settings: Mutex<Settings>,
    /// Anthropic-format messages. Locked for the duration of a turn.
    history: tokio::sync::Mutex<Vec<Value>>,
    cancel: Mutex<Option<CancellationToken>>,
}

struct Server {
    out: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    native: NativeConfig,
    /// Provider discovery timestamps and runtime metadata by selectable model ID.
    discovered: Mutex<HashMap<String, std::time::Instant>>,
    registry: Mutex<HashMap<String, ModelInfo>>,
    learned: Mutex<models::LearnedWindows>,
    endpoint_scopes: HashMap<String, [Option<EndpointScope>; 2]>,
    /// models.dev fallback, indexed by bare model ID.
    catalog: Mutex<HashMap<String, ModelInfo>>,
    discovery: tokio::sync::Mutex<()>,
    dir: PathBuf,
    http: reqwest::Client,
}

struct EndpointScope {
    id: String,
    url: String,
}

fn endpoint_scopes(native: &NativeConfig) -> HashMap<String, [Option<EndpointScope>; 2]> {
    use sha2::{Digest, Sha256};
    let make = |url: String, key: Option<&str>| {
        let canonical = diagnostics::endpoint(&url)?;
        let id = format!("{:x}", Sha256::digest(canonical.as_bytes()));
        let url = if key.filter(|key| !key.is_empty()).is_some_and(|key| canonical.contains(key)) {
            "[redacted]".into()
        } else { canonical };
        Some(EndpointScope { id, url })
    };
    let base = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into());
    let key = config::credential("anthropic", None);
    let mut scopes = HashMap::from([(String::new(), [make(format!("{}/v1/messages", base.trim_end_matches('/')), key.as_deref()), None])]);
    for (name, provider) in &native.providers {
        let base = provider.base_url.trim_end_matches('/');
        let key = config::credential(name, provider.api_key_env.as_deref());
        let pair = match provider.kind {
            ProviderKind::Anthropic => [make(format!("{base}/v1/messages"), key.as_deref()), None],
            ProviderKind::Openai => [None, make(format!("{base}/chat/completions"), key.as_deref())],
            ProviderKind::Cliproxy => [make(format!("{base}/v1/messages"), key.as_deref()), make(format!("{base}/v1/chat/completions"), key.as_deref())],
        };
        scopes.insert(name.clone(), pair);
    }
    scopes
}

pub async fn serve_stdio() -> Result<()> {
    let cfg = Config::load_or_init()?;
    anyhow::ensure!(cfg.native.compact_ratio.is_finite() && cfg.native.compact_ratio > 0.0
        && cfg.native.compact_ratio < 1.0, "native.compact_ratio must be between 0 and 1");
    let dir = config::data_dir().join("agent");
    std::fs::create_dir_all(&dir)?;
    let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });
    let server = Arc::new(Server {
        out,
        pending: Mutex::default(),
        next_id: AtomicU64::new(1),
        sessions: Mutex::default(),
        endpoint_scopes: endpoint_scopes(&cfg.native),
        native: cfg.native,
        discovered: Mutex::default(),
        registry: Mutex::default(),
        learned: Mutex::new(models::load_learned(&dir.join("learned-windows.json"))),
        catalog: Mutex::default(),
        discovery: tokio::sync::Mutex::new(()),
        dir,
        http: reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(30)).build()?,
    });

    let s = server.clone();
    tokio::spawn(async move {
        let catalog = models::models_dev(&s.http, &s.dir.join("models.json")).await;
        *s.catalog.lock() = catalog;
        s.discover_models().await;
        let sessions: Vec<_> = s.sessions.lock().values().cloned().collect();
        for session in sessions {
            {
                let mut settings = session.settings.lock();
                s.normalize_settings(&mut settings);
            }
            let _ = s.save_settings(&session);
            s.update(&session.id, json!({ "sessionUpdate": "config_option_update",
                "configOptions": s.config_options(&session.settings.lock()) }));
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let method = msg["method"].as_str().map(str::to_string);
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match (method, msg.get("id").cloned()) {
            (Some(method), Some(id)) => {
                let s = server.clone();
                tokio::spawn(async move {
                    let reply = match s.handle(&method, params).await {
                        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": format!("{e:#}") } }),
                    };
                    s.send(reply);
                });
            }
            (Some(method), None) => server.notification(&method, &params),
            (None, Some(id)) => {
                if let Some(tx) = id.as_u64().and_then(|id| server.pending.lock().remove(&id)) {
                    let _ = tx.send(msg.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            (None, None) => {}
        }
    }
    Ok(())
}

impl Server {
    fn send(&self, msg: Value) {
        let _ = self.out.send(msg.to_string());
    }

    fn update(&self, session_id: &str, update: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": session_id, "update": update } }));
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        rx.await.map_err(|_| anyhow!("client went away"))
    }

    fn notification(&self, method: &str, params: &Value) {
        if method == "session/cancel" {
            if let Some(s) = params["sessionId"].as_str().and_then(|id| self.session(id).ok()) {
                if let Some(t) = s.cancel.lock().as_ref() {
                    t.cancel();
                }
            }
        }
    }

    async fn handle(self: &Arc<Self>, method: &str, p: Value) -> Result<Value> {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": 1,
                "agentCapabilities": {
                    "loadSession": false,
                    "promptCapabilities": { "image": true, "embeddedContext": true },
                    "sessionCapabilities": { "resume": {} },
                },
                "agentInfo": { "name": "sci-pi", "title": "sci-pi", "version": crate::build_info::VERSION, "build": crate::build_info::info() },
                "authMethods": [],
            })),
            "session/new" => {
                let cwd = PathBuf::from(p["cwd"].as_str().ok_or_else(|| anyhow!("cwd is required"))?);
                self.discover_models().await;
                let mut settings = Settings {
                    cwd,
                    model: self.native.model.clone(),
                    effort: self.native.effort.clone(),
                    fast: false,
                    mode: "default".into(),
                    always_allow: vec![],
                    cost_usd: 0.0,
                    context_tokens: 0,
                };
                self.normalize_settings(&mut settings);
                let session = Arc::new(Session {
                    id: uuid::Uuid::new_v4().to_string(),
                    settings: Mutex::new(settings),
                    history: tokio::sync::Mutex::new(vec![]),
                    cancel: Mutex::new(None),
                });
                self.save_settings(&session)?;
                self.save_history(&session.id, &[])?;
                self.sessions.lock().insert(session.id.clone(), session.clone());
                self.advertise_commands(&session.id);
                Ok(self.session_info(&session, true))
            }
            "session/resume" | "session/load" => {
                let id = p["sessionId"].as_str().ok_or_else(|| anyhow!("sessionId is required"))?;
                let session = self.load(id)?;
                self.sessions.lock().insert(id.to_string(), session.clone());
                self.discover_models().await;
                {
                    let mut settings = session.settings.lock();
                    self.normalize_settings(&mut settings);
                }
                self.save_settings(&session)?;
                self.advertise_commands(id);
                Ok(self.session_info(&session, false))
            }
            "session/set_mode" => {
                let s = self.session(p["sessionId"].as_str().unwrap_or_default())?;
                self.set_option(&s, "mode", &p["modeId"])?;
                Ok(json!({}))
            }
            "session/set_config_option" => {
                let s = self.session(p["sessionId"].as_str().unwrap_or_default())?;
                self.set_option(&s, p["configId"].as_str().unwrap_or_default(), &p["value"])?;
                Ok(json!({ "configOptions": self.config_options(&s.settings.lock()) }))
            }
            "session/prompt" => {
                let s = self.session(p["sessionId"].as_str().unwrap_or_default())?;
                let stop = self.run_turn(&s, p["prompt"].as_array().cloned().unwrap_or_default()).await?;
                Ok(json!({ "stopReason": stop }))
            }
            other => bail!("method not found: {other}"),
        }
    }

    fn advertise_commands(&self, session_id: &str) {
        self.update(session_id, json!({ "sessionUpdate": "available_commands_update", "availableCommands": [
            { "name": "compact", "description": "Summarize the conversation so far to free up context" },
        ] }));
    }

    fn session(&self, id: &str) -> Result<Arc<Session>> {
        self.sessions.lock().get(id).cloned().ok_or_else(|| anyhow!("unknown session {id}"))
    }

    fn session_info(&self, s: &Session, with_id: bool) -> Value {
        let settings = s.settings.lock();
        let mut v = json!({
            "modes": {
                "currentModeId": settings.mode,
                "availableModes": MODES.iter().map(|(id, name, d)| json!({ "id": id, "name": name, "description": d })).collect::<Vec<_>>(),
            },
            "configOptions": self.config_options(&settings),
        });
        if with_id {
            v["sessionId"] = json!(s.id);
        }
        v
    }

    fn config_options(&self, s: &Settings) -> Value {
        let mut ids: Vec<_> = self.registry.lock().keys().cloned().collect();
        ids.sort();
        let choices: Vec<_> = ids.iter().map(|id| models::option(id, &self.info(id), id)).collect();
        let info = self.info(&s.model);
        let mut options = vec![
            json!({ "id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": s.mode,
                "options": MODES.iter().map(|(id, name, d)| json!({ "value": id, "name": name, "description": d })).collect::<Vec<_>>() }),
            json!({ "id": "model", "name": "Model", "category": "model", "type": "select",
                "currentValue": s.model, "options": choices,
                "metadata": { "info": info, "pay_per_token": self.pay_per_token(&s.model),
                    "endpoint": self.scope(&s.model).map(|(scope, _)| &scope.url),
                    "endpoint_id": self.scope(&s.model).map(|(scope, _)| &scope.id) } }),
        ];
        if !info.efforts.is_empty() {
            options.push(json!({ "id": "effort", "name": "Effort", "category": "thought_level",
                "type": "select", "currentValue": s.effort,
                "options": info.efforts.iter().map(|(value, description)| json!({
                    "value": value, "name": value, "description": description
                })).collect::<Vec<_>>() }));
        }
        if let Some(fast) = &info.fast {
            let description = match fast {
                models::Fast::AnthropicSpeed => None,
                models::Fast::ServiceTier { description, .. } => description.as_deref(),
            };
            options.push(json!({ "id": "fast", "name": "Fast", "type": "boolean",
                "currentValue": s.fast, "description": description }));
        }
        json!(options)
    }

    /// Discover metadata even for a configured model allowlist; cache successful fetches.
    async fn discover_models(&self) {
        let _discovery = self.discovery.lock().await;
        for (name, provider) in &self.native.providers {
            let fresh = self.discovered.lock().get(name).is_some_and(|at| at.elapsed().as_secs() < 300);
            if fresh {
                continue;
            }
            let key = config::credential(name, provider.api_key_env.as_deref());
            let base = provider.base_url.trim_end_matches('/');
            let result = match provider.kind {
                ProviderKind::Cliproxy => models::cliproxy_models(&self.http, base, key.as_deref()).await,
                ProviderKind::Anthropic => match key.as_deref() {
                    Some(key) => models::anthropic_models(&self.http, base, key, true).await,
                    None => Err(llm::no_key(name)),
                },
                ProviderKind::Openai => llm::list_models(&self.http, &format!("{base}/models"), key.as_deref()).await,
            };
            match result {
                Ok(entries) => {
                    let mut registry = self.registry.lock();
                    let prefix = format!("{name}/");
                    registry.retain(|id, _| !id.starts_with(&prefix));
                    for (id, info) in entries {
                        if provider.models.is_empty() || provider.models.contains(&id) {
                            registry.insert(format!("{name}/{id}"), info);
                        }
                    }
                    for id in &provider.models {
                        registry.entry(format!("{name}/{id}")).or_default();
                    }
                    self.discovered.lock().insert(name.clone(), std::time::Instant::now());
                }
                Err(error) => {
                    eprintln!("model discovery for {name} failed: {error:#}");
                    let mut registry = self.registry.lock();
                    for id in &provider.models {
                        registry.entry(format!("{name}/{id}")).or_default();
                    }
                }
            }
        }
        // Direct Anthropic models are fetched only with a configured API credential.
        if let Some(key) = config::credential("anthropic", None) {
            let fresh = self.discovered.lock().get("").is_some_and(|at| at.elapsed().as_secs() < 300);
            if !fresh {
                let base = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into());
                match models::anthropic_models(&self.http, &base, &key, false).await {
                    Ok(entries) => {
                        let mut registry = self.registry.lock();
                        for (id, mut info) in entries {
                            info.provider = Some("anthropic".into());
                            registry.insert(id, info);
                        }
                        self.discovered.lock().insert(String::new(), std::time::Instant::now());
                    }
                    Err(error) => eprintln!("Anthropic model discovery failed: {error:#}"),
                }
            }
        } else if crate::anthropic_auth::status().ok().flatten().is_some() {
            let catalog = self.catalog.lock();
            let mut registry = self.registry.lock();
            for (id, info) in catalog.iter().filter(|(_, info)| info.provider.as_deref() == Some("anthropic")) {
                registry.entry(id.clone()).or_insert_with(|| info.clone());
            }
        }
        if !self.native.model.is_empty() {
            let mut registry = self.registry.lock();
            let builtin = self.native.model.strip_prefix("anthropic/")
                .filter(|_| !self.native.providers.contains_key("anthropic"));
            if let Some(info) = builtin.and_then(|bare| registry.get(bare)).cloned() {
                registry.insert(self.native.model.clone(), info);
            } else {
                registry.entry(self.native.model.clone()).or_default();
            }
        }
    }

    fn pay_per_token(&self, model: &str) -> bool {
        let (provider, _) = model.split_once('/').unwrap_or(("anthropic", model));
        if let Some(config) = self.native.providers.get(provider) {
            config.pay_per_token.unwrap_or(config.kind != ProviderKind::Cliproxy)
        } else {
            config::credential("anthropic", None).is_some_and(|key| !key.starts_with("sk-ant-oat"))
        }
    }

    fn scope<'a>(&'a self, model: &'a str) -> Option<(&'a EndpointScope, &'a str)> {
        let (name, bare) = model.split_once('/').unwrap_or(("", model));
        let name = if name == "anthropic" && !self.native.providers.contains_key(name) { "" } else { name };
        let index = match self.native.providers.get(name).map(|provider| provider.kind) {
            Some(ProviderKind::Openai) => 1,
            Some(ProviderKind::Cliproxy) if !bare.starts_with("claude-") => 1,
            _ => 0,
        };
        Some((self.endpoint_scopes.get(name)?[index].as_ref()?, bare))
    }

    fn info(&self, model: &str) -> ModelInfo {
        let (_, bare) = model.split_once('/').unwrap_or(("", model));
        let mut info = {
            let registry = self.registry.lock();
            registry.get(model).or_else(|| registry.get(bare)).cloned().unwrap_or_default()
        };
        if let Some(fallback) = self.catalog.lock().get(bare) {
            info.fill_from(fallback);
        }
        if let Some(window) = self.native.context_windows.get(model).copied().filter(|w| *w > 0) {
            info.window = Some(window);
            info.provenance.insert("window".into(), models::MetadataSource::UserOverride);
        }
        if let Some(limit) = self.scope(model).and_then(|(scope, bare)| self.learned.lock().get(&scope.id, bare)) {
            if info.window.is_none_or(|window| limit <= window) {
                info.window = Some(limit);
                info.provenance.insert("window".into(), models::MetadataSource::LearnedOverflow);
            }
        }
        if !self.pay_per_token(model) {
            info.cost = None;
            info.provenance.retain(|field, _| !field.starts_with("cost."));
        }
        info
    }

    fn window(&self, model: &str) -> Option<u64> {
        self.info(model).window
    }

    fn normalize_settings(&self, settings: &mut Settings) {
        if settings.model.is_empty() {
            settings.model = self.registry.lock().keys().min().cloned().unwrap_or_default();
        }
        let info = self.info(&settings.model);
        let opts = Opts::from_info(&info, &settings.effort, settings.fast);
        settings.effort = opts.effort.unwrap_or_default().to_owned();
        if info.fast.is_none() {
            settings.fast = false;
        }
    }

    fn set_option(&self, session: &Session, id: &str, value: &Value) -> Result<()> {
        {
            let mut settings = session.settings.lock();
            match id {
                "mode" => {
                    let mode = value.as_str().context("mode must be a string")?;
                    anyhow::ensure!(MODES.iter().any(|m| m.0 == mode), "unsupported mode {mode}");
                    settings.mode = mode.to_owned();
                }
                "model" => {
                    let model = value.as_str().context("model must be a string")?;
                    anyhow::ensure!(self.registry.lock().contains_key(model), "unknown model {model}");
                    settings.model = model.to_owned();
                    settings.effort.clear();
                    settings.fast = false;
                    self.normalize_settings(&mut settings);
                }
                "effort" => {
                    let effort = value.as_str().context("effort must be a string")?;
                    anyhow::ensure!(self.info(&settings.model).efforts.iter().any(|e| e.0 == effort),
                        "unsupported effort {effort} for {}", settings.model);
                    settings.effort = effort.to_owned();
                }
                "fast" => {
                    anyhow::ensure!(self.info(&settings.model).fast.is_some(), "Fast is unavailable for {}", settings.model);
                    settings.fast = value.as_bool().context("fast must be a boolean")?;
                }
                _ => bail!("unsupported option {id}"),
            }
        }
        if id == "mode" {
            self.update(&session.id, json!({ "sessionUpdate": "current_mode_update", "currentModeId": value }));
        }
        self.save_settings(session)
    }

    // ── persistence ───────────────────────────────────────────────────────────

    fn save_settings(&self, s: &Session) -> Result<()> {
        let json = serde_json::to_vec(&*s.settings.lock())?;
        atomic_write(&self.dir.join(format!("{}.settings.json", s.id)), &json)
    }

    fn save_history(&self, id: &str, history: &[Value]) -> Result<()> {
        atomic_write(&self.dir.join(format!("{id}.json")), &serde_json::to_vec(history)?)
    }

    fn load(&self, id: &str) -> Result<Arc<Session>> {
        if let Ok(s) = self.session(id) {
            return Ok(s);
        }
        let settings: Settings = serde_json::from_slice(&std::fs::read(self.dir.join(format!("{id}.settings.json"))).context("no such session")?)?;
        let history: Vec<Value> = serde_json::from_slice(&std::fs::read(self.dir.join(format!("{id}.json")))?)?;
        Ok(Arc::new(Session {
            id: id.to_string(),
            settings: Mutex::new(settings),
            history: tokio::sync::Mutex::new(history),
            cancel: Mutex::new(None),
        }))
    }

    // ── the loop ──────────────────────────────────────────────────────────────

    async fn provider(&self, model: &str) -> Result<(Provider, String)> {
        let model = if !self.native.providers.contains_key("anthropic") {
            model.strip_prefix("anthropic/").unwrap_or(model)
        } else {
            model
        };
        if let Some((name, m)) = model.split_once('/') {
            let p = self.native.providers.get(name).ok_or_else(|| anyhow!("unknown provider `{name}` in model `{model}`"))?;
            let api_key = config::credential(name, p.api_key_env.as_deref());
            let base = p.base_url.trim_end_matches('/').to_string();
            let anthropic = |base_url: String| -> Result<Provider> {
                let api_key = api_key.clone().ok_or_else(|| llm::no_key(name))?;
                Ok(Provider::Anthropic { api_key, base_url, proxy: true })
            };
            let provider = match p.kind {
                ProviderKind::Openai => Provider::OpenAi { api_key: api_key.clone(), base_url: base },
                ProviderKind::Anthropic => anthropic(base)?,
                ProviderKind::Cliproxy if m.starts_with("claude-") => anthropic(base)?,
                ProviderKind::Cliproxy => Provider::OpenAi { api_key: api_key.clone(), base_url: format!("{base}/v1") },
            };
            return Ok((provider, m.to_string()));
        }
        if self.info(model).provider.as_deref() == Some("anthropic") {
            let api_key = crate::anthropic_auth::credential(&self.http).await?;
            let base_url = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into());
            return Ok((Provider::Anthropic { api_key, base_url, proxy: false }, model.to_string()));
        }
        bail!("don't know which provider serves `{model}` (use claude-* or <provider>/<model>)")
    }

    async fn run_turn(self: &Arc<Self>, s: &Arc<Session>, prompt: Vec<Value>) -> Result<&'static str> {
        let token = CancellationToken::new();
        *s.cancel.lock() = Some(token.clone());
        let mut history = s.history.lock().await;
        let cwd = s.settings.lock().cwd.clone();
        let system = prompt::system(&cwd);
        let tool_defs = main_tools();

        close_dangling_tool_uses(&mut history);
        // `/compact` is ours: summarize now instead of starting a turn.
        if prompt_text(&prompt).trim() == "/compact" {
            let model = s.settings.lock().model.clone();
            let _ = self.compact(s, &mut history, &system, &tool_defs, &model, &Sink::Main, false).await;
            return self.finish(s, &history, "end_turn");
        }
        let blocks = prompt_blocks(&prompt, &cwd);
        match history.last_mut() {
            // Results of interrupted tools are already a user message; add the prompt to it.
            Some(last) if last["role"] == "user" && last["content"].is_array() => last["content"].as_array_mut().unwrap().extend(blocks),
            _ => history.push(json!({ "role": "user", "content": blocks })),
        }
        self.save_history(&s.id, &history)?;
        let stop = self.run_loop(s, &mut history, &system, &tool_defs, &Sink::Main, &token).await;
        let saved = self.finish(s, &history, "end_turn");
        let stop = stop?;
        saved.map(|_| stop)
    }

    fn finish(&self, s: &Session, history: &[Value], stop: &'static str) -> Result<&'static str> {
        *s.cancel.lock() = None;
        self.save_history(&s.id, history)?;
        Ok(stop)
    }

    /// The agent loop, shared by the session and its subagents: stream a response, run its tool
    /// calls, repeat until the model stops. Compacts the history when it outgrows the budget.
    async fn run_loop(
        self: &Arc<Self>,
        s: &Arc<Session>,
        history: &mut Vec<Value>,
        system: &str,
        tool_defs: &[Value],
        sink: &Sink,
        token: &CancellationToken,
    ) -> Result<&'static str> {
        let mut context = if matches!(sink, Sink::Main) { s.settings.lock().context_tokens } else { 0 };
        let mut overflowed = false;
        for _ in 0..MAX_STEPS {
            let settings = s.settings.lock().clone();
            let model = sink.model().unwrap_or(&settings.model).to_string();
            if self.compact_threshold(&model).is_some_and(|threshold| context > threshold) {
                let _ = self.compact(s, history, system, tool_defs, &model, sink, true).await;
            }
            if token.is_cancelled() {
                return Ok("cancelled");
            }
            // Persist any rotated OAuth credentials before honoring cancellation.
            let (provider, api_model) = self.provider(&model).await?;
            if token.is_cancelled() {
                return Ok("cancelled");
            }
            let message_id = uuid::Uuid::new_v4().to_string();
            let (sid, this, main) = (s.id.clone(), self.clone(), matches!(sink, Sink::Main));
            let mut on = move |d: Delta| {
                match d {
                    Delta::Diagnostic(diagnostic) => this.update(&sid, json!({
                        "sessionUpdate": "request_diagnostic", "diagnostic": diagnostic
                    })),
                    _ if !main => {},
                    Delta::Text(t) => this.update(&sid, json!({ "sessionUpdate": "agent_message_chunk", "messageId": message_id, "content": { "type": "text", "text": t } })),
                    Delta::Thinking(t) => this.update(&sid, json!({ "sessionUpdate": "agent_thought_chunk", "messageId": message_id, "content": { "type": "text", "text": t } })),
                    Delta::ToolStart { id, name } => this.update(&sid, json!({
                        "sessionUpdate": "tool_call", "toolCallId": id, "title": name, "kind": tools::kind(&name),
                        "status": "pending", "rawInput": {}, "content": [],
                    })),
                }
            };
            let info = self.info(&model);
            let opts = Opts::from_info(&info, &settings.effort, settings.fast);
            let req = Request { model: &api_model, system, messages: history, tools: tool_defs, opts: &opts, session_id: &s.id, phase: "inference" };
            let resp = tokio::select! {
                r = provider.stream(&self.http, &req, &mut on) => r,
                _ = token.cancelled() => return Ok("cancelled"),
            };
            let resp = match resp {
                Ok(r) => r,
                Err(e) if e.is::<llm::StreamFailure>() => return Err(e),
                // The provider's real window is smaller than we thought: remember it, make room, retry once.
                Err(e) => match llm::overflow(&format!("{e:#}")) {
                    Some(limit) if !overflowed => {
                        overflowed = true;
                        if let (Some(limit), Some((scope, bare))) = (limit.filter(|limit| *limit > 0), self.scope(&model)) {
                            let mut learned = self.learned.lock();
                            learned.remember(&scope.id, bare, limit);
                            if let Err(error) = models::save_learned(&self.dir.join("learned-windows.json"), &learned) {
                                eprintln!("could not persist learned context limit: {error}");
                            }
                            drop(learned);
                            self.update(&s.id, json!({ "sessionUpdate": "config_option_update",
                                "configOptions": self.config_options(&s.settings.lock()) }));
                        }
                        if !self.compact(s, history, system, tool_defs, &model, sink, true).await {
                            shrink_old_tool_results(history);
                            self.compact(s, history, system, tool_defs, &model, sink, true).await;
                        }
                        context = 0;
                        continue;
                    }
                    _ => return Err(e),
                },
            };
            context = self.report_usage(s, &model, &resp.usage, sink);
            if !resp.content.is_empty() {
                history.push(json!({ "role": "assistant", "content": resp.content }));
                self.checkpoint(s, history, sink)?;
            }

            let calls: Vec<Value> = resp.content.iter().filter(|b| b["type"] == "tool_use").cloned().collect();
            if calls.is_empty() {
                if resp.stop_reason == "pause_turn" {
                    continue;
                }
                return Ok(match resp.stop_reason.as_str() {
                    "max_tokens" => "max_tokens",
                    "refusal" => "refusal",
                    _ => "end_turn",
                });
            }
            // A response cut off mid-tool-call may carry truncated input: never run those.
            let results = if matches!(resp.stop_reason.as_str(), "max_tokens" | "refusal") {
                calls.iter().map(|c| tool_result(c, "Not run: the response was cut off before this call was complete. Retry in smaller steps.", true)).collect()
            } else {
                self.run_tools(s, &settings, &calls, &resp.invalid_inputs, token, sink).await
            };
            history.push(json!({ "role": "user", "content": results }));
            self.checkpoint(s, history, sink)?;
            if token.is_cancelled() {
                return Ok("cancelled");
            }
        }
        Ok("max_turn_requests")
    }

    fn checkpoint(&self, s: &Session, history: &[Value], sink: &Sink) -> Result<()> {
        match sink {
            Sink::Main => self.save_history(&s.id, history),
            Sink::Sub { .. } => Ok(()), // subagent histories are ephemeral
        }
    }

    /// An explicit token threshold, otherwise a fraction of the known runtime window.
    fn compact_threshold(&self, model: &str) -> Option<u64> {
        self.native.compact_at_tokens.or_else(|| self.window(model)
            .map(|window| (window as f64 * self.native.compact_ratio) as u64))
    }

    /// Replaces the history with a summary. On failure the history is left as it was.
    #[allow(clippy::too_many_arguments)]
    async fn compact(
        self: &Arc<Self>,
        s: &Arc<Session>,
        history: &mut Vec<Value>,
        system: &str,
        tool_defs: &[Value],
        model: &str,
        sink: &Sink,
        mid_turn: bool,
    ) -> bool {
        let id = format!("compact-{}", uuid::Uuid::new_v4().simple());
        let title = match sink {
            Sink::Main => "Compacting the conversation".to_string(),
            Sink::Sub { label, .. } => format!("⤷ {label}: compacting"),
        };
        self.update(&s.id, json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "think", "status": "in_progress", "rawInput": {}, "content": [] }));
        let settings = s.settings.lock().clone();
        let info = self.info(model);
        let opts = Opts::from_info(&info, &settings.effort, settings.fast);
        let result = async {
            let (provider, api_model) = self.provider(model).await?;
            let req = Request { model: &api_model, system, messages: history, tools: tool_defs, opts: &opts, session_id: &s.id, phase: "compaction" };
            provider.compact(&self.http, &req, &mut |delta| {
                if let Delta::Diagnostic(diagnostic) = delta {
                    self.update(&s.id, json!({ "sessionUpdate": "request_diagnostic", "diagnostic": diagnostic }));
                }
            }).await
        }
        .await;
        match result {
            Ok(c) => {
                let before = history.len();
                *history = c.history;
                if mid_turn {
                    history.push(json!({ "role": "user", "content": [{ "type": "text",
                        "text": "[sci-pi: the conversation so far was compacted into the summary above to free context. Continue the task from where you left off.]" }] }));
                }
                let _ = self.checkpoint(s, history, sink);
                self.report_usage(s, model, &c.usage, &Sink::Sub { label: String::new(), model: None });
                let text = format!("Compacted {before} messages into a summary:\n\n{}", c.summary.trim());
                self.update(&s.id, json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed",
                    "content": [{ "type": "content", "content": { "type": "text", "text": text } }] }));
                true
            }
            Err(e) => {
                self.update(&s.id, json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "failed",
                    "content": [{ "type": "content", "content": { "type": "text", "text": format!("Compaction failed: {e:#}") } }] }));
                false
            }
        }
    }

    /// Adds a response's cost to the session; for the main loop, also reports context use.
    /// Returns the context size this response implies (what the next request will send).
    fn report_usage(&self, s: &Session, model: &str, usage: &Value, sink: &Sink) -> u64 {
        let n = |k: &str| usage[k].as_u64().unwrap_or(0);
        let (input, read, write, output) =
            (n("input_tokens"), n("cache_read_input_tokens"), n("cache_creation_input_tokens"), n("output_tokens"));
        let used = input + read + write + output;
        let cost = if self.pay_per_token(model) { models::cost(&self.info(model), usage) } else { None };
        let (total, context, main_model) = {
            let mut st = s.settings.lock();
            st.cost_usd += cost.unwrap_or(0.0);
            if matches!(sink, Sink::Main) {
                st.context_tokens = used;
            }
            (st.cost_usd, st.context_tokens, st.model.clone())
        };
        let _ = self.save_settings(s);
        let mut update = json!({ "sessionUpdate": "usage_update", "used": context });
        if let Some(window) = self.window(&main_model) {
            update["size"] = json!(window);
        }
        if self.pay_per_token(&main_model) && total > 0.0 {
            update["cost"] = json!({ "amount": total, "currency": "USD" });
        }
        self.update(&s.id, update);
        used
    }

    /// Runs one response's tool calls: consecutive calls that can't conflict (read-only tools,
    /// explore subagents) concurrently, everything else one at a time. Results keep call order.
    async fn run_tools(
        self: &Arc<Self>,
        s: &Arc<Session>,
        settings: &Settings,
        calls: &[Value],
        invalid: &HashMap<String, String>,
        token: &CancellationToken,
        sink: &Sink,
    ) -> Vec<Value> {
        let mut results = Vec::with_capacity(calls.len());
        let mut i = 0;
        while i < calls.len() {
            if concurrent(&calls[i]) {
                let mut j = i;
                while j < calls.len() && concurrent(&calls[j]) {
                    j += 1;
                }
                let batch = calls[i..j].iter().map(|c| self.run_tool(s, settings, c, invalid, token, sink));
                results.extend(futures::future::join_all(batch).await);
                i = j;
            } else {
                results.push(self.run_tool(s, settings, &calls[i], invalid, token, sink).await);
                i += 1;
            }
        }
        results
    }

    async fn run_tool(
        self: &Arc<Self>,
        s: &Arc<Session>,
        settings: &Settings,
        call: &Value,
        invalid: &HashMap<String, String>,
        token: &CancellationToken,
        sink: &Sink,
    ) -> Value {
        let id = call["id"].as_str().unwrap_or_default();
        let name = call["name"].as_str().unwrap_or_default();
        let input = &call["input"];
        let kind = if name == TASK_TOOL { "think" } else { tools::kind(name) };
        let title = match (name, sink) {
            (TASK_TOOL, _) => format!("Subagent: {}", input["description"].as_str().unwrap_or("task")),
            (_, Sink::Sub { label, .. }) => format!("⤷ {label}: {}", tools::title(name, input)),
            _ => tools::title(name, input),
        };
        let cwd = settings.cwd.clone();
        // For edits, the diff they'd make – shown on the approval card and the tool call.
        let preview = tools::preview(name, input, &cwd);
        if !matches!(sink, Sink::Main) {
            // Subagent calls weren't announced while streaming; introduce them now.
            self.update(&s.id, json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": kind, "status": "pending", "rawInput": input, "content": preview }));
        }
        let call_update = json!({
            "sessionUpdate": "tool_call_update", "toolCallId": id, "title": title, "kind": kind,
            "rawInput": input, "content": preview,
        });
        let done = |status: &str, content: Vec<Value>, output: &str| {
            let mut u = call_update.clone();
            u["status"] = json!(status);
            u["content"] = json!(content);
            u["rawOutput"] = json!(output);
            self.update(&s.id, u);
        };

        if let Some(raw) = invalid.get(id) {
            let msg = json!({ "INVALID_JSON": raw }).to_string();
            done("failed", vec![], &msg);
            return tool_result(call, &msg, true);
        }
        if token.is_cancelled() {
            done("failed", vec![], "cancelled");
            return tool_result(call, "Cancelled by the user.", true);
        }
        let mutating = matches!(kind, "edit" | "execute");
        if settings.mode == "plan" && mutating {
            let msg = "Plan mode is read-only. Describe the change instead of making it.";
            done("failed", vec![], msg);
            return tool_result(call, msg, true);
        }
        let ask = mutating
            && match settings.mode.as_str() {
                "bypassPermissions" => false,
                "acceptEdits" => kind == "execute",
                _ => true,
            }
            && !s.settings.lock().always_allow.iter().any(|k| k == kind);

        if ask {
            let always = if kind == "edit" { "Allow all edits this session" } else { "Allow all commands this session" };
            let params = json!({
                "sessionId": s.id,
                "toolCall": { "toolCallId": id, "title": title, "kind": kind, "rawInput": input, "content": preview, "status": "pending" },
                "options": [
                    { "optionId": "allow-once", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "allow-always", "name": always, "kind": "allow_always" },
                    { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
                ],
            });
            let answer = tokio::select! {
                r = self.request("session/request_permission", params) => r.unwrap_or(Value::Null),
                _ = token.cancelled() => Value::Null,
            };
            match answer["outcome"]["optionId"].as_str() {
                Some("allow-once") => {}
                Some("allow-always") => {
                    s.settings.lock().always_allow.push(kind.to_string());
                    let _ = self.save_settings(s);
                }
                Some(_) => {
                    done("failed", vec![], "rejected");
                    return tool_result(call, "The user rejected this tool call. Ask them how to proceed if it's unclear.", true);
                }
                None => {
                    done("failed", vec![], "cancelled");
                    return tool_result(call, "Cancelled by the user.", true);
                }
            }
        }

        let mut started = call_update.clone();
        started["status"] = json!("in_progress");
        self.update(&s.id, started);
        if name == "todo_write" && matches!(sink, Sink::Main) {
            let entries: Vec<Value> = input["todos"]
                .as_array()
                .map(|a| a.iter().map(|t| json!({ "content": t["content"], "status": t["status"], "priority": "medium" })).collect())
                .unwrap_or_default();
            self.update(&s.id, json!({ "sessionUpdate": "plan", "entries": entries }));
        }
        let out = if name == TASK_TOOL {
            self.run_subagent(s, settings, input, token).await
        } else {
            tools::run(name, input, &cwd, token).await
        };
        let content = if out.content.is_empty() && (kind != "think" || name == TASK_TOOL) {
            let preview: String = out.text.chars().take(4000).collect();
            vec![json!({ "type": "content", "content": { "type": "text", "text": preview } })]
        } else {
            out.content.clone()
        };
        done(if out.is_error { "failed" } else { "completed" }, content, &out.text.chars().take(4000).collect::<String>());
        tool_result(call, &out.text, out.is_error)
    }

    /// Runs a subagent: a fresh context with the delegated prompt, read-only tools for
    /// "explore", all tools for "work". Its final message is the report the parent gets back.
    async fn run_subagent(self: &Arc<Self>, s: &Arc<Session>, settings: &Settings, input: &Value, token: &CancellationToken) -> tools::Output {
        let Some(task) = input["prompt"].as_str().filter(|p| !p.trim().is_empty()) else {
            return tools::Output::err("`prompt` is required");
        };
        let work = input["mode"] == "work";
        let label = input["description"].as_str().unwrap_or("subagent").to_string();
        let model = input["model"]
            .as_str()
            .map(str::to_string)
            .or_else(|| self.native.subagent_model.clone())
            .unwrap_or_else(|| settings.model.clone());
        let tool_defs: Vec<Value> = tools::definitions()
            .into_iter()
            .filter(|t| work || tools::read_only(t["name"].as_str().unwrap_or_default()))
            .filter(|t| t["name"] != "todo_write")
            .collect();
        let system = format!(
            "{}\n# You are a subagent\nAnother agent delegated one task to you; it sees only your final message. \
             Do the task{}, then reply with a complete, self-contained report: what you found or did, with exact \
             file paths, line numbers and identifiers. Don't ask questions back – make reasonable assumptions and say what they were.\n",
            prompt::system(&settings.cwd),
            if work { "" } else { " (you have read-only tools)" },
        );
        let mut history = vec![json!({ "role": "user", "content": [{ "type": "text", "text": task }] })];
        let sink = Sink::Sub { label, model: Some(model) };
        // Boxed: the subagent's loop runs tools, which is how we got here.
        let stop = Box::pin(self.run_loop(s, &mut history, &system, &tool_defs, &sink, token)).await;
        let report: String = history
            .iter()
            .rev()
            .find(|m| m["role"] == "assistant")
            .and_then(|m| m["content"].as_array())
            .map(|c| c.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        match stop {
            Ok("end_turn") if !report.trim().is_empty() => tools::Output { text: report, is_error: false, content: vec![] },
            Ok(stop) => tools::Output::err(format!("The subagent stopped early ({stop}). Last report:\n{report}")),
            Err(e) => tools::Output::err(format!("The subagent failed: {e:#}")),
        }
    }
}

const TASK_TOOL: &str = "task";

/// Who a loop runs for: the session itself (streamed, saved) or a subagent (quiet, ephemeral).
enum Sink {
    Main,
    Sub { label: String, model: Option<String> },
}

impl Sink {
    fn model(&self) -> Option<&str> {
        match self {
            Sink::Main => None,
            Sink::Sub { model, .. } => model.as_deref(),
        }
    }
}

/// The session's tools: the built-ins plus subagents.
fn main_tools() -> Vec<Value> {
    let mut defs = tools::definitions();
    defs.push(json!({
        "name": TASK_TOOL,
        "description": "Delegate a self-contained task to a subagent with a fresh context window. mode \"explore\" (default) gets read-only tools – use it for searching and understanding code; several explore tasks issued together run in parallel. mode \"work\" can edit files and run commands; work tasks run one at a time. The subagent sees only your prompt, so include the goal, relevant paths and what to report back. Returns its final report. Good for parallel investigation and for keeping bulky exploration out of your own context.",
        "input_schema": { "type": "object", "properties": {
            "description": { "type": "string", "description": "3-6 word label shown to the user" },
            "prompt": { "type": "string" },
            "mode": { "type": "string", "enum": ["explore", "work"] },
            "model": { "type": "string", "description": "Optional model override" }
        }, "required": ["description", "prompt"] }
    }));
    defs
}

/// Calls that can run alongside each other without conflicting.
fn concurrent(call: &Value) -> bool {
    let name = call["name"].as_str().unwrap_or_default();
    if name == TASK_TOOL {
        return call["input"]["mode"] != "work";
    }
    tools::read_only(name)
}

/// Last resort when even the summarizer can't fit the history: clip large tool outputs in all
/// but the latest exchange so a compaction request fits.
fn shrink_old_tool_results(history: &mut [Value]) {
    let keep_from = history.len().saturating_sub(2);
    for msg in &mut history[..keep_from] {
        for block in msg["content"].as_array_mut().into_iter().flatten() {
            if block["type"] != "tool_result" {
                continue;
            }
            if let Some(text) = block["content"].as_str().filter(|t| t.len() > 2_000) {
                let head: String = text.chars().take(1_000).collect();
                block["content"] = json!(format!("{head}\n… [output removed to make room in the context window]"));
            }
        }
    }
}

fn prompt_text(prompt: &[Value]) -> String {
    prompt.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect()
}

fn tool_result(call: &Value, text: &str, is_error: bool) -> Value {
    json!({ "type": "tool_result", "tool_use_id": call["id"], "content": text, "is_error": is_error })
}

/// If the last assistant message asked for tools that never got results (the process died
/// mid-turn), answer them so the history stays valid and the model knows what happened.
fn close_dangling_tool_uses(history: &mut Vec<Value>) {
    let Some(last) = history.last() else { return };
    if last["role"] != "assistant" {
        return;
    }
    let results: Vec<Value> = last["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "tool_use")
        .map(|b| tool_result(b, "Interrupted: the agent restarted before this call finished. Check its effect before retrying.", true))
        .collect();
    if !results.is_empty() {
        history.push(json!({ "role": "user", "content": results }));
    }
}

/// ACP prompt content → Anthropic content blocks. @-mentioned files are inlined in read_file
/// format (with their tag), which saves the model a round trip and lets it edit them directly.
fn prompt_blocks(prompt: &[Value], cwd: &Path) -> Vec<Value> {
    let mut out = vec![];
    for b in prompt {
        match b["type"].as_str() {
            Some("text") => out.push(json!({ "type": "text", "text": b["text"] })),
            Some("image") => out.push(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": b["mimeType"], "data": b["data"] },
            })),
            Some("resource_link") => {
                let uri = b["uri"].as_str().unwrap_or_default();
                let path = uri.strip_prefix("file://").unwrap_or(uri);
                let inline = std::fs::metadata(path)
                    .ok()
                    .filter(|m| m.is_file() && m.len() <= 256 * 1024)
                    .and_then(|_| tools::read_view(path, cwd).ok());
                let text = match inline {
                    Some(view) => format!("<file>\n{view}</file>"),
                    None => format!("[Attached: {path}]"),
                };
                out.push(json!({ "type": "text", "text": text }));
            }
            Some("resource") => {
                let r = &b["resource"];
                let text = r["text"].as_str().map(|t| format!("<file uri=\"{}\">\n{t}\n</file>", r["uri"].as_str().unwrap_or_default()));
                out.push(json!({ "type": "text", "text": text.unwrap_or_else(|| format!("[Attached: {}]", r["uri"])) }));
            }
            _ => {}
        }
    }
    if out.is_empty() {
        out.push(json!({ "type": "text", "text": "(empty prompt)" }));
    }
    out
}


fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
