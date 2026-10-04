//! sci-pi's own agent, served over the Agent Client Protocol on stdio (`sci-pi acp`). The
//! daemon runs it like any other ACP agent; editors that speak ACP can use it too.
//!
//! The loop: stream a model response (forwarding text, thinking and tool starts as session
//! updates), run the requested tools (read-only ones in parallel, edits and commands one at a time
//! behind the permission policy), append the results, repeat. Every step is saved, so a session
//! resumes with its full history after the process restarts.

pub mod llm;
pub mod prompt;
pub mod tools;

use crate::config::{self, Config, NativeConfig, ProviderKind};
use anyhow::{anyhow, bail, Context, Result};
use llm::{Delta, Provider, Request};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
    /// provider → (fetched at, model ids) for providers that list their own models.
    discovered: Mutex<HashMap<String, (std::time::Instant, Vec<String>)>>,
    dir: PathBuf,
    http: reqwest::Client,
}

pub async fn serve_stdio() -> Result<()> {
    let cfg = Config::load_or_init()?;
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
        native: cfg.native,
        discovered: Mutex::default(),
        dir,
        http: reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(30)).build()?,
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
                if let Some(tx) = id.as_u64().and_then(|id| server.pending.lock().unwrap().remove(&id)) {
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
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        rx.await.map_err(|_| anyhow!("client went away"))
    }

    fn notification(&self, method: &str, params: &Value) {
        if method == "session/cancel" {
            if let Some(s) = params["sessionId"].as_str().and_then(|id| self.session(id).ok()) {
                if let Some(t) = s.cancel.lock().unwrap().as_ref() {
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
                "agentInfo": { "name": "sci-pi", "title": "sci-pi", "version": env!("CARGO_PKG_VERSION") },
                "authMethods": [],
            })),
            "session/new" => {
                let cwd = PathBuf::from(p["cwd"].as_str().ok_or_else(|| anyhow!("cwd is required"))?);
                let settings = Settings {
                    cwd,
                    model: self.native.model.clone(),
                    effort: self.native.effort.clone(),
                    mode: "default".into(),
                    always_allow: vec![],
                    cost_usd: 0.0,
                    context_tokens: 0,
                };
                let session = Arc::new(Session {
                    id: uuid::Uuid::new_v4().to_string(),
                    settings: Mutex::new(settings),
                    history: tokio::sync::Mutex::new(vec![]),
                    cancel: Mutex::new(None),
                });
                self.save_settings(&session)?;
                self.save_history(&session.id, &[])?;
                self.discover_models().await;
                self.sessions.lock().unwrap().insert(session.id.clone(), session.clone());
                self.advertise_commands(&session.id);
                Ok(self.session_info(&session, true))
            }
            "session/resume" | "session/load" => {
                let id = p["sessionId"].as_str().ok_or_else(|| anyhow!("sessionId is required"))?;
                let session = self.load(id)?;
                self.sessions.lock().unwrap().insert(id.to_string(), session.clone());
                self.discover_models().await;
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
                Ok(json!({ "configOptions": self.config_options(&s.settings.lock().unwrap()) }))
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
        self.sessions.lock().unwrap().get(id).cloned().ok_or_else(|| anyhow!("unknown session {id}"))
    }

    fn session_info(&self, s: &Session, with_id: bool) -> Value {
        let settings = s.settings.lock().unwrap();
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
        let mut models: Vec<Value> =
            llm::ANTHROPIC_MODELS.iter().map(|(id, name)| json!({ "value": id, "name": name })).collect();
        let discovered = self.discovered.lock().unwrap();
        for (provider, p) in &self.native.providers {
            let listed = if p.models.is_empty() { discovered.get(provider).map(|d| d.1.clone()).unwrap_or_default() } else { p.models.clone() };
            for m in &listed {
                models.push(json!({ "value": format!("{provider}/{m}"), "name": format!("{m} ({provider})") }));
            }
        }
        if !models.iter().any(|m| m["value"] == s.model.as_str()) {
            models.push(json!({ "value": s.model, "name": s.model }));
        }
        json!([
            { "id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": s.mode,
              "options": MODES.iter().map(|(id, name, d)| json!({ "value": id, "name": name, "description": d })).collect::<Vec<_>>() },
            { "id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": s.model, "options": models },
            { "id": "effort", "name": "Effort", "category": "thought_level", "type": "select", "currentValue": s.effort,
              "options": llm::EFFORTS.iter().map(|e| json!({ "value": e, "name": e })).collect::<Vec<_>>() },
        ])
    }

    /// Fills in model lists for providers configured without one (cached for 5 minutes).
    async fn discover_models(&self) {
        for (name, p) in &self.native.providers {
            if !p.models.is_empty() {
                continue;
            }
            let fresh = self.discovered.lock().unwrap().get(name).is_some_and(|(at, _)| at.elapsed().as_secs() < 300);
            if fresh {
                continue;
            }
            let base = p.base_url.trim_end_matches('/');
            let url = match p.kind {
                ProviderKind::Cliproxy => format!("{base}/v1/models"),
                _ => format!("{base}/models"),
            };
            let key = config::credential(name, p.api_key_env.as_deref());
            match llm::list_models(&self.http, &url, key.as_deref()).await {
                Ok(ids) => {
                    self.discovered.lock().unwrap().insert(name.clone(), (std::time::Instant::now(), ids));
                }
                Err(e) => eprintln!("model discovery for {name} failed: {e:#}"),
            }
        }
    }

    fn set_option(&self, s: &Session, id: &str, value: &Value) -> Result<()> {
        let v = value.as_str().ok_or_else(|| anyhow!("value must be a string"))?.to_string();
        {
            let mut st = s.settings.lock().unwrap();
            match id {
                "mode" if MODES.iter().any(|m| m.0 == v) => st.mode = v.clone(),
                "model" => st.model = v.clone(),
                "effort" if llm::EFFORTS.contains(&v.as_str()) => st.effort = v.clone(),
                _ => bail!("unsupported option {id}={v}"),
            }
        }
        if id == "mode" {
            self.update(&s.id, json!({ "sessionUpdate": "current_mode_update", "currentModeId": v }));
        }
        self.save_settings(s)
    }

    // ── persistence ───────────────────────────────────────────────────────────

    fn save_settings(&self, s: &Session) -> Result<()> {
        let json = serde_json::to_vec(&*s.settings.lock().unwrap())?;
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

    fn provider(&self, model: &str) -> Result<(Provider, String)> {
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
        if model.starts_with("claude-") {
            let api_key = config::credential("anthropic", None).ok_or_else(|| llm::no_key("anthropic"))?;
            let base_url = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into());
            return Ok((Provider::Anthropic { api_key, base_url, proxy: false }, model.to_string()));
        }
        bail!("don't know which provider serves `{model}` (use claude-* or <provider>/<model>)")
    }

    async fn run_turn(self: &Arc<Self>, s: &Arc<Session>, prompt: Vec<Value>) -> Result<&'static str> {
        let token = CancellationToken::new();
        *s.cancel.lock().unwrap() = Some(token.clone());
        let mut history = s.history.lock().await;
        let cwd = s.settings.lock().unwrap().cwd.clone();
        let system = prompt::system(&cwd);
        let tool_defs = main_tools();

        close_dangling_tool_uses(&mut history);
        // `/compact` is ours: summarize now instead of starting a turn.
        if prompt_text(&prompt).trim() == "/compact" {
            let model = s.settings.lock().unwrap().model.clone();
            self.compact(s, &mut history, &system, &tool_defs, &model, &Sink::Main, false).await;
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
        *s.cancel.lock().unwrap() = None;
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
        let mut context: u64 = 0;
        for _ in 0..MAX_STEPS {
            let settings = s.settings.lock().unwrap().clone();
            let model = sink.model().unwrap_or(&settings.model).to_string();
            if context > self.compact_threshold(&model) {
                self.compact(s, history, system, tool_defs, &model, sink, true).await;
            }
            let (provider, api_model) = self.provider(&model)?;
            let message_id = uuid::Uuid::new_v4().to_string();
            let (sid, this, main) = (s.id.clone(), self.clone(), matches!(sink, Sink::Main));
            let mut on = move |d: Delta| {
                if !main {
                    return; // a subagent's stream isn't shown; its tool calls and report are
                }
                match d {
                    Delta::Text(t) => this.update(&sid, json!({ "sessionUpdate": "agent_message_chunk", "messageId": message_id, "content": { "type": "text", "text": t } })),
                    Delta::Thinking(t) => this.update(&sid, json!({ "sessionUpdate": "agent_thought_chunk", "messageId": message_id, "content": { "type": "text", "text": t } })),
                    Delta::ToolStart { id, name } => this.update(&sid, json!({
                        "sessionUpdate": "tool_call", "toolCallId": id, "title": name, "kind": tools::kind(&name),
                        "status": "pending", "rawInput": {}, "content": [],
                    })),
                }
            };
            let req = Request { model: &api_model, system, messages: history, tools: tool_defs, effort: &settings.effort };
            let resp = tokio::select! {
                r = provider.stream(&self.http, &req, &mut on) => r?,
                _ = token.cancelled() => return Ok("cancelled"),
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

    /// Tokens of context at which to compact: 80% of the window, capped so long sessions stay
    /// fast and sharp (quality degrades as context grows), or `native.compact_at_tokens`.
    fn compact_threshold(&self, model: &str) -> u64 {
        let window = llm::context_window(model);
        self.native.compact_at_tokens.unwrap_or(300_000).min(window * 8 / 10)
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
    ) {
        let id = format!("compact-{}", uuid::Uuid::new_v4().simple());
        let title = match sink {
            Sink::Main => "Compacting the conversation".to_string(),
            Sink::Sub { label, .. } => format!("⤷ {label}: compacting"),
        };
        self.update(&s.id, json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "think", "status": "in_progress", "rawInput": {}, "content": [] }));
        let effort = s.settings.lock().unwrap().effort.clone();
        let result = async {
            let (provider, api_model) = self.provider(model)?;
            let req = Request { model: &api_model, system, messages: history, tools: tool_defs, effort: &effort };
            provider.compact(&self.http, &req).await
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
            }
            Err(e) => {
                self.update(&s.id, json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "failed",
                    "content": [{ "type": "content", "content": { "type": "text", "text": format!("Compaction failed: {e:#}") } }] }));
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
        let cost = llm_cost(model, input, read, write, output);
        let (total, context, main_model) = {
            let mut st = s.settings.lock().unwrap();
            st.cost_usd += cost;
            if matches!(sink, Sink::Main) {
                st.context_tokens = used;
            }
            (st.cost_usd, st.context_tokens, st.model.clone())
        };
        let _ = self.save_settings(s);
        let mut update = json!({ "sessionUpdate": "usage_update", "used": context, "size": llm::context_window(&main_model) });
        if total > 0.0 {
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
            && !s.settings.lock().unwrap().always_allow.iter().any(|k| k == kind);

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
                    s.settings.lock().unwrap().always_allow.push(kind.to_string());
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

/// Anthropic list prices per million tokens: (input, output, cache read). Cache writes bill at
/// 1.25× input (5-minute TTL).
fn llm_cost(model: &str, input: u64, read: u64, write: u64, output: u64) -> f64 {
    let (i, o, r) = match model {
        m if m.starts_with("claude-fable") || m.starts_with("claude-mythos") => (10.0, 50.0, 0.25),
        "claude-opus-5-5" => (4.0, 20.0, 0.20),
        m if m.starts_with("claude-opus") => (5.0, 25.0, 0.50),
        "claude-sonnet-5-5" | "claude-sonnet-5" => (2.0, 10.0, 0.20),
        m if m.starts_with("claude-sonnet") => (3.0, 15.0, 0.30),
        m if m.starts_with("claude-haiku") => (1.0, 5.0, 0.10),
        _ => return 0.0,
    };
    (input as f64 * i + write as f64 * i * 1.25 + read as f64 * r + output as f64 * o) / 1_000_000.0
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
