//! Real-binary provider boundaries: no provider SDK, external credentials, or global env changes.
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{fs, io::{BufRead, BufReader, Read, Write}, net::{TcpListener, TcpStream}, os::unix::fs::{OpenOptionsExt, PermissionsExt}, path::PathBuf, process::{Child, ChildStdin, Command, Stdio}, sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc}, thread, time::{Duration, Instant}};

const DEADLINE: Duration = Duration::from_secs(15);
const MODEL: &str = "claude-boundary-runtime";

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("scipi-boundary-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn put(&self, name: &str, value: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).unwrap();
        file.write_all(value.as_bytes()).unwrap();
    }
}
impl Drop for Scratch { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

#[derive(Clone, Debug)]
struct Request { path: String, headers: String, body: Value }
enum Reply { Json(u16, Value), Stream(String), Pending }
struct Fixture {
    url: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Request>>>,
    errors: Arc<Mutex<Vec<String>>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new(mut respond: impl FnMut(&Request, usize) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let (done, records, failures) = (stop.clone(), requests.clone(), errors.clone());
        let worker = thread::spawn(move || {
            let mut posts = 0;
            while !done.load(Ordering::Acquire) {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => { thread::sleep(Duration::from_millis(2)); continue; }
                    Err(e) => { failures.lock().push(e.to_string()); break; }
                };
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                let result = (|| -> std::io::Result<()> {
                    let request = read_request(&mut socket)?;
                    let reply = if request.path.contains("models") {
                        Reply::Json(200, json!({"data":[{"id":MODEL,"context_window":98765,"max_output_tokens":4321,"capabilities":{"compaction":{"supported":true}},"fallbacks":true},{"id":"boundary-open","context_window":76543,"max_output_tokens":3210}]}))
                    } else {
                        let index = posts;
                        posts += 1;
                        records.lock().push(request.clone());
                        respond(&request, index)
                    };
                    match reply {
                        Reply::Pending => { while !done.load(Ordering::Acquire) { thread::sleep(Duration::from_millis(2)); } }
                        Reply::Json(status, body) => send(&mut socket, status, "application/json", &body.to_string())?,
                        Reply::Stream(body) => send(&mut socket, 200, "text/event-stream", &body)?,
                    }
                    Ok(())
                })();
                if let Err(e) = result { failures.lock().push(e.to_string()); }
            }
        });
        Self { url, stop, requests, errors, worker: Some(worker) }
    }
    fn count(&self) -> usize { self.requests.lock().len() }
    fn wait_posts(&self, count: usize) {
        let deadline = Instant::now() + DEADLINE;
        while self.count() < count {
            assert!(Instant::now() < deadline, "HTTP request timed out: {:?}", self.errors.lock());
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) { self.stop.store(true, Ordering::Release); if let Some(worker) = self.worker.take() { let _ = worker.join(); } }
}
fn read_request(socket: &mut TcpStream) -> std::io::Result<Request> {
    let mut reader = BufReader::new(socket);
    let mut first = String::new(); reader.read_line(&mut first)?;
    let mut headers = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 { return Err(std::io::Error::other("EOF in HTTP headers")); }
        if line == "\r\n" { break; }
        if let Some((name, value)) = line.split_once(':') { if name.eq_ignore_ascii_case("content-length") { length = value.trim().parse().map_err(std::io::Error::other)?; } }
        headers.push_str(&line.to_ascii_lowercase());
    }
    if length > 4 * 1024 * 1024 { return Err(std::io::Error::other("fixture request too large")); }
    let mut body = vec![0; length]; reader.read_exact(&mut body)?;
    Ok(Request { path: first.split_whitespace().nth(1).unwrap_or_default().into(), headers, body: if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).map_err(std::io::Error::other)? } })
}
fn send(socket: &mut TcpStream, status: u16, kind: &str, body: &str) -> std::io::Result<()> {
    write!(socket, "HTTP/1.1 {status} Fixture\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
    socket.flush()
}
fn event(value: Value) -> String { format!("data: {value}\n\n") }
fn anthropic(blocks: Vec<Value>, stop: &str) -> String {
    let mut stream = event(json!({"type":"message_start","message":{"usage":{"input_tokens":17,"output_tokens":0}}}));
    for (index, block) in blocks.into_iter().enumerate() {
        let input = block.get("input").cloned();
        let text = block.get("text").and_then(Value::as_str).map(str::to_owned);
        let thinking = block.get("thinking").and_then(Value::as_str).map(str::to_owned);
        let signature = block.get("signature").and_then(Value::as_str).map(str::to_owned);
        let mut initial = block.clone();
        if input.is_some() { initial["input"] = json!({}); }
        if text.is_some() { initial["text"] = json!(""); }
        if thinking.is_some() { initial["thinking"] = json!(""); }
        if signature.is_some() { initial["signature"] = json!(""); }
        stream += &event(json!({"type":"content_block_start","index":index,"content_block":initial}));
        if let Some(input) = input { stream += &event(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":input.to_string()}})); }
        if let Some(text) = text { stream += &event(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}})); }
        if let Some(thinking) = thinking { stream += &event(json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":thinking}})); }
        if let Some(signature) = signature { stream += &event(json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":signature}})); }
        stream += &event(json!({"type":"content_block_stop","index":index}));
    }
    stream += &event(json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":11}}));
    stream += &event(json!({"type":"message_stop"}));
    stream
}
fn text(text: &str) -> Reply { Reply::Stream(anthropic(vec![json!({"type":"text","text":text})], "end_turn")) }
fn openai_text(text: &str) -> Reply { Reply::Stream(event(json!({"choices":[{"delta":{"content":text},"finish_reason":null}]})) + &event(json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":17,"completion_tokens":11}})) + "data: [DONE]\n\n") }

struct Driver { child: Child, input: ChildStdin, output: mpsc::Receiver<Value>, reader: Option<thread::JoinHandle<()>>, scratch: Scratch, next: u64, updates: Vec<Value>, session: String }
impl Driver {
    fn new(fixture: &Fixture, route: &str) -> Self {
        let scratch = Scratch::new();
        // OAuth uses the private runtime catalog rather than an API-key-only Models API.
        // These are fixture facts for an arbitrary model, never production defaults.
        scratch.put("data/agent/models.json", &json!({(MODEL):{"provider":"anthropic","window":98765,"max_output":4321,"efforts":[["fixture-effort",null]],"default_effort":"fixture-effort","server_compaction":true,"fallbacks":true}}).to_string());
        let model = match route { "key" | "oauth" => MODEL.to_string(), "proxy" => format!("fixture/{MODEL}"), _ => "fixture/boundary-open".into() };
        let provider = match route { "proxy" => format!("\n[native.providers.fixture]\nkind='cliproxy'\nbase_url='{}'\n", fixture.url), "openai" => format!("\n[native.providers.fixture]\nkind='openai'\nbase_url='{}/v1'\n", fixture.url), _ => String::new() };
        scratch.put("config.toml", &format!("[native]\nmodel='{model}'\n{provider}"));
        if route == "oauth" {
            scratch.put("anthropic-oauth.json", &json!({"access_token":"sk-ant-oat-fixture-only","refresh_token":"fixture-refresh","expires_at_ms":4102444800000_i64,"account_id":"fixture-account","email":"fixture@example.invalid","org_id":"fixture-org"}).to_string());
            scratch.put("anthropic-installation-id", "12345678-1234-4234-8234-123456789abc");
        } else { scratch.put("credentials.toml", "anthropic='sk-ant-fixture-only'\nfixture='fixture-only'\n"); }
        let mut command = Command::new(env!("CARGO_BIN_EXE_sci-pi"));
        command.arg("acp").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", &scratch.0).env("SCIPI_HOME", &scratch.0).env("ANTHROPIC_BASE_URL", &fixture.url).env("NO_PROXY", "127.0.0.1,localhost").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).current_dir(&scratch.0);
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, output) = mpsc::channel();
        let reader = thread::spawn(move || { for line in BufReader::new(stdout).lines() { let Ok(line) = line else { break }; if let Ok(value) = serde_json::from_str(&line) { if tx.send(value).is_err() { break; } } } });
        let mut driver = Self { child, input, output, reader: Some(reader), scratch, next: 100, updates: vec![], session: String::new() };
        let initialized = driver.call("initialize", json!({"protocolVersion":1,"clientCapabilities":{}}));
        assert!(initialized.get("result").is_some(), "{initialized}");
        let session = driver.call("session/new", json!({"cwd":driver.scratch.0,"mcpServers":[]}));
        driver.session = session["result"]["sessionId"].as_str().expect("new session").into();
        let mode = driver.call("session/set_mode", json!({"sessionId":driver.session,"modeId":"bypassPermissions"}));
        assert!(mode.get("result").is_some(), "{mode}");
        if route == "oauth" {
            // The catalog is loaded by a startup task. Synchronize through ACP's
            // consumer-visible runtime effort option, not a timing sleep.
            let deadline = Instant::now() + DEADLINE;
            loop {
                let options = driver.call("session/set_config_option", json!({"sessionId":driver.session,"configId":"model","value":MODEL}));
                if options["result"]["configOptions"].as_array().is_some_and(|a| a.iter().any(|v| v["id"] == "effort")) { break; }
                assert!(Instant::now() < deadline, "runtime OAuth catalog did not become available: {options}");
                thread::yield_now();
            }
        }
        driver
    }
    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        writeln!(self.input, "{}", json!({"jsonrpc":"2.0","id":self.next,"method":method,"params":params})).unwrap();
        self.input.flush().unwrap(); self.next
    }
    fn wait(&mut self, id: u64) -> Value {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let value = self.output.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("ACP response within finite deadline");
            assert!(value.get("method").is_none() || value.get("id").is_none(), "unexpected agent RPC {value}");
            if value["id"] == id { return value; }
            self.updates.push(value);
        }
    }
    fn call(&mut self, method: &str, params: Value) -> Value { let id = self.send(method, params); self.wait(id) }
    fn start_prompt(&mut self, prompt: &str) -> u64 { self.send("session/prompt", json!({"sessionId":self.session,"prompt":[{"type":"text","text":prompt}]})) }
    fn prompt(&mut self, prompt: &str) -> Value { let id = self.start_prompt(prompt); self.wait(id) }
    fn history(&self) -> Vec<Value> { serde_json::from_slice(&fs::read(self.scratch.0.join(format!("data/agent/{}.json",self.session))).unwrap()).unwrap() }
    fn saw_text(&self, expected: &str) -> bool { self.updates.iter().any(|v| v["params"]["update"]["sessionUpdate"] == "agent_message_chunk" && v["params"]["update"]["content"]["text"].as_str().is_some_and(|t| t.contains(expected))) }
}
impl Drop for Driver { fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); if let Some(reader) = self.reader.take() { let _ = reader.join(); } } }
fn assert_complete(reply: Value) { assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}"); }
fn assert_pairs(history: &[Value]) {
    for (index, message) in history.iter().enumerate() {
        for block in message["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "tool_use") {
            let next = history.get(index + 1).expect("tool use must have result");
            assert_eq!(next["role"], "user");
            let results: Vec<_> = next["content"].as_array().unwrap().iter().filter(|b| b["type"] == "tool_result" && b["tool_use_id"] == block["id"]).collect();
            assert_eq!(results.len(), 1, "exactly one terminal result per tool use");
        }
    }
}

#[test]
fn anthropic_routes_preserve_signed_rounds_and_compaction() {
    for route in ["key", "oauth", "proxy"] {
        let oauth = route == "oauth";
        let fixture = Fixture::new(move |request, index| {
            if request.body.get("compaction").is_some() {
                return Reply::Json(200, json!({"stop_reason":"compaction","content":[{"type":"compaction","content":"boundary compacted state","signature":"compaction-seal"}],"usage":{"iterations":[{"input_tokens":19,"output_tokens":7}]}}));
            }
            match index {
                0 | 1 => {
                    let name = if oauth { "_bash" } else { "bash" };
                    Reply::Stream(anthropic(vec![json!({"type":"thinking","thinking":"private reasoning","signature":"signed-thinking-exact"}), json!({"type":"tool_use","id":format!("effect-{index}"),"name":name,"input":{"command":format!("printf 'effect-{index}\\n' >> effects.txt"),"timeout":5}})], "tool_use"))
                }
                2 => text("rounds completed"),
                4 => text("compacted replay completed"),
                _ => Reply::Json(400, json!({"error":{"message":"unexpected extra attempt"}})),
            }
        });
        let mut driver = Driver::new(&fixture, route);
        assert_complete(driver.prompt("Run two one-time effects."));
        assert!(driver.saw_text("rounds completed"));
        assert_eq!(fs::read_to_string(driver.scratch.0.join("effects.txt")).unwrap(), "effect-0\neffect-1\n");
        let history = driver.history();
        assert_pairs(&history);
        assert_eq!(history.len(), 6);
        let thinking: Vec<_> = history.iter().flat_map(|m| m["content"].as_array().into_iter().flatten()).filter(|b| b["type"] == "thinking").collect();
        assert_eq!(thinking.len(), 2);
        assert!(thinking.iter().all(|b| b["signature"] == "signed-thinking-exact"));
        assert_complete(driver.prompt("/compact"));
        let compacted = driver.history();
        assert_eq!(compacted.len(), 1);
        assert_eq!(compacted[0]["content"][0]["type"], "compaction");
        assert_eq!(compacted[0]["content"][0]["signature"], "compaction-seal");
        assert_complete(driver.prompt("Continue from compacted state."));
        assert!(driver.saw_text("compacted replay completed"));
        assert_eq!(fixture.count(), 5);
        let records = fixture.requests.lock();
        assert!(records[1].body["messages"].to_string().contains("signed-thinking-exact"));
        assert!(records[3].body["messages"].to_string().contains("signed-thinking-exact"));
        assert!(records[4].body["messages"].to_string().contains("compaction-seal"));
        assert!(records.iter().all(|r| r.path == "/v1/messages"));
        if oauth { assert!(records[0].headers.contains("authorization: bearer sk-ant-oat-fixture-only")); }
        assert_eq!(fs::read_to_string(driver.scratch.0.join("effects.txt")).unwrap(), "effect-0\neffect-1\n");
    }
}

#[test]
fn openai_route_executes_multiple_tool_rounds_and_simple_compaction() {
    let fixture = Fixture::new(|_, index| match index {
        0 | 1 => Reply::Stream(event(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":format!("open-{index}"),"type":"function","function":{"name":"bash","arguments":json!({"command":format!("printf 'open-{index}\\n' >> effects.txt"),"timeout":5}).to_string()}}]},"finish_reason":null}]})) + &event(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})) + "data: [DONE]\n\n"),
        2 => openai_text("open rounds completed"),
        3 => openai_text("all effects completed; continue safely"),
        4 => openai_text("open compacted continuation"),
        _ => Reply::Json(400,json!({"error":{"message":"unexpected attempt"}})),
    });
    let mut driver = Driver::new(&fixture, "openai");
    assert_complete(driver.prompt("Run two effects."));
    assert!(driver.saw_text("open rounds completed"));
    assert_pairs(&driver.history());
    assert_eq!(fs::read_to_string(driver.scratch.0.join("effects.txt")).unwrap(), "open-0\nopen-1\n");
    assert_complete(driver.prompt("/compact"));
    let history = driver.history();
    assert_eq!(history.len(), 1);
    assert!(history[0]["content"][0]["text"].as_str().unwrap().contains("<conversation-summary>"));
    assert_complete(driver.prompt("Continue."));
    assert!(driver.saw_text("open compacted continuation"));
    assert_eq!(fixture.count(), 5);
    let records = fixture.requests.lock();
    assert!(records.iter().all(|r| r.path == "/v1/chat/completions"));
    assert_eq!(records[1].body["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").count(), 1);
    assert!(!records[4].body["messages"].to_string().contains("tool_call_id"));
}

#[test]
fn optional_rejection_is_bounded_and_remembered() {
    let fixture = Fixture::new(|request, _| {
        if request.body.get("fallbacks").is_some() { Reply::Json(400, json!({"error":{"message":"unknown field fallbacks"}})) }
        else { text("optional compatibility succeeded") }
    });
    let mut driver = Driver::new(&fixture, "key");
    assert_complete(driver.prompt("First turn."));
    assert_eq!(fixture.count(), 2);
    assert_complete(driver.prompt("Second turn."));
    assert_eq!(fixture.count(), 3);
    assert!(driver.saw_text("optional compatibility succeeded"));
    assert_eq!(driver.history().len(), 4);
    let records = fixture.requests.lock();
    assert!(records[0].body.get("fallbacks").is_some());
    assert!(records[1..].iter().all(|r| r.body.get("fallbacks").is_none()));
}

#[test]
fn repeated_optional_rejection_does_not_loop() {
    let fixture = Fixture::new(|_, _| Reply::Json(400, json!({"error":{"message":"unsupported field fallbacks"}})));
    let mut driver = Driver::new(&fixture, "key");
    let reply = driver.prompt("Fail safely.");
    assert!(reply.get("error").is_some(), "{reply}");
    assert_eq!(fixture.count(), 2);
    assert_eq!(driver.history().len(), 1);
}

#[test]
fn streamed_error_never_retries_or_executes_partial_tool() {
    for route in ["key", "oauth", "proxy", "openai"] {
        let open = route == "openai";
        let fixture = Fixture::new(move |_, _| {
            let body = if open {
                event(json!({"choices":[{"delta":{"content":"visible partial"},"finish_reason":null}]})) + &event(json!({"error":{"message":"context window exceeded, maximum 1234 tokens; unsupported field fallbacks"}}))
            } else {
                event(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})) + &event(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"visible partial"}})) + &event(json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"never-run","name":"bash","input":{}}})) + &event(json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"touch forbidden\"}"}})) + &event(json!({"type":"error","error":{"message":"context window exceeded, maximum 1234 tokens; unsupported field fallbacks"}}))
            };
            Reply::Stream(body)
        });
        let mut driver = Driver::new(&fixture, route);
        let reply = driver.prompt("Do not duplicate partial output.");
        assert!(reply.get("error").is_some(), "{route}: {reply}");
        assert!(driver.saw_text("visible partial"));
        assert_eq!(fixture.count(), 1, "{route}: no overflow/optional retry after visible output");
        assert!(!driver.scratch.0.join("forbidden").exists());
        assert_pairs(&driver.history());
    }
}

#[test]
fn premature_eof_never_retries_or_executes_an_unfinished_tool() {
    for route in ["key", "oauth", "proxy", "openai"] {
        let open = route == "openai";
        let fixture = Fixture::new(move |_, index| {
            if index > 0 {
                return if open {
                    Reply::Stream(event(json!({"choices":[{"delta":{"content":"unexpected continuation"},"finish_reason":"stop"}]})) + "data: [DONE]\n\n")
                } else { text("unexpected continuation") };
            }
            let args = r#"{"command":"touch forbidden"}"#;
            let body = if open {
                event(json!({"choices":[{"delta":{"content":"visible before EOF"},"finish_reason":null}]}))
                    + &event(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"unfinished","type":"function","function":{"name":"bash","arguments":args}}]},"finish_reason":null}]}))
            } else {
                event(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}))
                    + &event(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"visible before EOF"}}))
                    + &event(json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"unfinished","name":"bash","input":{}}}))
                    + &event(json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":args}}))
                    + &event(json!({"type":"content_block_stop","index":1}))
            };
            Reply::Stream(body)
        });
        let mut driver = Driver::new(&fixture, route);
        let reply = driver.prompt("Preserve partial output but do not execute an unfinished response.");
        assert!(reply.get("error").is_some(), "{route}: {reply}");
        assert!(driver.saw_text("visible before EOF"));
        assert_eq!(fixture.count(), 1, "{route}: incomplete streams must not be retransmitted");
        assert!(!driver.scratch.0.join("forbidden").exists(), "{route}: incomplete streams must not execute tools");
        assert_pairs(&driver.history());
    }
}

#[test]
fn cancellation_interrupts_pending_http_response() {
    for route in ["key", "oauth", "proxy", "openai"] {
        let fixture = Fixture::new(|_, _| Reply::Pending);
        let mut driver = Driver::new(&fixture, route);
        let id = driver.start_prompt("Wait for response.");
        fixture.wait_posts(1);
        writeln!(driver.input,"{}",json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":driver.session}})).unwrap();
        driver.input.flush().unwrap();
        let reply = driver.wait(id);
        assert_eq!(reply["result"]["stopReason"], "cancelled", "{route}: {reply}");
        assert_eq!(fixture.count(), 1);
        assert_eq!(driver.history().len(), 1);
        assert!(!driver.saw_text("Wait for response."));
    }
}

#[test]
fn proxy_added_cache_points_do_not_consume_a_fifth_slot() {
    let fixture = Fixture::new(|request, _| {
        // Emulate the proxy's four explicit breakpoints. A forwarded automatic cache
        // point would be a fifth and must produce a consumer-visible failure.
        let proxy_breakpoints = 4;
        let automatic = usize::from(request.body.get("cache_control").is_some());
        if proxy_breakpoints + automatic > 4 { Reply::Json(400,json!({"error":{"message":"too many cache_control blocks (maximum four)"}})) }
        else { text("proxy cache interaction succeeded") }
    });
    let mut driver = Driver::new(&fixture,"proxy");
    assert_complete(driver.prompt("Use the proxy cache."));
    assert!(driver.saw_text("proxy cache interaction succeeded"));
    assert_eq!(fixture.count(),1);
    assert_eq!(driver.history().len(),2);
}
