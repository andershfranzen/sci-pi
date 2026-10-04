//! Model providers for the native agent, over raw HTTP (there is no official Rust SDK).
//!
//! Conversation history is kept in Anthropic Messages format – content blocks as raw JSON,
//! echoed back exactly as received (thinking blocks must round-trip unchanged). Other
//! providers translate from that format on the way out.

use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::time::Duration;

pub const ANTHROPIC_MODELS: &[(&str, &str)] = &[
    ("claude-opus-5-5", "Claude Opus 5.5"),
    ("claude-fable-5-1", "Claude Fable 5.1"),
    ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
    ("claude-haiku-4-5", "Claude Haiku 4.5"),
];

pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Streaming progress, forwarded to the client as ACP session updates.
pub enum Delta {
    Text(String),
    Thinking(String),
    ToolStart { id: String, name: String },
}

pub struct Request<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Value],
    /// Tool definitions in Anthropic format (`name`, `description`, `input_schema`).
    pub tools: &'a [Value],
    pub effort: &'a str,
}

pub struct Response {
    /// Assistant content blocks, Anthropic format.
    pub content: Vec<Value>,
    pub stop_reason: String,
    /// `{ input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens }`
    pub usage: Value,
    /// tool_use id → raw input that wasn't valid JSON (never run those).
    pub invalid_inputs: HashMap<String, String>,
}

#[derive(Clone)]
pub enum Provider {
    /// `proxy`: a compatible server rather than Anthropic itself – sends the key as a bearer
    /// token too and leaves out fields proxies may reject (eager tool streaming, fallbacks).
    Anthropic { api_key: String, base_url: String, proxy: bool },
    /// Any OpenAI-compatible chat-completions endpoint (OpenAI, OpenRouter, llama.cpp, vLLM, Ollama…).
    OpenAi { api_key: Option<String>, base_url: String },
}

impl Provider {
    pub async fn stream(
        &self,
        http: &reqwest::Client,
        req: &Request<'_>,
        on: &mut (dyn FnMut(Delta) + Send),
    ) -> Result<Response> {
        match self {
            Provider::Anthropic { api_key, base_url, proxy } => anthropic(http, api_key, base_url, *proxy, req, on).await,
            Provider::OpenAi { api_key, base_url } => openai(http, api_key.as_deref(), base_url, req, on).await,
        }
    }
}

/// POSTs with retries on rate limits / overload / 5xx (only before any output streamed).
async fn post_with_retry(builder: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response> {
    let mut delay = Duration::from_secs(2);
    for attempt in 0.. {
        let res = builder().send().await;
        match res {
            Ok(r) if r.status().is_success() => return Ok(r),
            Ok(r) => {
                let status = r.status();
                let retry_after = r
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok());
                let body = r.text().await.unwrap_or_default();
                let retryable = status.as_u16() == 429 || status.as_u16() == 529 || status.is_server_error();
                if !retryable || attempt >= 4 {
                    let msg = serde_json::from_str::<Value>(&body)
                        .ok()
                        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                        .unwrap_or(body);
                    bail!("HTTP {status}: {msg}");
                }
                tokio::time::sleep(retry_after.map(Duration::from_secs).unwrap_or(delay)).await;
            }
            Err(e) if attempt < 4 && (e.is_connect() || e.is_timeout()) => tokio::time::sleep(delay).await,
            Err(e) => return Err(e.into()),
        }
        delay = (delay * 2).min(Duration::from_secs(60));
    }
    unreachable!()
}

/// Splits a byte stream into server-sent events: `(event name, data)`.
struct Sse {
    buf: String,
}

impl Sse {
    fn new() -> Self {
        Sse { buf: String::new() }
    }

    fn push(&mut self, chunk: &[u8]) -> Vec<(String, String)> {
        self.buf.push_str(&String::from_utf8_lossy(chunk).replace('\r', ""));
        let mut out = vec![];
        while let Some(end) = self.buf.find("\n\n") {
            let raw: String = self.buf.drain(..end + 2).collect();
            let (mut event, mut data) = (String::new(), String::new());
            for line in raw.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    event = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(v.strip_prefix(' ').unwrap_or(v));
                }
            }
            if !data.is_empty() {
                out.push((event, data));
            }
        }
        out
    }
}

/// Per-model request options: (thinking config, supports effort, server-side refusal fallbacks).
fn anthropic_model_opts(model: &str) -> (Option<Value>, bool, bool) {
    if model.starts_with("claude-haiku") {
        return (None, false, false);
    }
    let thinking = Some(json!({ "type": "adaptive", "display": "summarized" }));
    let fallbacks = matches!(model, "claude-fable-5-1" | "claude-opus-5-5" | "claude-opus-5" | "claude-sonnet-5-5");
    (thinking, true, fallbacks)
}

async fn anthropic(
    http: &reqwest::Client,
    api_key: &str,
    base_url: &str,
    proxy: bool,
    req: &Request<'_>,
    on: &mut (dyn FnMut(Delta) + Send),
) -> Result<Response> {
    let (thinking, effort, fallbacks) = anthropic_model_opts(req.model);
    let fallbacks = fallbacks && !proxy;
    // Stream tool inputs as they're generated; we validate them ourselves at block stop.
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            let mut t = t.clone();
            if !proxy {
                t["eager_input_streaming"] = json!(true);
            }
            t
        })
        .collect();
    let mut body = json!({
        "model": req.model,
        "max_tokens": 64000,
        "stream": true,
        "system": [{ "type": "text", "text": req.system }],
        "messages": req.messages,
        "tools": tools,
        // Auto-placed on the last cacheable block: each step re-reads the whole prefix from cache.
        "cache_control": { "type": "ephemeral" },
    });
    if let Some(t) = thinking {
        body["thinking"] = t;
    }
    if effort {
        body["output_config"] = json!({ "effort": req.effort });
    }
    if fallbacks {
        body["fallbacks"] = json!("default");
    }
    let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
    let res = post_with_retry(|| {
        let mut b = http
            .post(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body);
        if fallbacks {
            b = b.header("anthropic-beta", "server-side-fallback-2026-07-01");
        }
        if proxy {
            b = b.bearer_auth(api_key);
        }
        b
    })
    .await?;

    let mut blocks: Vec<Value> = vec![];
    let mut partial: HashMap<usize, String> = HashMap::new();
    let mut invalid = HashMap::new();
    let mut stop_reason = String::from("end_turn");
    let mut usage = json!({});
    let mut sse = Sse::new();
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading model stream")?;
        for (_, data) in sse.push(&chunk) {
            let ev: Value = serde_json::from_str(&data).with_context(|| format!("bad event: {data}"))?;
            match ev["type"].as_str().unwrap_or_default() {
                "message_start" => usage = ev["message"]["usage"].clone(),
                "content_block_start" => {
                    let i = ev["index"].as_u64().unwrap_or(0) as usize;
                    let block = ev["content_block"].clone();
                    if block["type"] == "tool_use" {
                        partial.insert(i, String::new());
                        on(Delta::ToolStart {
                            id: block["id"].as_str().unwrap_or_default().into(),
                            name: block["name"].as_str().unwrap_or_default().into(),
                        });
                    }
                    if blocks.len() <= i {
                        blocks.resize(i + 1, Value::Null);
                    }
                    blocks[i] = block;
                }
                "content_block_delta" => {
                    let i = ev["index"].as_u64().unwrap_or(0) as usize;
                    let d = &ev["delta"];
                    let Some(block) = blocks.get_mut(i) else { continue };
                    match d["type"].as_str().unwrap_or_default() {
                        "text_delta" => {
                            let t = d["text"].as_str().unwrap_or_default();
                            append(block, "text", t);
                            on(Delta::Text(t.into()));
                        }
                        "thinking_delta" => {
                            let t = d["thinking"].as_str().unwrap_or_default();
                            append(block, "thinking", t);
                            on(Delta::Thinking(t.into()));
                        }
                        "signature_delta" => append(block, "signature", d["signature"].as_str().unwrap_or_default()),
                        "input_json_delta" => {
                            partial.entry(i).or_default().push_str(d["partial_json"].as_str().unwrap_or_default())
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let i = ev["index"].as_u64().unwrap_or(0) as usize;
                    if let Some(raw) = partial.remove(&i) {
                        let parsed = if raw.trim().is_empty() { Ok(json!({})) } else { serde_json::from_str::<Value>(&raw) };
                        match parsed {
                            Ok(v) if v.is_object() => blocks[i]["input"] = v,
                            _ => {
                                blocks[i]["input"] = json!({});
                                invalid.insert(blocks[i]["id"].as_str().unwrap_or_default().to_string(), raw);
                            }
                        }
                    }
                }
                "message_delta" => {
                    if let Some(r) = ev["delta"]["stop_reason"].as_str() {
                        stop_reason = r.to_string();
                    }
                    if let (Some(u), Some(new)) = (usage.as_object_mut(), ev["usage"].as_object()) {
                        for (k, v) in new {
                            if !v.is_null() {
                                u.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
                "error" => bail!("{}", ev["error"]["message"].as_str().unwrap_or("model stream error")),
                _ => {}
            }
        }
    }
    blocks.retain(|b| !b.is_null());
    Ok(Response { content: blocks, stop_reason, usage, invalid_inputs: invalid })
}

fn append(block: &mut Value, field: &str, s: &str) {
    let cur = block[field].as_str().unwrap_or_default().to_string();
    block[field] = Value::String(cur + s);
}

/// Anthropic-format history → OpenAI chat messages.
fn to_openai_messages(system: &str, messages: &[Value]) -> Vec<Value> {
    let mut out = vec![json!({ "role": "system", "content": system })];
    for m in messages {
        let blocks: Vec<Value> = match &m["content"] {
            Value::String(s) => vec![json!({ "type": "text", "text": s })],
            Value::Array(a) => a.clone(),
            _ => vec![],
        };
        if m["role"] == "assistant" {
            let text: String = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
            let calls: Vec<Value> = blocks
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| {
                    json!({ "id": b["id"], "type": "function",
                            "function": { "name": b["name"], "arguments": b["input"].to_string() } })
                })
                .collect();
            let mut msg = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) } });
            if !calls.is_empty() {
                msg["tool_calls"] = json!(calls);
            }
            out.push(msg);
            continue;
        }
        // Tool results must directly follow the assistant message that asked for them.
        let mut parts = vec![];
        for b in &blocks {
            match b["type"].as_str() {
                Some("tool_result") => {
                    let content = match &b["content"] {
                        Value::String(s) => s.clone(),
                        Value::Array(a) => a.iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n"),
                        _ => String::new(),
                    };
                    out.push(json!({ "role": "tool", "tool_call_id": b["tool_use_id"], "content": content }));
                }
                Some("text") => parts.push(json!({ "type": "text", "text": b["text"] })),
                Some("image") => parts.push(json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{};base64,{}",
                        b["source"]["media_type"].as_str().unwrap_or("image/png"),
                        b["source"]["data"].as_str().unwrap_or_default()) }
                })),
                _ => {}
            }
        }
        if !parts.is_empty() {
            out.push(json!({ "role": "user", "content": parts }));
        }
    }
    out
}

async fn openai(
    http: &reqwest::Client,
    api_key: Option<&str>,
    base_url: &str,
    req: &Request<'_>,
    on: &mut (dyn FnMut(Delta) + Send),
) -> Result<Response> {
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!({ "type": "function", "function": { "name": t["name"], "description": t["description"], "parameters": t["input_schema"] } }))
        .collect();
    let mut body = json!({
        "model": req.model,
        "messages": to_openai_messages(req.system, req.messages),
        "tools": tools,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    // Reasoning models (OpenAI's gpt-5+/o-series, also via proxies) take an effort level.
    if req.model.starts_with("gpt-5") || req.model.starts_with("gpt-6") || req.model.starts_with('o') {
        body["reasoning_effort"] = json!(match req.effort {
            "low" => "low",
            "medium" => "medium",
            _ => "high",
        });
    }
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let res = post_with_retry(|| {
        let b = http.post(&url).json(&body);
        match api_key {
            Some(k) => b.bearer_auth(k),
            None => b,
        }
    })
    .await?;

    let mut text = String::new();
    // index → (id, name, arguments)
    let mut calls: Vec<(String, String, String)> = vec![];
    let mut finish = String::from("stop");
    let mut usage = json!({});
    let mut sse = Sse::new();
    let mut stream = res.bytes_stream();
    'outer: while let Some(chunk) = stream.next().await {
        for (_, data) in sse.push(&chunk.context("reading model stream")?) {
            if data.trim() == "[DONE]" {
                break 'outer;
            }
            let ev: Value = serde_json::from_str(&data).with_context(|| format!("bad chunk: {data}"))?;
            if let Some(err) = ev.get("error") {
                bail!("{}", err["message"].as_str().unwrap_or("model error"));
            }
            if let Some(u) = ev.get("usage").filter(|u| u.is_object()) {
                usage = json!({ "input_tokens": u["prompt_tokens"], "output_tokens": u["completion_tokens"] });
            }
            let Some(choice) = ev["choices"].get(0) else { continue };
            let d = &choice["delta"];
            if let Some(t) = d["content"].as_str().filter(|t| !t.is_empty()) {
                text.push_str(t);
                on(Delta::Text(t.into()));
            }
            for key in ["reasoning_content", "reasoning"] {
                if let Some(t) = d[key].as_str().filter(|t| !t.is_empty()) {
                    on(Delta::Thinking(t.into()));
                }
            }
            for tc in d["tool_calls"].as_array().into_iter().flatten() {
                let i = tc["index"].as_u64().unwrap_or(calls.len() as u64) as usize;
                if calls.len() <= i {
                    calls.resize(i + 1, Default::default());
                }
                let c = &mut calls[i];
                if let Some(id) = tc["id"].as_str() {
                    c.0 = id.to_string();
                }
                if let Some(name) = tc["function"]["name"].as_str() {
                    if c.1.is_empty() {
                        c.1 = name.to_string();
                        on(Delta::ToolStart { id: c.0.clone(), name: c.1.clone() });
                    }
                }
                if let Some(args) = tc["function"]["arguments"].as_str() {
                    c.2.push_str(args);
                }
            }
            if let Some(f) = choice["finish_reason"].as_str() {
                finish = f.to_string();
            }
        }
    }
    let mut content = vec![];
    if !text.is_empty() {
        content.push(json!({ "type": "text", "text": text }));
    }
    let mut invalid = HashMap::new();
    for (i, (id, name, args)) in calls.into_iter().enumerate() {
        let id = if id.is_empty() { format!("call_{i}") } else { id };
        let input = match serde_json::from_str::<Value>(if args.trim().is_empty() { "{}" } else { &args }) {
            Ok(v) if v.is_object() => v,
            _ => {
                invalid.insert(id.clone(), args);
                Value::Object(Map::new())
            }
        };
        content.push(json!({ "type": "tool_use", "id": id, "name": name, "input": input }));
    }
    let has_tools = content.iter().any(|b| b["type"] == "tool_use");
    let stop_reason = match finish.as_str() {
        "length" => "max_tokens",
        _ if has_tools => "tool_use",
        _ => "end_turn",
    };
    Ok(Response { content, stop_reason: stop_reason.into(), usage, invalid_inputs: invalid })
}

pub fn context_window(model: &str) -> u64 {
    if model.starts_with("claude-haiku") {
        200_000
    } else if model.starts_with("claude-") {
        1_000_000
    } else {
        128_000
    }
}

pub fn no_key(provider: &str) -> anyhow::Error {
    anyhow!("no API key for {provider}; run `outpost auth set {provider}` on this host (or set the env var)")
}

/// Model ids an OpenAI-style `/models` endpoint offers.
pub async fn list_models(http: &reqwest::Client, models_url: &str, api_key: Option<&str>) -> Result<Vec<String>> {
    let mut b = http.get(models_url).timeout(Duration::from_secs(8));
    if let Some(k) = api_key {
        b = b.bearer_auth(k);
    }
    let res = b.send().await?;
    if !res.status().is_success() {
        bail!("{models_url}: HTTP {}", res.status());
    }
    let v: Value = res.json().await?;
    let mut ids: Vec<String> = v["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(str::to_string)).collect();
    ids.sort();
    Ok(ids)
}
