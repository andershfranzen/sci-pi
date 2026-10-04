//! Model providers for the native agent, over raw HTTP (there is no official Rust SDK).
//!
//! Conversation history is kept in Anthropic Messages format – content blocks as raw JSON,
//! echoed back exactly as received (thinking blocks must round-trip unchanged). Other
//! providers translate from that format on the way out.


use super::models::{self, Fast, ModelInfo};
use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::time::Duration;
use std::collections::HashSet;
use std::sync::LazyLock;
use parking_lot::Mutex;


/// Streaming progress, forwarded to the client as ACP session updates.
pub enum Delta {
    Text(String),
    Thinking(String),
    ToolStart { id: String, name: String },
}

#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Value],
    /// Tool definitions in Anthropic format (`name`, `description`, `input_schema`).
    pub tools: &'a [Value],
    pub opts: &'a Opts<'a>,
    
}

#[derive(Clone, Copy)]
pub struct Opts<'a> {
    pub thinking: bool,
    pub effort: Option<&'a str>,
    pub fast: Option<&'a Fast>,
    pub max_output: Option<u64>,
    pub server_compaction: bool,
    pub thinking_display: Option<&'a str>,
    pub fallbacks: bool,
}

impl<'a> Opts<'a> {
    pub fn from_info(info: &'a ModelInfo, effort: &'a str, fast: bool) -> Self {
        let valid = |value: &str| info.efforts.iter().any(|(id, _)| id == value);
        let effort = if valid(effort) { Some(effort) } else {
            info.default_effort.as_deref().filter(|value| valid(value))
                .or_else(|| info.efforts.first().map(|(id, _)| id.as_str()))
        };
        Self {
            thinking: info.adaptive_thinking == Some(true),
            effort,
            fast: if fast { info.fast.as_ref() } else { None },
            max_output: info.max_output,
            server_compaction: info.server_compaction == Some(true),
            thinking_display: info.thinking_display.as_deref(),
            fallbacks: info.fallbacks == Some(true),
        }
    }
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
async fn post_with_retry(builder: impl Fn() -> Result<reqwest::RequestBuilder>) -> Result<reqwest::Response> {
    let mut delay = Duration::from_secs(2);
    for attempt in 0.. {
        let res = builder()?.send().await;
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

const OPTIONAL_FIELDS: &[&str] = &["fallbacks", "thinking.display", "speed", "eager_input_streaming", "service_tier"];
static REJECTED_FIELDS: LazyLock<Mutex<HashMap<(String, String), HashSet<&'static str>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn remove_optional(body: &mut Value, field: &str) -> bool {
    match field {
        "thinking.display" => body.get_mut("thinking").and_then(Value::as_object_mut)
            .and_then(|thinking| thinking.remove("display")).is_some(),
        "eager_input_streaming" => {
            let mut removed = false;
            for tool in body["tools"].as_array_mut().into_iter().flatten() {
                if let Some(tool) = tool.as_object_mut() {
                    removed |= tool.remove(field).is_some();
                }
            }
            removed
        }
        _ => body.as_object_mut().and_then(|body| body.remove(field)).is_some(),
    }
}

fn rejected_optional(error: &str) -> Option<&'static str> {
    let lower = error.to_ascii_lowercase();
    if !(lower.starts_with("http 400") || lower.starts_with("http 422")) {
        return None;
    }
    let rejection = ["unknown", "unrecognized", "unrecognised", "unsupported", "not supported",
        "not permitted", "not allowed", "unexpected", "extra inputs", "extra fields"]
        .iter().any(|phrase| lower.contains(phrase));
    if !rejection { return None; }
    OPTIONAL_FIELDS.iter().copied().find(|field| {
        lower.contains(field) || (*field == "thinking.display" && lower.contains("display") && lower.contains("thinking"))
    })
}

/// Compatibility retries occur only on an HTTP rejection, before a response stream is opened.
async fn post_optional(
    endpoint: &str,
    model: &str,
    mut body: Value,
    builder: impl Fn(&Value) -> Result<reqwest::RequestBuilder>,
) -> Result<reqwest::Response> {
    let key = (endpoint.to_owned(), model.to_owned());
    {
        let remembered = REJECTED_FIELDS.lock();
        for field in remembered.get(&key).into_iter().flatten() {
            remove_optional(&mut body, field);
        }
    }
    for _ in 0..=OPTIONAL_FIELDS.len() {
        match post_with_retry(|| builder(&body)).await {
            Ok(response) => return Ok(response),
            Err(error) => {
                let Some(field) = rejected_optional(&error.to_string()) else { return Err(error); };
                if !remove_optional(&mut body, field) { return Err(error); }
                REJECTED_FIELDS.lock()
                    .entry(key.clone()).or_default().insert(field);
            }
        }
    }
    bail!("provider rejected every optional request field for {model}")
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












fn anthropic_body(req: &Request<'_>, proxy: bool) -> Result<Value> {
    let fallbacks = req.opts.fallbacks;
    let tools: Vec<Value> = req.tools.iter().map(|tool| {
        let mut tool = tool.clone();
        if !proxy {
            tool["eager_input_streaming"] = json!(true);
        }
        
        tool
    }).collect();
    let mut body = json!({
        "model": req.model,
        "system": [{ "type": "text", "text": req.system }],
        "max_tokens": req.opts.max_output.filter(|n| *n > 0)
            .with_context(|| format!("{} has no output-token limit in provider metadata; refresh the provider model listing or configure metadata before using Anthropic Messages", req.model))?,
        "tools": tools,
    });
    body["messages"] = json!(req.messages);
    if req.opts.thinking {
        body["thinking"] = json!({ "type": "adaptive" });
        if let Some(display) = req.opts.thinking_display {
            body["thinking"]["display"] = json!(display);
        }
        
    }
    if let Some(effort) = req.opts.effort {
        body["output_config"] = json!({ "effort": effort });
    }
    if fallbacks {
        body["fallbacks"] = json!("default");
    }
    if matches!(req.opts.fast, Some(Fast::AnthropicSpeed)) {
        body["speed"] = json!("fast");
    }
    Ok(body)
}



async fn anthropic(
    http: &reqwest::Client,
    api_key: &str,
    base_url: &str,
    proxy: bool,
    req: &Request<'_>,
    on: &mut (dyn FnMut(Delta) + Send),
) -> Result<Response> {
    let mut body = anthropic_body(req, proxy)?;
    body["stream"] = json!(true);
    // Auto-placed on the last cacheable block.
    body["cache_control"] = json!({ "type": "ephemeral" });
    let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
    let res = post_optional(&url, req.model, body, |body| {
        let mut betas = vec![];
        if body.get("fallbacks").is_some() {
            betas.push("server-side-fallback-2026-07-01");
        }
        if body.get("speed").is_some() {
            betas.push("fast-mode-2026-02-01");
        }
        if req.opts.server_compaction && carries_compaction(req.messages) {
            betas.push(COMPACTION_BETA);
        }
        let payload = bytes::Bytes::from(serde_json::to_string(body)?);
        Ok(anthropic_request(http, &url, api_key, proxy, &betas).body(payload))
    }).await?;

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
                    let mut block = ev["content_block"].clone();
                    
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

const COMPACTION_BETA: &str = "compact-2026-09-04";


fn carries_compaction(messages: &[Value]) -> bool {
    messages.iter().any(|m| m["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "compaction")))
}

fn anthropic_request(http: &reqwest::Client, url: &str, api_key: &str, proxy: bool, betas: &[&str]) -> reqwest::RequestBuilder {
    let mut b = http
        .post(url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json");
    if !betas.is_empty() {
        b = b.header("anthropic-beta", betas.join(","));
    }
    if proxy {
        b = b.bearer_auth(api_key);
    }
    b
}

pub const SUMMARY_INSTRUCTIONS: &str = "Summarize this conversation so that a fresh instance of the agent can continue the work \
without it. Keep: the user's goals, constraints and preferences; decisions made and why; every file examined or changed \
(exact paths) and what changed; commands that matter and their results, including exact error messages; the current state \
of the task; open problems; and the immediate next step. Be specific and complete, not brief. Do not call any tools; \
respond with the summary text only.";

/// A compacted replacement for the whole history, plus the usage it cost.
pub struct Compacted {
    pub history: Vec<Value>,
    pub summary: String,
    pub usage: Value,
}

impl Provider {
    /// Summarizes the conversation into a replacement history. Anthropic models that support it
    /// use on-demand server compaction (a signed block, prompt cache and thinking stay valid);
    /// everything else gets client-side "simple compaction": the model writes a summary that
    /// replaces the history, with no earlier turns or thinking replayed.
    pub async fn compact(&self, http: &reqwest::Client, req: &Request<'_>) -> Result<Compacted> {
        if let Provider::Anthropic { api_key, base_url, proxy } = self {
            if req.opts.server_compaction {
                return server_compaction(http, api_key, base_url, *proxy, req).await;
            }
        }
        let mut messages = req.messages.to_vec();
        let ask = json!({ "type": "text", "text": SUMMARY_INSTRUCTIONS });
        match messages.last_mut() {
            Some(last) if last["role"] == "user" => match &mut last["content"] {
                Value::Array(a) => a.push(ask),
                other => *other = json!([{ "type": "text", "text": other.as_str().unwrap_or_default() }, ask]),
            },
            _ => messages.push(json!({ "role": "user", "content": [ask] })),
        }
        let res = self.stream(http, &Request { messages: &messages, ..*req }, &mut |_| {}).await?;
        let summary: String = res.content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
        if summary.trim().is_empty() {
            bail!("the model returned no summary (stop reason: {})", res.stop_reason);
        }
        let history = vec![json!({ "role": "user", "content": [{ "type": "text",
            "text": format!("<conversation-summary>\n{}\n</conversation-summary>", summary.trim()) }] })];
        Ok(Compacted { history, summary, usage: res.usage })
    }
}

async fn server_compaction(http: &reqwest::Client, api_key: &str, base_url: &str, proxy: bool, req: &Request<'_>) -> Result<Compacted> {
    let mut body = anthropic_body(req, proxy)?;
    // Compaction cannot carry context_management.
    body.as_object_mut().unwrap().remove("context_management");
    body.as_object_mut().unwrap().remove("fallbacks");
    body["compaction"] = json!({ "type": "summarize", "instructions": SUMMARY_INSTRUCTIONS });
    let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
    let res = post_optional(&url, req.model, body, |body| {
        let mut betas = vec![COMPACTION_BETA];
        if body.get("speed").is_some() { betas.push("fast-mode-2026-02-01"); }
        let payload = bytes::Bytes::from(serde_json::to_string(body)?);
        Ok(anthropic_request(http, &url, api_key, proxy, &betas).body(payload))
    }).await?;
    let v: Value = res.json().await?;
    if v["stop_reason"] != "compaction" {
        bail!("no summary came back (stop reason: {})", v["stop_reason"].as_str().unwrap_or("?"));
    }
    let block = v["content"].get(0).cloned().ok_or_else(|| anyhow!("compaction response had no block"))?;
    let summary = block["content"].as_str().unwrap_or_default().to_string();
    // Billing is reported per iteration; the top-level counts are zero.
    let mut usage = json!({ "input_tokens": 0, "output_tokens": 0 });
    for it in v["usage"]["iterations"].as_array().into_iter().flatten() {
        for k in ["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"] {
            let add = it[k].as_u64().unwrap_or(0);
            usage[k] = json!(usage[k].as_u64().unwrap_or(0) + add);
        }
    }
    Ok(Compacted { history: vec![json!({ "role": "assistant", "content": [block] })], summary, usage })
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
    if let Some(effort) = req.opts.effort {
        body["reasoning_effort"] = json!(effort);
    }
    if let Some(Fast::ServiceTier { tier, .. }) = req.opts.fast {
        body["service_tier"] = json!(tier);
    }
    if let Some(limit) = req.opts.max_output.filter(|n| *n > 0) {
        body["max_tokens"] = json!(limit);
    }
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let res = post_optional(&url, req.model, body, |body| {
        let b = http.post(&url).json(body);
        Ok(match api_key {
            Some(k) => b.bearer_auth(k),
            None => b,
        })
    }).await?;

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
                let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
                usage = json!({
                    "input_tokens": u["prompt_tokens"].as_u64().map(|tokens| tokens.saturating_sub(cached)),
                    "output_tokens": u["completion_tokens"],
                    "cache_read_input_tokens": cached,
                });
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


pub fn no_key(provider: &str) -> anyhow::Error {
    anyhow!("no API key for {provider}; run `sci-pi auth set {provider}` on this host (or set the env var)")
}

/// Model ids an OpenAI-style `/models` endpoint offers, with their context window when the
/// endpoint says (OpenRouter `context_length`, Anthropic `max_input_tokens`, vLLM `max_model_len`, …).
pub async fn list_models(http: &reqwest::Client, models_url: &str, api_key: Option<&str>) -> Result<Vec<(String, ModelInfo)>> {
    let mut b = http.get(models_url).timeout(Duration::from_secs(8));
    if let Some(k) = api_key {
        b = b.bearer_auth(k);
    }
    let res = b.send().await?;
    if !res.status().is_success() {
        bail!("{models_url}: HTTP {}", res.status());
    }
    let v: Value = res.json().await?;
    Ok(models::parse_openai_models(&v))
}


/// If an error says the request didn't fit the context window: `Some(the limit, if stated)`.
pub fn overflow(err: &str) -> Option<Option<u64>> {
    let e = err.to_lowercase();
    let hit = ["context length", "context_length", "maximum context", "context window", "prompt is too long", "too many tokens", "input is too long"]
        .iter()
        .any(|p| e.contains(p));
    if !hit {
        return None;
    }
    static LIMIT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?:maximum context length is|context window (?:of|is)|limit (?:of|is)|maximum of|max(?:imum)?[^0-9]{0,20})\s*([0-9][0-9,]{3,})").unwrap()
    });
    Some(LIMIT.captures(&e).and_then(|c| c[1].replace(',', "").parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_context_overflow_errors() {
        let openai = "HTTP 400: This model's maximum context length is 272,000 tokens. However, your messages resulted in 301,442 tokens.";
        assert_eq!(overflow(openai), Some(Some(272_000)));
        assert_eq!(overflow("HTTP 400: prompt is too long: 1050211 tokens > 1000000 maximum"), Some(None));
        assert_eq!(overflow("HTTP 429: rate limit exceeded"), None);
    }

    #[tokio::test]
    async fn rejected_optional_fields_are_remembered_per_endpoint_and_model() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for field in ["fallbacks", "service_tier"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let mut bodies = vec![];
                for attempt in 0..3 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let body_start;
                    let length;
                    loop {
                        let mut chunk = [0; 4096];
                        let n = socket.read(&mut chunk).await.unwrap();
                        assert!(n > 0);
                        bytes.extend_from_slice(&chunk[..n]);
                        if let Some(start) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                            body_start = start + 4;
                            let headers = String::from_utf8_lossy(&bytes[..start]).to_ascii_lowercase();
                            length = headers.lines().find_map(|line| line.strip_prefix("content-length:"))
                                .unwrap().trim().parse::<usize>().unwrap();
                            break;
                        }
                    }
                    while bytes.len() < body_start + length {
                        let mut chunk = [0; 4096];
                        let n = socket.read(&mut chunk).await.unwrap();
                        assert!(n > 0);
                        bytes.extend_from_slice(&chunk[..n]);
                    }
                    bodies.push(serde_json::from_slice::<Value>(&bytes[body_start..body_start + length]).unwrap());
                    let (status, response) = if attempt == 0 {
                        ("400 Bad Request", json!({ "error": { "message": format!("Unknown field: {field}") } }).to_string())
                    } else {
                        ("200 OK", "{}".to_string())
                    };
                    let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len());
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
                bodies
            });
            let http = reqwest::Client::new();
            let mut body = json!({ "model": "runtime-model" });
            body[field] = json!("default");
            for _ in 0..2 {
                post_optional(&url, "runtime-model", body.clone(), |body| Ok(http.post(&url).json(body))).await.unwrap();
            }
            let bodies = server.await.unwrap();
            assert!(bodies[0].get(field).is_some());
            assert!(bodies[1].get(field).is_none());
            assert!(bodies[2].get(field).is_none());
        }
        assert_eq!(rejected_optional("HTTP 400: invalid speed value"), None);
        assert_eq!(rejected_optional("HTTP 401: unsupported service_tier"), None);
        assert_eq!(rejected_optional("HTTP 400: unsupported model"), None);
    }
    #[test]
    fn options_follow_metadata_without_model_name_rules() {
        let info = ModelInfo {
            efforts: vec![("custom".into(), None), ("other".into(), None)],
            default_effort: Some("other".into()),
            adaptive_thinking: Some(false),
            ..Default::default()
        };
        let opts = Opts::from_info(&info, "unknown", true);
        assert_eq!(opts.effort, Some("other"));
        assert!(!opts.thinking);
        assert!(opts.fast.is_none());
        assert!(opts.max_output.is_none());
        assert_eq!(Opts::from_info(&ModelInfo::default(), "high", false).effort, None);
    }
}


