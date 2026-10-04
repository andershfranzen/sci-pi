//! Minimal Agent Client Protocol client: JSON-RPC 2.0 over an adapter's stdio.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

pub const PROTOCOL_VERSION: u64 = 1;

/// This binary's path. After an upgrade replaces the file, Linux reports the running image as
/// "<path> (deleted)"; the new binary at that path is the one to launch.
fn self_exe() -> Result<String> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    Ok(exe.strip_suffix(" (deleted)").map(str::to_string).unwrap_or(exe))
}

/// Something the agent sent us that isn't a response to one of our requests.
#[derive(Debug)]
pub enum Incoming {
    Notification { method: String, params: Value },
    Request { id: Value, method: String, params: Value },
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

pub struct Conn {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    child: tokio::sync::Mutex<Child>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

impl Conn {
    /// Spawns the adapter in its own process group (so `npx` and its node child die together).
    pub fn spawn(
        command: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Result<(Arc<Conn>, mpsc::UnboundedReceiver<Incoming>)> {
        let (program, args) = command.split_first().ok_or_else(|| anyhow!("empty agent command"))?;
        // "@self" is this binary: the native agent runs as `outpost acp`.
        let program = if program == "@self" { self_exe()? } else { program.clone() };
        let mut child = Command::new(&program)
            .args(args)
            .envs(env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("failed to start `{program}`: {e}"))?;

        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = out_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = stdin.flush().await;
            }
        });

        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "agent_stderr", "{line}");
                let mut t = tail.lock().unwrap();
                t.push_back(line);
                if t.len() > 20 {
                    t.pop_front();
                }
            }
        });

        let pending: Pending = Arc::default();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    tracing::warn!("non-JSON line from agent: {line}");
                    continue;
                };
                let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
                let id = msg.get("id").cloned();
                match (method, id) {
                    (Some(method), Some(id)) => {
                        let params = msg.get("params").cloned().unwrap_or(Value::Null);
                        let _ = in_tx.send(Incoming::Request { id, method, params });
                    }
                    (Some(method), None) => {
                        let params = msg.get("params").cloned().unwrap_or(Value::Null);
                        let _ = in_tx.send(Incoming::Notification { method, params });
                    }
                    (None, Some(id)) => {
                        let Some(id) = id.as_u64() else { continue };
                        let Some(tx) = reader_pending.lock().unwrap().remove(&id) else { continue };
                        let result = match msg.get("error") {
                            Some(err) => Err(anyhow!(
                                "{}",
                                err.get("message").and_then(Value::as_str).unwrap_or("agent error")
                            )),
                            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(result);
                    }
                    (None, None) => {}
                }
            }
            // EOF: fail everything still waiting; dropping in_tx tells the session the agent is gone.
            for (_, tx) in reader_pending.lock().unwrap().drain() {
                let _ = tx.send(Err(anyhow!("agent exited")));
            }
        });

        let conn = Conn {
            out: out_tx,
            pending,
            next_id: AtomicU64::new(1),
            child: tokio::sync::Mutex::new(child),
            stderr_tail,
        };
        Ok((Arc::new(conn), in_rx))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        match rx.await {
            Ok(r) => r,
            Err(_) => bail!("agent exited"),
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    pub fn respond(&self, id: Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn respond_error(&self, id: Value, code: i64, message: &str) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }));
    }

    fn send(&self, msg: Value) {
        let _ = self.out.send(msg.to_string());
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n")
    }

    /// SIGTERM the whole process group, then make sure the direct child is reaped.
    pub async fn kill(&self) {
        let mut child = self.child.lock().await;
        if let Some(pid) = child.id() {
            let _ = std::process::Command::new("kill").args(["-TERM", &format!("-{pid}")]).status();
        }
        if tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
    }
}
