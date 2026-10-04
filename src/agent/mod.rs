//! outpost's own agent, served over the Agent Client Protocol on stdio (`outpost acp`). The
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
                "agentInfo": { "name": "outpost", "title": "outpost", "version": env!("CARGO_PKG_VERSION") },
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
                Ok(self.session_info(&session, true))
            }
            "session/resume" | "session/load" => {
                let id = p["sessionId"].as_str().ok_or_else(|| anyhow!("sessionId is required"))?;
                let session = self.load(id)?;
                self.sessions.lock().unwrap().insert(id.to_string(), session.clone());
                self.discover_models().await;
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

        close_dangling_tool_uses(&mut history);
        let blocks = prompt_blocks(&prompt, &cwd);
        match history.last_mut() {
            // Results of interrupted tools are already a user message; add the prompt to it.
            Some(last) if last["role"] == "user" => last["content"].as_array_mut().unwrap().extend(blocks),
            _ => history.push(json!({ "role": "user", "content": blocks })),
        }
        self.save_history(&s.id, &history)?;

        let system = prompt::system(&cwd);
        let tool_defs = tools::definitions();
        for _ in 0..MAX_STEPS {
            let settings = s.settings.lock().unwrap().clone();
            let (provider, api_model) = self.provider(&settings.model)?;
            let message_id = uuid::Uuid::new_v4().to_string();
            let sid = s.id.clone();
            let this = self.clone();
            let mut on = move |d: Delta| match d {
                Delta::Text(t) => this.update(&sid, json!({ "sessionUpdate": "agent_message_chunk", "messageId": message_id, "content": { "type": "text", "text": t } })),
                Delta::Thinking(t) => this.update(&sid, json!({ "sessionUpdate": "agent_thought_chunk", "messageId": message_id, "content": { "type": "text", "text": t } })),
                Delta::ToolStart { id, name } => this.update(&sid, json!({
                    "sessionUpdate": "tool_call", "toolCallId": id, "title": name, "kind": tools::kind(&name),
                    "status": "pending", "rawInput": {}, "content": [],
                })),
            };
            let req = Request { model: &api_model, system: &system, messages: &history, tools: &tool_defs, effort: &settings.effort };
            let resp = tokio::select! {
                r = provider.stream(&self.http, &req, &mut on) => r?,
                _ = token.cancelled() => return self.finish(s, &history, "cancelled"),
            };
            self.report_usage(s, &settings.model, &resp.usage);
            if !resp.content.is_empty() {
                history.push(json!({ "role": "assistant", "content": resp.content }));
                self.save_history(&s.id, &history)?;
            }

            let calls: Vec<Value> = resp.content.iter().filter(|b| b["type"] == "tool_use").cloned().collect();
            if calls.is_empty() {
                if resp.stop_reason == "pause_turn" {
                    continue;
                }
                let stop = match resp.stop_reason.as_str() {
                    "max_tokens" => "max_tokens",
                    "refusal" => "refusal",
                    _ => "end_turn",
                };
                return self.finish(s, &history, stop);
            }
            // A response cut off mid-tool-call may carry truncated input: never run those.
            let results = if matches!(resp.stop_reason.as_str(), "max_tokens" | "refusal") {
                calls.iter().map(|c| tool_result(c, "Not run: the response was cut off before this call was complete. Retry in smaller steps.", true)).collect()
            } else {
                self.run_tools(s, &settings, &calls, &resp.invalid_inputs, &token).await
            };
            history.push(json!({ "role": "user", "content": results }));
            self.save_history(&s.id, &history)?;
            if token.is_cancelled() {
                return self.finish(s, &history, "cancelled");
            }
        }
        self.finish(s, &history, "max_turn_requests")
    }

    fn finish(&self, s: &Session, history: &[Value], stop: &'static str) -> Result<&'static str> {
        *s.cancel.lock().unwrap() = None;
        self.save_history(&s.id, history)?;
        Ok(stop)
    }

    fn report_usage(&self, s: &Session, model: &str, usage: &Value) {
        let n = |k: &str| usage[k].as_u64().unwrap_or(0);
        let (input, read, write, output) =
            (n("input_tokens"), n("cache_read_input_tokens"), n("cache_creation_input_tokens"), n("output_tokens"));
        let cost = llm_cost(model, input, read, write, output);
        let total = {
            let mut st = s.settings.lock().unwrap();
            st.cost_usd += cost;
            st.cost_usd
        };
        let _ = self.save_settings(s);
        let mut update = json!({
            "sessionUpdate": "usage_update",
            "used": input + read + write + output,
            "size": llm::context_window(model),
        });
        if model.starts_with("claude-") {
            update["cost"] = json!({ "amount": total, "currency": "USD" });
        }
        self.update(&s.id, update);
    }

    /// Runs one response's tool calls: consecutive read-only calls concurrently, everything
    /// else one at a time in order. Results come back in call order.
    async fn run_tools(
        self: &Arc<Self>,
        s: &Arc<Session>,
        settings: &Settings,
        calls: &[Value],
        invalid: &HashMap<String, String>,
        token: &CancellationToken,
    ) -> Vec<Value> {
        let mut results = Vec::with_capacity(calls.len());
        let mut i = 0;
        while i < calls.len() {
            let name = calls[i]["name"].as_str().unwrap_or_default();
            if tools::read_only(name) {
                let mut j = i;
                while j < calls.len() && tools::read_only(calls[j]["name"].as_str().unwrap_or_default()) {
                    j += 1;
                }
                let batch = calls[i..j].iter().map(|c| self.run_tool(s, settings, c, invalid, token));
                results.extend(futures::future::join_all(batch).await);
                i = j;
            } else {
                results.push(self.run_tool(s, settings, &calls[i], invalid, token).await);
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
    ) -> Value {
        let id = call["id"].as_str().unwrap_or_default();
        let name = call["name"].as_str().unwrap_or_default();
        let input = &call["input"];
        let kind = tools::kind(name);
        let title = tools::title(name, input);
        let cwd = settings.cwd.clone();
        // For edits, the diff they'd make – shown on the approval card and the tool call.
        let preview = tools::preview(name, input, &cwd);
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
            let mut pending = call_update.clone();
            pending["status"] = json!("pending");
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
        if name == "todo_write" {
            let entries: Vec<Value> = input["todos"]
                .as_array()
                .map(|a| a.iter().map(|t| json!({ "content": t["content"], "status": t["status"], "priority": "medium" })).collect())
                .unwrap_or_default();
            self.update(&s.id, json!({ "sessionUpdate": "plan", "entries": entries }));
        }
        let out = tools::run(name, input, &cwd, token).await;
        let content = if out.content.is_empty() && kind != "think" {
            let preview: String = out.text.chars().take(4000).collect();
            vec![json!({ "type": "content", "content": { "type": "text", "text": preview } })]
        } else {
            out.content.clone()
        };
        done(if out.is_error { "failed" } else { "completed" }, content, &out.text.chars().take(4000).collect::<String>());
        tool_result(call, &out.text, out.is_error)
    }
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
