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
    #[serde(default)]
    pub queued: usize,
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
