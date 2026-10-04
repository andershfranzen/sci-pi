//! Session supervision. Each live session is an actor task that owns one ACP adapter process,
//! turns its traffic into events, and keeps running no matter which clients are connected.

use crate::acp::{self, Conn, Incoming};
use crate::config::{expand_tilde, now_ms, Config};
use crate::git;
use crate::model::{Attachment, Event, Mode, QueuedPrompt, Session, Status, WsMsg};
use crate::store::Store;
use crate::terminal::Terminal;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(180);

enum Cmd {
    /// Start the next queued prompt if no turn is running.
    Kick,
    /// Cancel the running turn and drop the queue.
    Cancel,
    /// Cancel the running turn but keep the queue (used to "send now").
    Interrupt,
    Permission { request_id: String, option_id: Option<String> },
    SetMode(String),
    SetConfig { config_id: String, value: Value },
    Stop,
}

struct Handle {
    state: Mutex<Session>,
    actor: Mutex<Option<mpsc::UnboundedSender<Cmd>>>,
    deleted: AtomicBool,
}

pub struct Manager {
    pub store: Arc<Store>,
    pub cfg: Config,
    pub tx: broadcast::Sender<WsMsg>,
    sessions: Mutex<HashMap<String, Arc<Handle>>>,
    terminals: Mutex<HashMap<String, Arc<Terminal>>>,
    worktrees_dir: PathBuf,
    pub attachments_dir: PathBuf,
    http: reqwest::Client,
}

#[derive(Deserialize)]
pub struct CreateReq {
    pub agent: String,
    pub project: String,
    #[serde(default)]
    pub worktree: bool,
    pub title: Option<String>,
    pub mode: Option<String>,
    pub prompt: Option<String>,
    #[serde(default)]
    pub attachments: Vec<AttachmentIn>,
}

#[derive(Deserialize)]
pub struct PromptReq {
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<AttachmentIn>,
}

/// An attachment as uploaded by a client.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachmentIn {
    Image { mime_type: String, data: String },
    File { path: String },
}

#[derive(Deserialize)]
pub struct PatchReq {
    pub title: Option<String>,
    pub pinned: Option<bool>,
    pub archived: Option<bool>,
}

impl Manager {
    /// Loads persisted sessions. Nothing survives a daemon restart except the log, so live
    /// sessions become `detached` (they resume on the next prompt) and dangling approvals are
    /// closed out. Call [`Manager::resume_queues`] once the runtime is up.
    pub fn new(cfg: Config, store: Arc<Store>, data_dir: &Path) -> Result<Arc<Self>> {
        let (tx, _) = broadcast::channel(4096);
        let attachments_dir = data_dir.join("attachments");
        std::fs::create_dir_all(&attachments_dir)?;
        let mgr = Arc::new(Manager {
            store,
            cfg,
            tx,
            sessions: Mutex::default(),
            terminals: Mutex::default(),
            worktrees_dir: data_dir.join("worktrees"),
            attachments_dir,
            http: reqwest::Client::new(),
        });
        for mut s in mgr.store.sessions()? {
            for ev in mgr.store.unresolved_permissions(Some(&s.id))? {
                let request_id = ev.data["request_id"].clone();
                mgr.store.append(
                    &s.id,
                    "permission_resolved",
                    json!({ "request_id": request_id, "outcome": "cancelled", "option_id": null }),
                )?;
            }
            if matches!(s.status, Status::Starting | Status::Idle | Status::Running | Status::AwaitingPermission) {
                if matches!(s.status, Status::Running | Status::AwaitingPermission) {
                    s.status_message = Some("Daemon restarted during a turn; send a prompt to continue.".into());
                }
                s.status = Status::Detached;
            }
            s.pending_permissions = 0;
            mgr.store.save_session(&s)?;
            let handle = Handle { state: Mutex::new(s.clone()), actor: Mutex::new(None), deleted: AtomicBool::new(false) };
            mgr.sessions.lock().unwrap().insert(s.id.clone(), Arc::new(handle));
        }
        Ok(mgr)
    }

    /// Queued prompts are durable: pick them back up after a restart.
    pub fn resume_queues(self: &Arc<Self>) {
        let ids: Vec<String> = self.list().into_iter().filter(|s| !s.queue.is_empty()).map(|s| s.id).collect();
        for id in ids {
            let _ = self.send(&id, Cmd::Kick, true);
        }
    }

    pub fn list(&self) -> Vec<Session> {
        let mut out: Vec<Session> =
            self.sessions.lock().unwrap().values().map(|h| h.state.lock().unwrap().clone()).collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        out
    }

    pub fn get(&self, id: &str) -> Option<Session> {
        self.handle(id).map(|h| h.state.lock().unwrap().clone())
    }

    pub fn require_session(&self, id: &str) -> Result<Session> {
        self.get(id).ok_or_else(|| anyhow!("no such session"))
    }

    fn handle(&self, id: &str) -> Option<Arc<Handle>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    fn require(&self, id: &str) -> Result<Arc<Handle>> {
        self.handle(id).ok_or_else(|| anyhow!("no such session"))
    }

    pub async fn create(self: &Arc<Self>, req: CreateReq) -> Result<Session> {
        if !self.cfg.agents.contains_key(&req.agent) {
            bail!("unknown agent `{}`", req.agent);
        }
        let project = expand_tilde(&req.project);
        let project = project.canonicalize().with_context(|| format!("{} does not exist", project.display()))?;
        if !project.is_dir() {
            bail!("{} is not a directory", project.display());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (cwd, branch, base_commit) = if req.worktree {
            let wt = git::create_worktree(&project, &self.worktrees_dir, &id).await?;
            (wt.path, Some(wt.branch), Some(wt.base_commit))
        } else {
            (project.clone(), None, None)
        };
        let (title, title_locked) = match (&req.title, &req.prompt) {
            (Some(t), _) if !t.trim().is_empty() => (t.trim().to_string(), true),
            (_, Some(p)) if !p.trim().is_empty() => (title_from_prompt(p), false),
            _ => (project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), false),
        };
        let now = now_ms();
        let session = Session {
            id: id.clone(),
            title,
            agent: req.agent,
            project: project.to_string_lossy().into_owned(),
            cwd: cwd.to_string_lossy().into_owned(),
            branch,
            base_commit,
            status: Status::Starting,
            status_message: None,
            mode: req.mode.filter(|m| !m.is_empty()),
            modes: vec![],
            usage: None,
            queue: vec![],
            queued: 0,
            config_options: vec![],
            commands: vec![],
            prompt_caps: Value::Null,
            turns: 0,
            pinned: false,
            archived: false,
            pr_url: None,
            pending_permissions: 0,
            created_at: now,
            updated_at: now,
            acp_session_id: None,
            title_locked,
            agent_note: None,
        };
        self.store.save_session(&session)?;
        let handle = Arc::new(Handle {
            state: Mutex::new(session.clone()),
            actor: Mutex::new(None),
            deleted: AtomicBool::new(false),
        });
        self.sessions.lock().unwrap().insert(id.clone(), handle);
        let _ = self.tx.send(WsMsg::Session { session: session.clone() });
        // Start the adapter right away so a broken agent setup shows up immediately.
        match req.prompt.filter(|p| !p.trim().is_empty()) {
            Some(text) => {
                self.prompt(&id, PromptReq { text, attachments: req.attachments })?;
            }
            None => {
                self.ensure_actor(&id)?;
            }
        }
        Ok(self.get(&id).unwrap_or(session))
    }

    /// Queues a prompt (it runs immediately if the agent is free). Returns whether it had to wait.
    pub fn prompt(self: &Arc<Self>, id: &str, req: PromptReq) -> Result<bool> {
        let h = self.require(id)?;
        let attachments = req.attachments.into_iter().map(|a| self.store_attachment(a)).collect::<Result<Vec<_>>>()?;
        let item = QueuedPrompt { id: uuid::Uuid::new_v4().simple().to_string()[..10].to_string(), text: req.text, attachments };
        let s = self.update(&h, |s| {
            s.queue.push(item);
            s.queued = s.queue.len();
        });
        let busy = matches!(s.status, Status::Running | Status::AwaitingPermission) || s.queue.len() > 1;
        self.send(id, Cmd::Kick, true)?;
        Ok(busy)
    }

    fn store_attachment(&self, a: AttachmentIn) -> Result<Attachment> {
        Ok(match a {
            AttachmentIn::Image { mime_type, data } => {
                let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).context("image data is not base64")?;
                let ext = mime_guess::get_mime_extensions_str(&mime_type).and_then(|e| e.first()).copied().unwrap_or("bin");
                let name = format!("{}.{ext}", uuid::Uuid::new_v4().simple());
                std::fs::write(self.attachments_dir.join(&name), bytes)?;
                Attachment::Image { name, mime_type }
            }
            AttachmentIn::File { path } => Attachment::File { path },
        })
    }

    pub fn queue_delete(&self, id: &str, qid: &str) -> Result<()> {
        let h = self.require(id)?;
        self.update(&h, |s| {
            s.queue.retain(|q| q.id != qid);
            s.queued = s.queue.len();
        });
        Ok(())
    }

    pub fn queue_edit(&self, id: &str, qid: &str, text: String) -> Result<()> {
        let h = self.require(id)?;
        self.update(&h, |s| {
            if let Some(q) = s.queue.iter_mut().find(|q| q.id == qid) {
                q.text = text;
            }
        });
        Ok(())
    }

    /// Moves a queued prompt to the front and interrupts the running turn so it runs now.
    pub fn queue_send_now(self: &Arc<Self>, id: &str, qid: &str) -> Result<()> {
        let h = self.require(id)?;
        let s = self.update(&h, |s| {
            if let Some(i) = s.queue.iter().position(|q| q.id == qid) {
                let q = s.queue.remove(i);
                s.queue.insert(0, q);
            }
        });
        let busy = matches!(s.status, Status::Running | Status::AwaitingPermission);
        self.send(id, if busy { Cmd::Interrupt } else { Cmd::Kick }, true)
    }

    pub fn patch(&self, id: &str, req: PatchReq) -> Result<Session> {
        let h = self.require(id)?;
        Ok(self.update(&h, |s| {
            if let Some(t) = req.title.filter(|t| !t.trim().is_empty()) {
                s.title = t.trim().to_string();
                s.title_locked = true;
            }
            if let Some(p) = req.pinned {
                s.pinned = p;
            }
            if let Some(a) = req.archived {
                s.archived = a;
            }
        }))
    }

    pub fn cancel(self: &Arc<Self>, id: &str) -> Result<()> {
        let h = self.require(id)?;
        self.update(&h, |s| {
            s.queue.clear();
            s.queued = 0;
        });
        self.send(id, Cmd::Cancel, false)
    }

    pub fn permission(self: &Arc<Self>, id: &str, request_id: String, option_id: Option<String>) -> Result<()> {
        let open = self.store.unresolved_permissions(Some(id))?;
        if !open.iter().any(|e| e.data["request_id"] == request_id.as_str()) {
            bail!("that request is no longer pending");
        }
        self.send(id, Cmd::Permission { request_id, option_id }, false)
    }

    pub fn set_mode(self: &Arc<Self>, id: &str, mode: String) -> Result<()> {
        let h = self.require(id)?;
        if h.actor.lock().unwrap().as_ref().is_some_and(|tx| !tx.is_closed()) {
            self.send(id, Cmd::SetMode(mode), false)
        } else {
            // Applied when the adapter next starts.
            self.update(&h, |s| s.mode = Some(mode));
            Ok(())
        }
    }

    /// Model, effort, … – anything the agent lists in `configOptions`. Starts the adapter if needed.
    pub fn set_config(self: &Arc<Self>, id: &str, config_id: String, value: Value) -> Result<()> {
        self.send(id, Cmd::SetConfig { config_id, value }, true)
    }

    pub fn stop(self: &Arc<Self>, id: &str) -> Result<()> {
        self.send(id, Cmd::Stop, false)
    }

    /// Restores the working tree to the checkpoint taken just before turn `turn` started.
    pub async fn revert(&self, id: &str, turn: u32) -> Result<()> {
        let h = self.require(id)?;
        let s = h.state.lock().unwrap().clone();
        if matches!(s.status, Status::Running | Status::AwaitingPermission | Status::Starting) {
            bail!("wait for the current turn to finish (or cancel it) before reverting");
        }
        let checkpoint = self
            .store
            .turn_event(id, "user_prompt", turn)?
            .and_then(|e| e.data["checkpoint"].as_str().map(str::to_string))
            .ok_or_else(|| anyhow!("no checkpoint for turn {turn}"))?;
        git::restore(Path::new(&s.cwd), &checkpoint).await?;
        self.emit(&h, "reverted", json!({ "turn": turn, "checkpoint": checkpoint }));
        self.update(&h, |s| {
            s.agent_note = Some(format!(
                "[outpost: the user reverted the working tree to how it was before their prompt #{turn}. \
                 Every file change made since then has been undone; re-read files before editing them.]"
            ))
        });
        Ok(())
    }

    /// The diff produced by one turn (checkpoint before → checkpoint after, or the live tree).
    pub async fn turn_diff(&self, id: &str, turn: u32) -> Result<Value> {
        let s = self.require_session(id)?;
        let start = self
            .store
            .turn_event(id, "user_prompt", turn)?
            .and_then(|e| e.data["checkpoint"].as_str().map(str::to_string))
            .ok_or_else(|| anyhow!("no checkpoint for turn {turn}"))?;
        let end = match self.store.turn_event(id, "turn_end", turn)?.and_then(|e| e.data["checkpoint"].as_str().map(str::to_string)) {
            Some(end) => end,
            None => git::snapshot(Path::new(&s.cwd), None).await?.unwrap_or_default(),
        };
        git::diff_between(Path::new(&s.cwd), &start, &end).await
    }

    pub fn set_pr_url(&self, id: &str, url: String) -> Result<()> {
        let h = self.require(id)?;
        self.emit(&h, "pr_created", json!({ "url": url }));
        self.update(&h, |s| s.pr_url = Some(url));
        Ok(())
    }

    pub fn emit_for(&self, id: &str, kind: &str, data: Value) -> Result<()> {
        let h = self.require(id)?;
        self.emit(&h, kind, data);
        Ok(())
    }

    /// The session's persistent shell, spawned on first use (or after it exited).
    pub fn terminal(&self, id: &str) -> Result<Arc<Terminal>> {
        let s = self.require_session(id)?;
        let mut terms = self.terminals.lock().unwrap();
        if let Some(t) = terms.get(id).filter(|t| t.alive()) {
            return Ok(t.clone());
        }
        let t = Terminal::spawn(Path::new(&s.cwd))?;
        terms.insert(id.to_string(), t.clone());
        Ok(t)
    }

    pub fn kill_terminal(&self, id: &str) {
        if let Some(t) = self.terminals.lock().unwrap().remove(id) {
            t.kill();
        }
    }

    pub async fn delete(self: &Arc<Self>, id: &str, remove_worktree: bool) -> Result<()> {
        let h = self.require(id)?;
        h.deleted.store(true, Ordering::SeqCst);
        let actor = h.actor.lock().unwrap().take();
        if let Some(tx) = actor {
            let _ = tx.send(Cmd::Stop);
            // Give the adapter a moment to exit before pulling its worktree out from under it.
            for _ in 0..50 {
                if tx.is_closed() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        self.kill_terminal(id);
        let s = h.state.lock().unwrap().clone();
        git::delete_refs(Path::new(&s.project), &format!("refs/outpost/{id}/")).await;
        if remove_worktree {
            if let Some(branch) = &s.branch {
                git::remove_worktree(Path::new(&s.project), Path::new(&s.cwd), branch).await?;
            }
        }
        self.store.delete_session(id)?;
        self.sessions.lock().unwrap().remove(id);
        let _ = self.tx.send(WsMsg::SessionDeleted { id: id.into() });
        Ok(())
    }

    pub fn inbox(&self) -> Result<Vec<Value>> {
        Ok(self
            .store
            .unresolved_permissions(None)?
            .into_iter()
            .filter_map(|e| {
                let title = self.get(&e.session_id)?.title;
                Some(json!({
                    "session_id": e.session_id,
                    "session_title": title,
                    "request_id": e.data["request_id"],
                    "tool_call": e.data["tool_call"],
                    "options": e.data["options"],
                    "ts": e.ts,
                }))
            })
            .collect())
    }

    /// Delivers a command to the session's actor, starting one if `start` and none is running.
    fn send(self: &Arc<Self>, id: &str, cmd: Cmd, start: bool) -> Result<()> {
        let tx = if start { Some(self.ensure_actor(id)?) } else { self.require(id)?.actor.lock().unwrap().clone() };
        match tx {
            Some(tx) if tx.send(cmd).is_ok() => Ok(()),
            _ if start => bail!("session actor went away; try again"),
            _ => Ok(()), // nothing running: Stop/Cancel/Permission are no-ops
        }
    }

    fn ensure_actor(self: &Arc<Self>, id: &str) -> Result<mpsc::UnboundedSender<Cmd>> {
        let h = self.require(id)?;
        let mut slot = h.actor.lock().unwrap();
        if let Some(tx) = slot.as_ref().filter(|tx| !tx.is_closed()) {
            return Ok(tx.clone());
        }
        let (tx, rx) = mpsc::unbounded_channel();
        *slot = Some(tx.clone());
        tokio::spawn(run_actor(self.clone(), h.clone(), rx));
        Ok(tx)
    }

    /// Mutates a session row, persists it and tells every client.
    fn update(&self, h: &Handle, f: impl FnOnce(&mut Session)) -> Session {
        let snapshot = {
            let mut s = h.state.lock().unwrap();
            f(&mut s);
            s.updated_at = now_ms();
            s.clone()
        };
        if !h.deleted.load(Ordering::SeqCst) {
            if let Err(e) = self.store.save_session(&snapshot) {
                tracing::error!("saving session: {e:#}");
            }
            let _ = self.tx.send(WsMsg::Session { session: snapshot.clone() });
        }
        snapshot
    }

    fn emit(&self, h: &Handle, kind: &str, data: Value) -> Option<Event> {
        if h.deleted.load(Ordering::SeqCst) {
            return None;
        }
        let id = h.state.lock().unwrap().id.clone();
        match self.store.append(&id, kind, data) {
            Ok(event) => {
                let _ = self.tx.send(WsMsg::Event { event: event.clone() });
                Some(event)
            }
            Err(e) => {
                tracing::error!("appending event: {e:#}");
                None
            }
        }
    }

    fn push(&self, title: String, body: String, priority: &'static str, tags: &'static str) {
        let Some(url) = self.cfg.ntfy_url.clone() else { return };
        let http = self.http.clone();
        tokio::spawn(async move {
            let res = http
                .post(&url)
                .header("Title", title)
                .header("Priority", priority)
                .header("Tags", tags)
                .body(body)
                .send()
                .await;
            if let Err(e) = res {
                tracing::warn!("ntfy push failed: {e}");
            }
        });
    }
}

fn title_from_prompt(prompt: &str) -> String {
    let line = prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if line.chars().count() > 60 {
        format!("{}…", line.chars().take(60).collect::<String>())
    } else {
        line.to_string()
    }
}

async fn run_actor(mgr: Arc<Manager>, h: Arc<Handle>, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let mut actor = Actor { mgr: mgr.clone(), h: h.clone(), conn: None, acp_id: String::new(), pending: HashMap::new(), turn: None };
    let result = actor.run(&mut rx).await;
    if let Some(conn) = actor.conn.take() {
        conn.kill().await;
    }
    if let Err(e) = result {
        let msg = format!("{e:#}");
        tracing::warn!("session {} failed: {msg}", h.state.lock().unwrap().id);
        mgr.emit(&h, "error", json!({ "message": msg }));
        mgr.update(&h, |s| {
            s.status = Status::Error;
            s.status_message = Some(msg);
            s.pending_permissions = 0;
        });
    }
    // Detach from the handle (our receiver is still open, so the slot can only hold our own
    // sender or nothing), then let a fresh actor take over anything that raced in.
    *h.actor.lock().unwrap() = None;
    rx.close();
    let id = h.state.lock().unwrap().id.clone();
    let mut kicked = false;
    while let Ok(cmd) = rx.try_recv() {
        if matches!(cmd, Cmd::Kick) && !kicked && !h.deleted.load(Ordering::SeqCst) {
            kicked = true;
            let _ = mgr.send(&id, Cmd::Kick, true);
        }
    }
}

struct Actor {
    mgr: Arc<Manager>,
    h: Arc<Handle>,
    conn: Option<Arc<Conn>>,
    acp_id: String,
    /// Our request id → the agent's JSON-RPC id for that permission request.
    pending: HashMap<String, Value>,
    /// The running turn's number.
    turn: Option<u32>,
}

enum Flow {
    Continue,
    Stop,
}

impl Actor {
    fn session(&self) -> Session {
        self.h.state.lock().unwrap().clone()
    }

    fn update(&self, f: impl FnOnce(&mut Session)) -> Session {
        self.mgr.update(&self.h, f)
    }

    fn emit(&self, kind: &str, data: Value) {
        self.mgr.emit(&self.h, kind, data);
    }

    fn conn(&self) -> &Arc<Conn> {
        self.conn.as_ref().expect("connected")
    }

    async fn run(&mut self, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Result<()> {
        let s = self.session();
        let spec = self.mgr.cfg.agents.get(&s.agent).cloned().ok_or_else(|| anyhow!("unknown agent `{}`", s.agent))?;
        self.update(|s| {
            s.status = Status::Starting;
            s.status_message = None;
        });
        let (conn, mut incoming) = Conn::spawn(&spec.command, &spec.env, Path::new(&s.cwd))?;
        self.conn = Some(conn.clone());

        let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, self.open(&mut incoming)).await;
        match handshake {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e.context(conn.stderr_tail())),
            Err(_) => bail!("agent did not finish starting within {}s\n{}", HANDSHAKE_TIMEOUT.as_secs(), conn.stderr_tail()),
        }
        self.update(|s| {
            s.status = Status::Idle;
            s.status_message = None;
        });

        let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<Result<Value>>();
        self.kick(&turn_tx).await;
        loop {
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { return Ok(()) };
                    if let Flow::Stop = self.handle_cmd(cmd, &turn_tx).await {
                        return Ok(());
                    }
                }
                msg = incoming.recv() => match msg {
                    Some(msg) => self.handle_incoming(msg),
                    None => bail!("agent process exited\n{}", conn.stderr_tail()),
                },
                Some(res) = turn_rx.recv() => self.turn_done(res, &turn_tx).await,
            }
        }
    }

    /// initialize + resume the previous agent session if we have one, else start a new one.
    async fn open(&mut self, incoming: &mut mpsc::UnboundedReceiver<Incoming>) -> Result<()> {
        let conn = self.conn().clone();
        let init = conn
            .request(
                "initialize",
                json!({
                    "protocolVersion": acp::PROTOCOL_VERSION,
                    "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
                    "clientInfo": { "name": "outpost", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await
            .context("initialize")?;
        let caps = &init["agentCapabilities"];
        let s = self.session();
        let params = |id: &str| json!({ "sessionId": id, "cwd": s.cwd, "mcpServers": [] });

        let mut opened: Option<Value> = None;
        if let Some(prev) = &s.acp_session_id {
            if caps["sessionCapabilities"].get("resume").is_some() {
                opened = conn.request("session/resume", params(prev)).await.ok();
            }
            if opened.is_none() && caps["loadSession"] == true {
                if let Ok(res) = conn.request("session/load", params(prev)).await {
                    // `load` replays the whole conversation as session/update notifications;
                    // we already have it in the log, so drop the replay.
                    while let Ok(msg) = incoming.try_recv() {
                        if let Incoming::Request { .. } = msg {
                            self.handle_incoming(msg);
                        }
                    }
                    opened = Some(res);
                }
            }
            match opened {
                Some(_) => self.acp_id = prev.clone(),
                None => self.emit(
                    "status",
                    json!({ "status": "starting", "message": "Couldn't restore the agent's previous context; started a fresh agent session." }),
                ),
            }
        }
        let res = match opened {
            Some(res) => res,
            None => {
                let res = conn.request("session/new", json!({ "cwd": s.cwd, "mcpServers": [] })).await.context("session/new")?;
                self.acp_id = res["sessionId"].as_str().ok_or_else(|| anyhow!("session/new returned no sessionId"))?.to_string();
                res
            }
        };

        let modes: Vec<Mode> = res["modes"]["availableModes"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| serde_json::from_value(m.clone()).ok()).collect())
            .unwrap_or_default();
        let current = res["modes"]["currentModeId"].as_str().map(str::to_string);
        let config_options = res["configOptions"].as_array().cloned();
        let prompt_caps = caps["promptCapabilities"].clone();
        let acp_id = self.acp_id.clone();
        let wanted = s.mode.clone();
        self.update(|s| {
            s.acp_session_id = Some(acp_id);
            s.prompt_caps = prompt_caps;
            if !modes.is_empty() {
                s.modes = modes;
            }
            if let Some(opts) = config_options {
                s.config_options = opts;
            }
            if s.mode.is_none() {
                s.mode = current.clone();
            }
        });
        if let Some(wanted) = wanted.filter(|w| Some(w) != current.as_ref()) {
            self.set_mode(wanted).await;
        }
        Ok(())
    }

    async fn set_mode(&self, mode: String) {
        match self.conn().request("session/set_mode", json!({ "sessionId": self.acp_id, "modeId": mode })).await {
            Ok(_) => {
                self.update(|s| {
                    s.mode = Some(mode.clone());
                    set_config_value(&mut s.config_options, "mode", json!(mode));
                });
            }
            Err(e) => self.emit("error", json!({ "message": format!("couldn't switch mode: {e}") })),
        }
    }

    async fn set_config(&self, config_id: String, value: Value) {
        let params = json!({ "sessionId": self.acp_id, "configId": config_id, "value": value });
        match self.conn().request("session/set_config_option", params).await {
            Ok(res) => {
                let opts = res["configOptions"].as_array().cloned();
                self.update(|s| {
                    match opts {
                        Some(opts) => s.config_options = opts,
                        None => set_config_value(&mut s.config_options, &config_id, value.clone()),
                    }
                    if config_id == "mode" {
                        s.mode = value.as_str().map(str::to_string);
                    }
                });
            }
            Err(e) => self.emit("error", json!({ "message": format!("couldn't change {config_id}: {e}") })),
        }
    }

    async fn handle_cmd(&mut self, cmd: Cmd, turn_tx: &mpsc::UnboundedSender<Result<Value>>) -> Flow {
        match cmd {
            Cmd::Kick => self.kick(turn_tx).await,
            Cmd::Cancel | Cmd::Interrupt => {
                if self.turn.is_some() {
                    self.conn().notify("session/cancel", json!({ "sessionId": self.acp_id }));
                }
                self.cancel_pending();
            }
            Cmd::Permission { request_id, option_id } => self.resolve(&request_id, option_id),
            Cmd::SetMode(mode) => self.set_mode(mode).await,
            Cmd::SetConfig { config_id, value } => self.set_config(config_id, value).await,
            Cmd::Stop => {
                self.cancel_pending();
                if let Some(conn) = self.conn.take() {
                    conn.kill().await;
                }
                self.update(|s| {
                    s.status = Status::Stopped;
                    s.status_message = None;
                });
                return Flow::Stop;
            }
        }
        Flow::Continue
    }

    /// Starts the next queued prompt if the agent is free.
    async fn kick(&mut self, turn_tx: &mpsc::UnboundedSender<Result<Value>>) {
        if self.turn.is_some() {
            return;
        }
        let mut next = None;
        let s = self.update(|s| {
            if !s.queue.is_empty() {
                next = Some(s.queue.remove(0));
                s.queued = s.queue.len();
                s.turns += 1;
                // Same update as the pop, so nobody ever sees "idle with an empty queue" mid-handoff.
                s.status = Status::Running;
                s.status_message = None;
            }
        });
        let Some(item) = next else { return };
        let turn = s.turns;
        self.turn = Some(turn);
        let cwd = PathBuf::from(&s.cwd);

        let checkpoint = git::snapshot(&cwd, Some(&format!("refs/outpost/{}/{turn}-start", s.id))).await.ok().flatten();
        self.emit("user_prompt", json!({ "text": item.text, "attachments": item.attachments, "turn": turn, "checkpoint": checkpoint }));

        let mut text = item.text.clone();
        let note = s.agent_note.clone();
        if let Some(note) = &note {
            text = format!("{note}\n\n{text}");
        }
        let first = title_from_prompt(&item.text);
        self.update(|s| {
            s.agent_note = None;
            if s.title.is_empty() && !s.title_locked {
                s.title = first;
            }
        });

        let mut blocks = vec![json!({ "type": "text", "text": text })];
        for a in &item.attachments {
            match a {
                Attachment::Image { name, mime_type } => {
                    let path = self.mgr.attachments_dir.join(name);
                    if s.prompt_caps["image"] == true {
                        if let Ok(bytes) = std::fs::read(&path) {
                            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                            blocks.push(json!({ "type": "image", "mimeType": mime_type, "data": data }));
                        }
                    } else {
                        blocks.push(resource_link(&path, name));
                    }
                }
                Attachment::File { path } => blocks.push(resource_link(&cwd.join(path), path)),
            }
        }
        let conn = self.conn().clone();
        let params = json!({ "sessionId": self.acp_id, "prompt": blocks });
        let turn_tx = turn_tx.clone();
        tokio::spawn(async move {
            let _ = turn_tx.send(conn.request("session/prompt", params).await);
        });
    }

    async fn turn_done(&mut self, res: Result<Value>, turn_tx: &mpsc::UnboundedSender<Result<Value>>) {
        let turn = self.turn.take().unwrap_or_default();
        self.cancel_pending();
        let s = self.session();
        let checkpoint = git::snapshot(Path::new(&s.cwd), Some(&format!("refs/outpost/{}/{turn}-end", s.id))).await.ok().flatten();
        let stop_reason = match &res {
            Ok(v) => {
                let reason = v["stopReason"].as_str().unwrap_or("end_turn").to_string();
                self.emit("turn_end", json!({ "stop_reason": reason, "usage": v.get("usage"), "turn": turn, "checkpoint": checkpoint }));
                reason
            }
            Err(e) => {
                self.emit("error", json!({ "message": format!("{e:#}") }));
                self.emit("turn_end", json!({ "stop_reason": "error", "turn": turn, "checkpoint": checkpoint }));
                "error".into()
            }
        };
        if !self.session().queue.is_empty() {
            self.kick(turn_tx).await;
            return;
        }
        let s = self.update(|s| s.status = Status::Idle);
        if stop_reason != "cancelled" {
            let body = self
                .mgr
                .store
                .last_agent_text(&s.id)
                .ok()
                .flatten()
                .map(|t| t.chars().take(300).collect())
                .unwrap_or_else(|| format!("Stopped: {stop_reason}"));
            self.mgr.push(format!("Done: {}", s.title), body, "default", "white_check_mark");
        }
    }

    fn handle_incoming(&mut self, msg: Incoming) {
        match msg {
            Incoming::Notification { method, params } if method == "session/update" => {
                self.handle_update(params["update"].clone());
            }
            Incoming::Notification { .. } => {}
            Incoming::Request { id, method, params } if method == "session/request_permission" => {
                let request_id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
                self.pending.insert(request_id.clone(), id);
                let tool_call = params["toolCall"].clone();
                self.emit(
                    "permission_request",
                    json!({ "request_id": request_id, "tool_call": tool_call, "options": params["options"] }),
                );
                let n = self.pending.len();
                let s = self.update(|s| {
                    s.status = Status::AwaitingPermission;
                    s.pending_permissions = n;
                });
                let what = tool_call["title"].as_str().unwrap_or("a tool call").to_string();
                self.mgr.push(format!("Approval needed: {}", s.title), what, "high", "warning");
            }
            Incoming::Request { id, method, .. } => {
                self.conn().respond_error(id, -32601, &format!("outpost does not implement {method}"));
            }
        }
    }

    fn handle_update(&mut self, update: Value) {
        let kind = update["sessionUpdate"].as_str().unwrap_or_default().to_string();
        match kind.as_str() {
            "usage_update" => {
                let mut usage = update.clone();
                if let Some(o) = usage.as_object_mut() {
                    o.remove("sessionUpdate");
                    o.remove("_meta");
                }
                self.update(|s| s.usage = Some(usage));
            }
            "available_commands_update" => {
                let commands: Vec<Value> = update["availableCommands"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|c| json!({ "name": c["name"], "description": c["description"], "hint": c["input"]["hint"] }))
                            .collect()
                    })
                    .unwrap_or_default();
                self.update(|s| s.commands = commands);
            }
            "config_option_update" => {
                let Some(opts) = update["configOptions"].as_array().cloned() else { return };
                let mode = opts.iter().find(|o| o["category"] == "mode" || o["id"] == "mode").cloned();
                self.update(|s| {
                    if let Some(opt) = &mode {
                        if let Some(current) = opt["currentValue"].as_str() {
                            s.mode = Some(current.to_string());
                        }
                        let modes: Vec<Mode> = opt["options"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|o| {
                                        Some(Mode {
                                            id: o["value"].as_str()?.into(),
                                            name: o["name"].as_str().unwrap_or_default().into(),
                                            description: o["description"].as_str().map(str::to_string),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        if !modes.is_empty() {
                            s.modes = modes;
                        }
                    }
                    s.config_options = opts;
                });
            }
            "current_mode_update" => {
                let mode = update["currentModeId"].as_str().map(str::to_string);
                self.update(|s| {
                    if let Some(m) = &mode {
                        set_config_value(&mut s.config_options, "mode", json!(m));
                    }
                    s.mode = mode;
                });
                self.emit("update", update);
            }
            "session_info_update" => {
                if let Some(title) = update["title"].as_str().filter(|t| !t.is_empty()) {
                    let title = title.to_string();
                    self.update(|s| {
                        if !s.title_locked {
                            s.title = title;
                        }
                    });
                }
                self.emit("update", update);
            }
            _ => self.emit("update", update),
        }
    }

    fn resolve(&mut self, request_id: &str, option_id: Option<String>) {
        let Some(rpc_id) = self.pending.remove(request_id) else { return };
        let outcome = match &option_id {
            Some(opt) => json!({ "outcome": "selected", "optionId": opt }),
            None => json!({ "outcome": "cancelled" }),
        };
        self.conn().respond(rpc_id, json!({ "outcome": outcome }));
        self.emit(
            "permission_resolved",
            json!({
                "request_id": request_id,
                "outcome": if option_id.is_some() { "selected" } else { "cancelled" },
                "option_id": option_id,
            }),
        );
        let n = self.pending.len();
        let running = self.turn.is_some();
        self.update(|s| {
            s.pending_permissions = n;
            if n == 0 && s.status == Status::AwaitingPermission {
                s.status = if running { Status::Running } else { Status::Idle };
            }
        });
    }

    fn cancel_pending(&mut self) {
        let ids: Vec<String> = self.pending.keys().cloned().collect();
        for id in ids {
            self.resolve(&id, None);
        }
    }
}

fn set_config_value(opts: &mut [Value], id: &str, value: Value) {
    if let Some(o) = opts.iter_mut().find(|o| o["id"] == id) {
        o["currentValue"] = value;
    }
}

fn resource_link(path: &Path, name: &str) -> Value {
    json!({ "type": "resource_link", "uri": format!("file://{}", path.display()), "name": name })
}
