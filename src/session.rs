//! Session supervision. Each live session is an actor task that owns one ACP adapter process,
//! turns its traffic into events, and keeps running no matter which clients are connected.

use crate::acp::{self, Conn, Incoming};
use crate::config::{expand_tilde, now_ms, Config};
use crate::git;
use crate::model::{Attachment, Event, Mode, QueuedPrompt, Session, Status, TurnOutcome, TurnRecovery, WsMsg};
use crate::store::Store;
use crate::terminal::Terminal;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
            recover_restart(&mgr.store, &mut s)?;
            s.pending_permissions = 0;
            mgr.store.save_session(&s)?;
            let handle = Handle { state: Mutex::new(s.clone()), actor: Mutex::new(None), deleted: AtomicBool::new(false) };
            mgr.sessions.lock().insert(s.id.clone(), Arc::new(handle));
        }
        Ok(mgr)
    }

    /// Queued prompts are durable: pick them back up after a restart.
    pub fn resume_queues(self: &Arc<Self>) {
        let ids: Vec<String> = self.list().into_iter().filter(|s| !s.queue.is_empty() && !s.queue_paused).map(|s| s.id).collect();
        for id in ids {
            let _ = self.send(&id, Cmd::Kick, true);
        }
    }

    pub fn list(&self) -> Vec<Session> {
        let mut out: Vec<Session> =
            self.sessions.lock().values().map(|h| h.state.lock().clone()).collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        out
    }

    pub fn get(&self, id: &str) -> Option<Session> {
        self.handle(id).map(|h| h.state.lock().clone())
    }

    pub fn require_session(&self, id: &str) -> Result<Session> {
        self.get(id).ok_or_else(|| anyhow!("no such session"))
    }

    fn handle(&self, id: &str) -> Option<Arc<Handle>> {
        self.sessions.lock().get(id).cloned()
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
            recovery: None,
            request_diagnostics: vec![],
            queue_paused: false,
            retry_pending: false,
            last_prompt: None,
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
        self.sessions.lock().insert(id.clone(), handle);
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
        let item = QueuedPrompt { id: uuid::Uuid::new_v4().simple().to_string()[..10].to_string(), text: req.text, attachments, retry_of: None };
        let s = self.update(&h, |s| {
            s.queue.push(item);
            s.queued = s.queue.len();
        });
        let busy = s.queue_paused || matches!(s.status, Status::Running | Status::AwaitingPermission) || s.queue.len() > 1;
        self.send(id, Cmd::Kick, true)?;
        Ok(busy)
    }
    /// Repeats the last unsuccessful prompt as a new attempt, without undoing any effects.
    pub fn retry(self: &Arc<Self>, id: &str) -> Result<Session> {
        let h = self.require(id)?;
        let turn = {
            let mut s = h.state.lock();
            require_recovery_action(&s)?;
            if s.retry_pending {
                bail!("a retry is already pending; resume its queued attempt instead");
            }
            let recovery = s.recovery.as_ref().filter(|r| r.outcome != TurnOutcome::Completed)
                .ok_or_else(|| anyhow!("no unsuccessful turn to retry"))?;
            let turn = recovery.turn;
            let mut item = match s.last_prompt.clone() {
                Some(item) => item,
                None => {
                    let event = self.store.turn_event(id, "user_prompt", turn)?
                        .ok_or_else(|| anyhow!("the original prompt is unavailable"))?;
                    QueuedPrompt {
                        id: String::new(),
                        text: event.data["text"].as_str().ok_or_else(|| anyhow!("the original prompt text is unavailable"))?.to_string(),
                        attachments: match event.data.get("attachments") {
                            Some(value) => serde_json::from_value(value.clone()).context("the original attachments are unavailable")?,
                            None => vec![],
                        },
                        retry_of: None,
                    }
                }
            };
            item.id = uuid::Uuid::new_v4().simple().to_string()[..10].to_string();
            item.retry_of = Some(turn);
            s.queue.insert(0, item);
            s.queued = s.queue.len();
            s.retry_pending = true;
            s.queue_paused = false;
            s.updated_at = now_ms();
            self.store.save_session(&s)?;
            let _ = self.tx.send(WsMsg::Session { session: s.clone() });
            turn
        };
        self.emit(&h, "retry_requested", json!({
            "turn": turn,
            "message": "Retry starts a new attempt. Previous tool effects remain and may be repeated."
        }));
        self.send(id, Cmd::Kick, true)?;
        self.require_session(id)
    }

    /// Restarts the adapter and releases the retained queue; never repeats the failed prompt.
    pub fn resume(self: &Arc<Self>, id: &str) -> Result<Session> {
        let h = self.require(id)?;
        {
            let mut s = h.state.lock();
            require_recovery_action(&s)?;
            s.queue_paused = false;
            s.status = Status::Starting;
            s.updated_at = now_ms();
            self.store.save_session(&s)?;
            let _ = self.tx.send(WsMsg::Session { session: s.clone() });
        }
        self.emit(&h, "status", json!({ "status": "starting", "message": "Resuming the agent and queued prompts without repeating the previous attempt. Existing effects remain." }));
        self.send(id, Cmd::Kick, true)?;
        self.require_session(id)
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
            let mut retry_pending = false;
            s.queue.retain(|q| {
                if q.id == qid { return false; }
                retry_pending |= q.retry_of.is_some();
                true
            });
            s.retry_pending = retry_pending;
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
                s.queue_paused = false;
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
            s.retry_pending = false;
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
        if h.actor.lock().as_ref().is_some_and(|tx| !tx.is_closed()) {
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
        let s = h.state.lock().clone();
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
                "[sci-pi: the user reverted the working tree to how it was before their prompt #{turn}. \
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

    /// Branches a session at the end of `turn` (default: latest): same project and agent (or
    /// another one), files restored from that turn's checkpoint into a fresh worktree, and the
    /// conversation so far handed to the new agent from our own log – so it works for any agent.
    pub async fn fork(self: &Arc<Self>, id: &str, turn: Option<u32>, agent: Option<String>) -> Result<Session> {
        let src = self.require_session(id)?;
        let upto = turn.unwrap_or(src.turns);
        let events = self.store.events_after(0, Some(id), i64::MAX)?;
        let end_id = events
            .iter()
            .find(|e| e.kind == "turn_end" && e.data["turn"] == upto)
            .map(|e| e.id)
            .unwrap_or(i64::MAX);
        let checkpoint = events
            .iter()
            .find(|e| e.id == end_id)
            .and_then(|e| e.data["checkpoint"].as_str().map(str::to_string));
        let transcript = transcript(&events.iter().filter(|e| e.id <= end_id).cloned().collect::<Vec<_>>());

        let forked = self
            .create(CreateReq {
                agent: agent.unwrap_or(src.agent.clone()),
                project: src.project.clone(),
                worktree: src.branch.is_some(),
                title: Some(format!("{} (fork)", src.title)),
                mode: src.mode.clone(),
                prompt: None,
                attachments: vec![],
            })
            .await?;
        // A shared (non-worktree) directory already has the files; only restore isolated copies.
        if let (Some(cp), Some(_)) = (&checkpoint, &forked.branch) {
            git::restore(Path::new(&forked.cwd), cp).await?;
        }
        let h = self.require(&forked.id)?;
        self.emit(&h, "forked", json!({ "from": id, "from_title": src.title, "turn": upto }));
        Ok(self.update(&h, |s| {
            s.agent_note = Some(format!(
                "[sci-pi: this session is a fork of an earlier conversation, continued from its turn {upto}. \
                 The working tree already contains the files as they were at that point. \
                 Conversation so far, for context:]\n\n{transcript}\n\n[end of earlier conversation]"
            ));
        }))
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
        let mut terms = self.terminals.lock();
        if let Some(t) = terms.get(id).filter(|t| t.alive()) {
            return Ok(t.clone());
        }
        let t = Terminal::spawn(Path::new(&s.cwd))?;
        terms.insert(id.to_string(), t.clone());
        Ok(t)
    }

    pub fn kill_terminal(&self, id: &str) {
        if let Some(t) = self.terminals.lock().remove(id) {
            t.kill();
        }
    }

    pub async fn delete(self: &Arc<Self>, id: &str, remove_worktree: bool) -> Result<()> {
        let h = self.require(id)?;
        h.deleted.store(true, Ordering::SeqCst);
        let actor = h.actor.lock().take();
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
        let s = h.state.lock().clone();
        git::delete_refs(Path::new(&s.project), &format!("refs/sci-pi/{id}/")).await;
        if remove_worktree {
            if let Some(branch) = &s.branch {
                git::remove_worktree(Path::new(&s.project), Path::new(&s.cwd), branch).await?;
            }
        }
        self.store.delete_session(id)?;
        self.sessions.lock().remove(id);
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
        let tx = if start { Some(self.ensure_actor(id)?) } else { self.require(id)?.actor.lock().clone() };
        match tx {
            Some(tx) if tx.send(cmd).is_ok() => Ok(()),
            _ if start => bail!("session actor went away; try again"),
            _ => Ok(()), // nothing running: Stop/Cancel/Permission are no-ops
        }
    }

    fn ensure_actor(self: &Arc<Self>, id: &str) -> Result<mpsc::UnboundedSender<Cmd>> {
        let h = self.require(id)?;
        let mut slot = h.actor.lock();
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
            let mut s = h.state.lock();
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
        let id = h.state.lock().id.clone();
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

fn require_recovery_action(s: &Session) -> Result<()> {
    if matches!(s.status, Status::Running | Status::AwaitingPermission | Status::Starting) || s.pending_permissions > 0 {
        bail!("wait for the current turn or approval to finish before recovering");
    }
    Ok(())
}

fn take_next_prompt(s: &mut Session) -> Option<QueuedPrompt> {
    if s.queue_paused { return None; }
    if s.queue.is_empty() {
        if s.status == Status::Starting { s.status = Status::Idle; }
        return None;
    }
    let item = s.queue.remove(0);
    s.last_prompt = Some(item.clone());
    s.queued = s.queue.len();
    s.turns += 1;
    s.recovery = None;
    s.request_diagnostics.clear();
    if item.retry_of.is_some() { s.retry_pending = false; }
    s.status = Status::Running;
    s.status_message = None;
    Some(item)
}

fn observe_tool(tools: &mut HashMap<String, bool>, update: &Value) {
    if !matches!(update["sessionUpdate"].as_str(), Some("tool_call" | "tool_call_update")) {
        return;
    }
    observe_tool_call(tools, update);
}

fn observe_tool_call(tools: &mut HashMap<String, bool>, update: &Value) {
    let Some(id) = update["toolCallId"].as_str().filter(|id| !id.is_empty()) else { return };
    let completed = update["status"] == "completed";
    // Completion is monotonic; repeated or partial updates never count a tool twice.
    let entry = tools.entry(id.to_string()).or_default();
    *entry |= completed;
}

fn turn_recovery(turn: u32, outcome: TurnOutcome, tools: &HashMap<String, bool>) -> TurnRecovery {
    let message = match outcome {
        TurnOutcome::Completed => "Turn completed.",
        TurnOutcome::Failed => "Turn failed. Partial output is preserved. Tool effects may remain; retry does not roll them back.",
        TurnOutcome::Cancelled => "Turn cancelled. Partial output is preserved. Tool effects may remain; cancellation does not roll them back.",
        TurnOutcome::Interrupted => "Turn interrupted. Partial output is preserved. Tool effects may remain and their final state may be unknown.",
    };
    TurnRecovery {
        turn, outcome,
        completed_tools: tools.values().filter(|completed| **completed).count() as u32,
        started_tools: tools.len() as u32,
        resumable: outcome != TurnOutcome::Completed,
        message: message.to_string(),
    }
}

fn recover_restart(store: &Store, s: &mut Session) -> Result<()> {
    if matches!(s.status, Status::Running | Status::AwaitingPermission) {
        let ended = store.turn_event(&s.id, "turn_end", s.turns)?;
        let recovery = match ended {
            Some(end) => serde_json::from_value::<TurnRecovery>(end.data["recovery"].clone()).ok()
                .unwrap_or_else(|| turn_recovery(s.turns, match end.data["stop_reason"].as_str() {
                    Some("cancelled") => TurnOutcome::Cancelled,
                    Some("error" | "failed") => TurnOutcome::Failed,
                    Some("interrupted") => TurnOutcome::Interrupted,
                    _ => TurnOutcome::Completed,
                }, &HashMap::new())),
            None => {
                let mut tools = HashMap::new();
                if let Some(start) = store.turn_event(&s.id, "user_prompt", s.turns)? {
                    for event in store.events_after(start.id, Some(&s.id), i64::MAX)? {
                        if event.kind == "user_prompt" { break; }
                        if event.kind == "update" { observe_tool(&mut tools, &event.data); }
                    }
                }
                let mut recovery = turn_recovery(s.turns, TurnOutcome::Interrupted, &tools);
                recovery.message = format!("Daemon restarted during turn {}. {}", s.turns, recovery.message);
                store.append(&s.id, "turn_end", json!({
                    "turn": s.turns, "stop_reason": "interrupted", "checkpoint": null,
                    "recovery": recovery,
                }))?;
                recovery
            }
        };
        s.queue_paused = recovery.outcome != TurnOutcome::Completed;
        s.status_message = if recovery.outcome == TurnOutcome::Completed { None } else { Some(recovery.message.clone()) };
        s.recovery = Some(recovery);
    }
    if s.recovery.as_ref().is_none_or(|recovery| recovery.outcome == TurnOutcome::Completed) {
        s.queue_paused = false;
    }
    s.retry_pending = s.queue.iter().any(|prompt| prompt.retry_of.is_some());
    if matches!(s.status, Status::Starting | Status::Idle | Status::Running | Status::AwaitingPermission) {
        s.status = Status::Detached;
    }
    Ok(())
}

fn safe_diagnostic(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    uuid::Uuid::parse_str(object.get("id")?.as_str()?).ok()?;
    let mut out = serde_json::Map::new();
    for (key, value) in object {
        let valid = match key.as_str() {
            "id" => value.as_str().is_some(),
            "route" => matches!(value.as_str(), Some("anthropic_key" | "anthropic_oauth" | "anthropic_proxy" | "openai")),
            "phase" => matches!(value.as_str(), Some("inference" | "compaction")),
            "state" => matches!(value.as_str(), Some("pending" | "response" | "network_error")),
            "cache_owner" => matches!(value.as_str(), Some("client" | "proxy" | "none")),
            "model" | "effort" | "fast" => value.is_null() || value.as_str().is_some_and(|s| s.len() <= 128 && !s.chars().any(char::is_control)),
            "request_id" => value.is_null() || value.as_str().is_some_and(|s| s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))),
            "thinking" | "server_compaction" | "compaction" | "automatic_cache" => value.is_boolean(),
            "attempt" | "explicit_cache_points" | "at_ms" => value.as_u64().is_some(),
            "http_status" | "context_window" | "max_output" => value.is_null() || value.as_u64().is_some(),
            "rejected_fields" => value.as_array().is_some_and(|fields| fields.iter().all(|field| matches!(field.as_str(), Some("fallbacks" | "thinking.display" | "speed" | "eager_input_streaming" | "service_tier")))),
            "endpoint" => {
                if value.is_null() || matches!(value.as_str(), Some("[redacted]" | "[invalid endpoint]")) {
                    out.insert(key.clone(), Value::Null);
                    continue;
                }
                let mut url = reqwest::Url::parse(value.as_str()?).ok()?;
                if !matches!(url.scheme(), "http" | "https") { return None; }
                let _ = url.set_username("");
                let _ = url.set_password(None);
                url.set_query(None);
                url.set_fragment(None);
                out.insert(key.clone(), Value::String(url.as_str().chars().take(512).collect()));
                continue;
            }
            _ => false,
        };
        if valid { out.insert(key.clone(), value.clone()); }
    }
    Some(Value::Object(out))
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
    let mut actor = Actor {
        mgr: mgr.clone(), h: h.clone(), conn: None, acp_id: String::new(),
        pending: HashMap::new(), turn: None, tools: HashMap::new(), cancellation: None,
    };
    let result = actor.run(&mut rx).await;
    if let Err(e) = result {
        let msg = format!("{e:#}");
        tracing::warn!("session {} failed: {msg}", h.state.lock().id);
        actor.connection_failed();
    }
    if let Some(conn) = actor.conn.take() {
        conn.kill().await;
    }
    // Detach from the handle (our receiver is still open, so the slot can only hold our own
    // sender or nothing), then let a fresh actor take over anything that raced in.
    *h.actor.lock() = None;
    rx.close();
    let id = h.state.lock().id.clone();
    let mut kicked = false;
    while let Ok(cmd) = rx.try_recv() {
        if matches!(cmd, Cmd::Kick) && !kicked && !h.deleted.load(Ordering::SeqCst) && !h.state.lock().queue_paused {
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
    tools: HashMap<String, bool>,
    cancellation: Option<TurnOutcome>,
}

enum Flow {
    Continue,
    Stop,
}

impl Actor {
    fn session(&self) -> Session {
        self.h.state.lock().clone()
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
                Some(res) = turn_rx.recv() => {
                    // Notifications preceding the ACP response may still be buffered when
                    // its request task wakes. Fold them before recording the final counts.
                    while let Ok(msg) = incoming.try_recv() {
                        self.handle_incoming(msg);
                    }
                    self.turn_done(res, &turn_tx).await;
                },
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
                    "clientInfo": { "name": "sci-pi", "version": env!("CARGO_PKG_VERSION") },
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
            cmd @ (Cmd::Cancel | Cmd::Interrupt) => {
                if self.turn.is_some() {
                    self.cancellation = Some(if matches!(cmd, Cmd::Interrupt) {
                        TurnOutcome::Interrupted
                    } else {
                        TurnOutcome::Cancelled
                    });
                    self.conn().notify("session/cancel", json!({ "sessionId": self.acp_id }));
                }
                self.cancel_pending();
            }
            Cmd::Permission { request_id, option_id } => self.resolve(&request_id, option_id),
            Cmd::SetMode(mode) => self.set_mode(mode).await,
            Cmd::SetConfig { config_id, value } => self.set_config(config_id, value).await,
            Cmd::Stop => {
                let had_turn = self.turn.is_some();
                self.cancel_pending();
                self.finish_turn(TurnOutcome::Interrupted, "interrupted", None, None);
                if let Some(conn) = self.conn.take() {
                    conn.kill().await;
                }
                self.update(|s| {
                    s.status = Status::Stopped;
                    if had_turn { s.queue_paused = true; }
                });
                return Flow::Stop;
            }
        }
        Flow::Continue
    }

    /// Starts the next queued prompt if the agent is free.
    async fn kick(&mut self, turn_tx: &mpsc::UnboundedSender<Result<Value>>) {
        if self.turn.is_some() || self.session().queue_paused {
            return;
        }
        let mut next = None;
        let s = self.update(|s| next = take_next_prompt(s));
        let Some(item) = next else { return };
        let turn = s.turns;
        self.turn = Some(turn);
        self.tools.clear();
        self.cancellation = None;
        let cwd = PathBuf::from(&s.cwd);

        let checkpoint = git::snapshot(&cwd, Some(&format!("refs/sci-pi/{}/{turn}-start", s.id))).await.ok().flatten();
        if let Some(ev) = self.mgr.emit(&self.h, "user_prompt", json!({ "text": item.text, "attachments": item.attachments, "turn": turn, "checkpoint": checkpoint, "retry_of": item.retry_of })) {
            let _ = self.mgr.store.index_text(&s.id, ev.id, "user", &item.text);
        }

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

    fn finish_turn(&mut self, outcome: TurnOutcome, stop_reason: &str, checkpoint: Option<String>, usage: Option<&Value>) {
        let Some(turn) = self.turn.take() else { return };
        let recovery = turn_recovery(turn, outcome, &self.tools);
        self.emit("turn_end", json!({
            "turn": turn, "stop_reason": stop_reason, "checkpoint": checkpoint,
            "usage": usage, "recovery": recovery,
        }));
        self.update(|s| {
            s.status = if outcome == TurnOutcome::Failed { Status::Error } else { Status::Idle };
            s.status_message = if outcome == TurnOutcome::Completed { None } else { Some(recovery.message.clone()) };
            s.recovery = Some(recovery);
            if outcome == TurnOutcome::Failed {
                s.queue_paused = true;
            }
        });
    }

    fn connection_failed(&mut self) {
        let had_turn = self.turn.is_some();
        self.cancel_pending();
        let outcome = self.cancellation.unwrap_or(TurnOutcome::Failed);
        let reason = match outcome {
            TurnOutcome::Cancelled => "cancelled",
            TurnOutcome::Interrupted => "interrupted",
            _ => "error",
        };
        self.finish_turn(outcome, reason, None, None);
        self.emit("error", json!({ "message": "Agent connection failed. Existing tool effects remain; inspect diagnostics before retrying." }));
        self.update(|s| {
            s.status = Status::Error;
            s.pending_permissions = 0;
            if had_turn { s.queue_paused = true; }
            s.status_message = Some(if s.queue_paused {
                "Agent connection failed; queued prompts are paused until you choose how to continue."
            } else {
                "Agent connection failed. Send a new prompt to restart the connection."
            }.into());
        });
    }

    async fn turn_done(&mut self, res: Result<Value>, turn_tx: &mpsc::UnboundedSender<Result<Value>>) {
        let Some(turn) = self.turn else { return };
        self.cancel_pending();
        let s = self.session();
        let checkpoint = git::snapshot(Path::new(&s.cwd), Some(&format!("refs/sci-pi/{}/{turn}-end", s.id))).await.ok().flatten();
        if let Ok(Some(start)) = self.mgr.store.turn_event(&s.id, "user_prompt", turn) {
            let reply = agent_text(&self.mgr.store.events_after(start.id, Some(&s.id), i64::MAX).unwrap_or_default());
            let _ = self.mgr.store.index_text(&s.id, start.id, "agent", &reply);
        }
        let reason = res.as_ref().ok().and_then(|v| v["stopReason"].as_str()).unwrap_or("error");
        let outcome = self.cancellation.take().unwrap_or_else(|| {
            if res.is_err() || matches!(reason, "error" | "failed") { TurnOutcome::Failed }
            else if reason == "cancelled" { TurnOutcome::Cancelled }
            else { TurnOutcome::Completed }
        });
        let stop_reason = match outcome {
            TurnOutcome::Failed => "error",
            TurnOutcome::Cancelled => "cancelled",
            TurnOutcome::Interrupted => "interrupted",
            TurnOutcome::Completed => reason,
        };
        if outcome == TurnOutcome::Failed {
            self.emit("error", json!({ "message": "Turn failed. Partial output and tool effects are retained; inspect request diagnostics before retrying." }));
        }
        self.finish_turn(outcome, stop_reason, checkpoint, res.as_ref().ok().and_then(|v| v.get("usage")));
        if outcome != TurnOutcome::Failed && !self.session().queue.is_empty() {
            self.kick(turn_tx).await;
            return;
        }
        if outcome == TurnOutcome::Completed {
            let body = self.mgr.store.last_agent_text(&s.id).ok().flatten()
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
                if self.turn.is_some() {
                    observe_tool_call(&mut self.tools, &tool_call);
                }
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
                self.conn().respond_error(id, -32601, &format!("sci-pi does not implement {method}"));
            }
        }
    }

    fn handle_update(&mut self, update: Value) {
        let kind = update["sessionUpdate"].as_str().unwrap_or_default().to_string();
        if self.turn.is_some() {
            observe_tool(&mut self.tools, &update);
        }
        match kind.as_str() {
            "request_diagnostic" => {
                if self.session().agent != crate::config::NATIVE_AGENT { return; }
                let Some(diagnostic) = safe_diagnostic(&update["diagnostic"]) else { return };
                self.update(|s| {
                    if let Some(existing) = s.request_diagnostics.iter_mut().find(|d| d["id"] == diagnostic["id"]) {
                        *existing = diagnostic.clone();
                    } else {
                        s.request_diagnostics.push(diagnostic.clone());
                        if s.request_diagnostics.len() > 50 {
                            s.request_diagnostics.remove(0);
                        }
                    }
                });
                self.emit("update", json!({ "sessionUpdate": "request_diagnostic", "diagnostic": diagnostic }));
            }
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

/// Concatenated agent message text in a slice of the log.
fn agent_text(events: &[Event]) -> String {
    events
        .iter()
        .filter(|e| e.kind == "update" && e.data["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|e| e.data["content"]["text"].as_str())
        .collect()
}

/// A compact, readable transcript (prompts, replies, tool titles), trimmed from the front.
fn transcript(events: &[Event]) -> String {
    const LIMIT: usize = 40_000;
    let mut out = String::new();
    let mut last_role = "";
    for e in events {
        match (e.kind.as_str(), e.data["sessionUpdate"].as_str()) {
            ("user_prompt", _) => {
                out.push_str(&format!("\n\n## User\n{}", e.data["text"].as_str().unwrap_or_default()));
                last_role = "user";
            }
            ("update", Some("agent_message_chunk")) => {
                if last_role != "agent" {
                    out.push_str("\n\n## Assistant\n");
                    last_role = "agent";
                }
                out.push_str(e.data["content"]["text"].as_str().unwrap_or_default());
            }
            ("update", Some("tool_call")) => {
                out.push_str(&format!("\n- [tool] {}", e.data["title"].as_str().unwrap_or("tool call")));
                last_role = "tool";
            }
            _ => {}
        }
    }
    if out.len() > LIMIT {
        let mut cut = out.len() - LIMIT;
        while !out.is_char_boundary(cut) {
            cut += 1;
        }
        out = format!("[…earlier conversation trimmed…]{}", &out[cut..]);
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
        mgr: Arc<Manager>,
        h: Arc<Handle>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("sci-pi-recovery-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let store = Arc::new(Store::open(&dir.join("state.db")).unwrap());
            let mut session: Session = serde_json::from_value(json!({
                "id": "session", "title": "Recovery", "agent": crate::config::NATIVE_AGENT,
                "project": dir, "cwd": dir, "branch": null, "base_commit": null,
                "status": "idle", "status_message": null, "mode": null, "usage": null,
                "created_at": 1, "updated_at": 1, "turns": 1
            })).unwrap();
            session.last_prompt = Some(QueuedPrompt {
                id: "original".into(), text: "original prompt".into(),
                attachments: vec![Attachment::File { path: "input.txt".into() }], retry_of: None,
            });
            store.save_session(&session).unwrap();
            let mgr = Manager::new(Config::default(), store, &dir).unwrap();
            let h = mgr.require("session").unwrap();
            mgr.update(&h, |s| s.status = Status::Idle);
            Fixture { dir, mgr, h }
        }

        fn actor(&self) -> Actor {
            Actor {
                mgr: self.mgr.clone(), h: self.h.clone(), conn: None, acp_id: String::new(),
                pending: HashMap::new(), turn: Some(1), tools: HashMap::new(), cancellation: None,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    async fn idle_stop_and_connection_failure_allow_the_next_prompt_to_resume() {
        for stop in [false, true] {
            let f = Fixture::new();
            let mut actor = f.actor();
            actor.finish_turn(TurnOutcome::Completed, "end_turn", None, None);
            if stop {
                let (turn_tx, _turn_rx) = mpsc::unbounded_channel();
                actor.handle_cmd(Cmd::Stop, &turn_tx).await;
            } else {
                actor.connection_failed();
            }
            let (tx, _rx) = mpsc::unbounded_channel();
            *f.h.actor.lock() = Some(tx);
            f.mgr.prompt("session", PromptReq { text: "continue".into(), attachments: vec![] }).unwrap();
            let mut resumed = f.mgr.get("session").unwrap();
            assert!(!resumed.queue_paused);
            assert_eq!(take_next_prompt(&mut resumed).unwrap().text, "continue");
            assert_eq!(resumed.turns, 2);
            assert_eq!(resumed.status, Status::Running);
        }
    }

    #[tokio::test]
    async fn idle_stop_or_connection_failure_preserves_an_existing_failed_queue_pause() {
        for stop in [false, true] {
            let f = Fixture::new();
            let mut actor = f.actor();
            actor.finish_turn(TurnOutcome::Failed, "error", None, None);
            if stop {
                let (turn_tx, _turn_rx) = mpsc::unbounded_channel();
                actor.handle_cmd(Cmd::Stop, &turn_tx).await;
            } else {
                actor.connection_failed();
            }
            let (tx, _rx) = mpsc::unbounded_channel();
            *f.h.actor.lock() = Some(tx);
            f.mgr.prompt("session", PromptReq { text: "queued after failure".into(), attachments: vec![] }).unwrap();
            let mut paused = f.mgr.get("session").unwrap();
            assert!(take_next_prompt(&mut paused).is_none());
            assert_eq!(paused.queued, 1);
            assert_eq!(paused.recovery.unwrap().outcome, TurnOutcome::Failed);
        }
    }

    #[test]
    fn retry_identity_survives_queue_reordering_and_deleting_a_nonfront_retry() {
        let f = Fixture::new();
        f.actor().finish_turn(TurnOutcome::Cancelled, "cancelled", None, None);
        let (tx, _rx) = mpsc::unbounded_channel();
        *f.h.actor.lock() = Some(tx);
        f.mgr.prompt("session", PromptReq { text: "different instruction".into(), attachments: vec![] }).unwrap();
        let other_id = f.mgr.get("session").unwrap().queue[0].id.clone();
        let queued = f.mgr.retry("session").unwrap();
        let retry_id = queued.queue[0].id.clone();
        f.mgr.queue_send_now("session", &other_id).unwrap();
        let mut reordered = f.mgr.get("session").unwrap();
        let ordinary = take_next_prompt(&mut reordered).unwrap();
        assert_eq!(ordinary.text, "different instruction");
        assert_eq!(ordinary.retry_of, None);
        assert!(reordered.retry_pending);
        let retry = take_next_prompt(&mut reordered).unwrap();
        assert_eq!(retry.text, "original prompt");
        assert_eq!(retry.retry_of, Some(1));
        assert!(!reordered.retry_pending);
        f.mgr.queue_delete("session", &retry_id).unwrap();
        let deleted = f.mgr.get("session").unwrap();
        assert!(!deleted.retry_pending);
        assert_eq!(deleted.queue.len(), 1);
        assert_eq!(deleted.queue[0].id, other_id);
        assert!(f.mgr.retry("session").is_ok());
    }

    #[tokio::test]
    async fn explicit_error_stop_reason_is_failed_even_in_a_successful_rpc_response() {
        for reason in ["error", "failed"] {
            let f = Fixture::new();
            let mut actor = f.actor();
            let (turn_tx, _turn_rx) = mpsc::unbounded_channel();
            actor.turn_done(Ok(json!({ "stopReason": reason })), &turn_tx).await;
            let failed = f.mgr.get("session").unwrap();
            assert_eq!(failed.status, Status::Error);
            assert!(failed.queue_paused);
            assert_eq!(failed.recovery.unwrap().outcome, TurnOutcome::Failed);
        }
    }

    #[test]
    fn failure_is_durable_and_does_not_consume_queued_work() {
        let f = Fixture::new();
        f.mgr.update(&f.h, |s| {
            s.status = Status::Running;
            s.queue.push(QueuedPrompt { id: "next".into(), text: "next prompt".into(), attachments: vec![], retry_of: None });
            s.queued = 1;
        });
        let mut actor = f.actor();
        actor.handle_update(json!({ "sessionUpdate": "tool_call", "toolCallId": "a", "status": "in_progress" }));
        actor.handle_update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "completed" }));
        actor.handle_update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "completed" }));
        actor.handle_update(json!({ "sessionUpdate": "tool_call", "toolCallId": "b", "status": "in_progress" }));
        actor.finish_turn(TurnOutcome::Failed, "error", None, None);
        let restarted = Manager::new(Config::default(), f.mgr.store.clone(), &f.dir).unwrap();
        let mut s = restarted.get("session").unwrap();
        assert_eq!(s.status, Status::Error);
        let recovery = s.recovery.as_ref().unwrap();
        assert_eq!(recovery.outcome, TurnOutcome::Failed);
        assert_eq!((recovery.completed_tools, recovery.started_tools), (1, 2));
        assert!(take_next_prompt(&mut s).is_none());
        assert_eq!(s.queue[0].text, "next prompt");
        assert_eq!(s.turns, 1);
    }

    #[test]
    fn restart_closes_an_active_turn_once_and_preserves_unknown_effects() {
        let f = Fixture::new();
        f.mgr.update(&f.h, |s| s.status = Status::AwaitingPermission);
        f.mgr.store.append("session", "user_prompt", json!({ "turn": 1, "text": "original prompt" })).unwrap();
        f.mgr.store.append("session", "update", json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "partial reply" } })).unwrap();
        for update in [
            json!({ "sessionUpdate": "tool_call", "toolCallId": "a", "status": "in_progress" }),
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "completed" }),
            json!({ "sessionUpdate": "tool_call", "toolCallId": "b", "status": "in_progress" }),
        ] {
            f.mgr.store.append("session", "update", update).unwrap();
        }
        f.mgr.store.append("session", "permission_request", json!({ "request_id": "approval" })).unwrap();
        let restarted = Manager::new(Config::default(), f.mgr.store.clone(), &f.dir).unwrap();
        let s = restarted.get("session").unwrap();
        let recovery = s.recovery.unwrap();
        assert_eq!(recovery.outcome, TurnOutcome::Interrupted);
        assert_eq!((recovery.completed_tools, recovery.started_tools), (1, 2));
        assert!(recovery.message.contains("effects may remain"));
        assert!(s.queue_paused);
        assert_eq!(s.pending_permissions, 0);
        assert!(f.mgr.store.unresolved_permissions(Some("session")).unwrap().is_empty());
        let _again = Manager::new(Config::default(), f.mgr.store.clone(), &f.dir).unwrap();
        let events = f.mgr.store.events_after(0, Some("session"), i64::MAX).unwrap();
        assert_eq!(events.iter().filter(|e| e.kind == "turn_end").count(), 1);
        assert_eq!(agent_text(&events), "partial reply");
    }

    #[test]
    fn retry_is_a_new_numbered_attempt_with_original_attachments_and_history() {
        let f = Fixture::new();
        f.mgr.store.append("session", "update", json!({ "sessionUpdate": "agent_message_chunk", "content": { "text": "partial" } })).unwrap();
        f.actor().finish_turn(TurnOutcome::Cancelled, "cancelled", None, None);
        let (tx, mut rx) = mpsc::unbounded_channel();
        *f.h.actor.lock() = Some(tx);
        let queued = f.mgr.retry("session").unwrap();
        assert_eq!(queued.turns, 1);
        assert!(queued.retry_pending);
        assert_eq!(queued.queue[0].text, "original prompt");
        assert!(matches!(&queued.queue[0].attachments[0], Attachment::File { path } if path == "input.txt"));
        assert!(f.mgr.retry("session").is_err());
        assert!(matches!(rx.try_recv().unwrap(), Cmd::Kick));
        let mut next = queued;
        let attempt = take_next_prompt(&mut next).unwrap();
        assert_eq!(next.turns, 2);
        assert_eq!(next.status, Status::Running);
        assert_eq!(attempt.retry_of, Some(1));
        assert!(next.recovery.is_none());
        let events = f.mgr.store.events_after(0, Some("session"), i64::MAX).unwrap();
        assert_eq!(agent_text(&events), "partial");
        assert_eq!(events.iter().find(|e| e.kind == "turn_end").unwrap().data["recovery"]["outcome"], "cancelled");
        for status in [Status::Running, Status::AwaitingPermission, Status::Starting] {
            f.mgr.update(&f.h, |s| { s.status = status; s.retry_pending = false; });
            assert!(f.mgr.retry("session").is_err());
        }
    }

    #[test]
    fn resume_releases_retained_queue_without_repeating_failed_prompt() {
        let f = Fixture::new();
        f.mgr.update(&f.h, |s| {
            s.queue.push(QueuedPrompt { id: "next".into(), text: "continue carefully".into(), attachments: vec![], retry_of: None });
            s.queued = 1;
        });
        f.actor().finish_turn(TurnOutcome::Failed, "error", None, None);
        let (tx, mut rx) = mpsc::unbounded_channel();
        *f.h.actor.lock() = Some(tx);
        f.mgr.resume_queues();
        assert!(rx.try_recv().is_err());
        let mut resumed = f.mgr.resume("session").unwrap();
        assert!(!resumed.queue_paused);
        assert!(matches!(rx.try_recv().unwrap(), Cmd::Kick));
        assert_eq!(take_next_prompt(&mut resumed).unwrap().text, "continue carefully");
        assert_eq!(resumed.turns, 2);
        assert!(resumed.queue.is_empty());
    }

    #[test]
    fn restart_reconciles_durable_completion_before_session_row_update() {
        let f = Fixture::new();
        f.mgr.update(&f.h, |s| s.status = Status::Running);
        let recovery = turn_recovery(1, TurnOutcome::Completed, &HashMap::new());
        f.mgr.store.append("session", "turn_end", json!({ "turn": 1, "stop_reason": "end_turn", "recovery": recovery })).unwrap();
        let restarted = Manager::new(Config::default(), f.mgr.store.clone(), &f.dir).unwrap();
        let s = restarted.get("session").unwrap();
        assert_eq!(s.recovery.unwrap().outcome, TurnOutcome::Completed);
        assert!(!s.queue_paused);
        assert_eq!(f.mgr.store.events_after(0, Some("session"), i64::MAX).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancelled_http_result_stays_cancelled_and_stop_stays_interrupted() {
        let f = Fixture::new();
        let mut actor = f.actor();
        actor.cancellation = Some(TurnOutcome::Cancelled);
        let (tx, _) = mpsc::unbounded_channel();
        actor.turn_done(Err(anyhow!("cancel transport")), &tx).await;
        assert_eq!(f.mgr.get("session").unwrap().recovery.unwrap().outcome, TurnOutcome::Cancelled);
        actor.turn = Some(2);
        actor.handle_cmd(Cmd::Stop, &tx).await;
        let s = f.mgr.get("session").unwrap();
        assert_eq!(s.status, Status::Stopped);
        assert_eq!(s.recovery.unwrap().outcome, TurnOutcome::Interrupted);
        assert!(s.queue_paused);
    }

    #[test]
    fn diagnostic_updates_are_upserted_bounded_persisted_and_native_only() {
        let f = Fixture::new();
        let mut actor = f.actor();
        let id = uuid::Uuid::new_v4().to_string();
        for state in ["pending", "response"] {
            actor.handle_update(json!({
                "sessionUpdate": "request_diagnostic", "diagnostic": {
                    "id": id, "state": state, "route": "openai",
                    "endpoint": "https://user:secret@example.test/v1?token=secret#secret",
                    "body": "never retain", "headers": { "authorization": "secret" }
                }
            }));
        }
        let s = f.mgr.get("session").unwrap();
        assert_eq!(s.request_diagnostics.len(), 1);
        assert_eq!(s.request_diagnostics[0]["state"], "response");
        assert_eq!(s.request_diagnostics[0]["endpoint"], "https://example.test/v1");
        assert!(s.request_diagnostics[0].get("body").is_none());
        for _ in 0..55 {
            actor.handle_update(json!({ "sessionUpdate": "request_diagnostic", "diagnostic": { "id": uuid::Uuid::new_v4().to_string(), "state": "pending" } }));
        }
        assert_eq!(f.mgr.get("session").unwrap().request_diagnostics.len(), 50);
        let events = f.mgr.store.events_after(0, Some("session"), i64::MAX).unwrap();
        assert_eq!(events.len(), 57);
        assert!(!serde_json::to_string(&events).unwrap().contains("secret"));
        let restarted = Manager::new(Config::default(), f.mgr.store.clone(), &f.dir).unwrap();
        assert_eq!(restarted.get("session").unwrap().request_diagnostics.len(), 50);
        f.mgr.update(&f.h, |s| {
            s.agent = "third-party".into();
            s.queue.push(QueuedPrompt { id: "new".into(), text: "new".into(), attachments: vec![], retry_of: None });
        });
        actor.handle_update(json!({ "sessionUpdate": "request_diagnostic", "diagnostic": { "id": uuid::Uuid::new_v4().to_string(), "state": "pending" } }));
        assert_eq!(f.mgr.store.events_after(0, Some("session"), i64::MAX).unwrap().len(), 57);
        let mut next = f.mgr.get("session").unwrap();
        take_next_prompt(&mut next).unwrap();
        assert!(next.request_diagnostics.is_empty());
    }
}
