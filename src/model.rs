//! Wire types shared by the store, the session actors and the HTTP API. See docs/PROTOCOL.md.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Starting,
    Idle,
    Running,
    AwaitingPermission,
    Detached,
    Stopped,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mode {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRecovery {
    pub turn: u32,
    pub outcome: TurnOutcome,
    pub completed_tools: u32,
    pub started_tools: u32,
    pub resumable: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub agent: String,
    pub project: String,
    pub cwd: String,
    pub branch: Option<String>,
    pub base_commit: Option<String>,
    pub status: Status,
    pub status_message: Option<String>,
    pub mode: Option<String>,
    #[serde(default)]
    pub modes: Vec<Mode>,
    pub usage: Option<Value>,
    /// Prompts waiting for the current turn to end. Persisted, so a queue survives restarts.
    #[serde(default)]
    pub queue: Vec<QueuedPrompt>,
    #[serde(default)]
    pub queued: usize,
    /// ACP `SessionConfigOption`s verbatim (model, effort, mode, …).
    #[serde(default)]
    pub config_options: Vec<Value>,
    /// The agent's slash commands: `{ name, description, input? }`.
    #[serde(default)]
    pub commands: Vec<Value>,
    /// ACP `promptCapabilities` (`image`, `embeddedContext`, …).
    #[serde(default)]
    pub prompt_caps: Value,
    #[serde(default)]
    pub turns: u32,
    #[serde(default)]
    pub recovery: Option<TurnRecovery>,
    #[serde(default)]
    pub request_diagnostics: Vec<Value>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub pr_url: Option<String>,
    #[serde(default)]
    pub pending_permissions: usize,
    pub created_at: i64,
    pub updated_at: i64,

    // Internal bookkeeping, persisted with the row.
    /// The agent's own session id, used to resume after the adapter restarts.
    #[serde(default)]
    pub acp_session_id: Option<String>,
    /// Set once the user (or the first prompt) picked a title, so agent title updates don't clobber it.
    #[serde(default)]
    pub title_locked: bool,
    /// Prepended (invisibly) to the next prompt, e.g. after the user reverted files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_note: Option<String>,
    /// An unsuccessful active turn pauses, rather than silently consuming, the durable queue.
    #[serde(default)]
    pub queue_paused: bool,
    #[serde(default)]
    pub retry_pending: bool,
    /// Persisted before checkpointing so even an early interruption can be retried.
    #[serde(default)]
    pub last_prompt: Option<QueuedPrompt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedPrompt {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_of: Option<u32>,
}

/// Something attached to a prompt. Images are stored on the daemon at upload time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Attachment {
    /// `name` is the file under `/api/attachments/`.
    Image { name: String, mime_type: String },
    /// A path in the project (from an @-mention).
    File { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub session_id: String,
    pub ts: i64,
    pub kind: String,
    pub data: Value,
}

/// Messages pushed to WebSocket clients.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsMsg {
    Event { event: Event },
    Session { session: Session },
    SessionDeleted { id: String },
}
