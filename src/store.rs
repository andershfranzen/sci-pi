//! SQLite persistence: session rows plus the append-only event log every client replays from.

use crate::config::now_ms;
use crate::model::{Event, Session};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 json TEXT NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL,
                 ts INTEGER NOT NULL,
                 kind TEXT NOT NULL,
                 data TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS events_by_session ON events(session_id, id);",
        )?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    pub fn save_session(&self, s: &Session) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO sessions (id, json, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
            params![s.id, serde_json::to_string(s)?, s.updated_at],
        )?;
        Ok(())
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT json FROM sessions ORDER BY updated_at DESC")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for json in rows {
            out.push(serde_json::from_str(&json?)?);
        }
        Ok(out)
    }

    pub fn delete_session(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM events WHERE session_id = ?1", [id])?;
        conn.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn append(&self, session_id: &str, kind: &str, data: Value) -> Result<Event> {
        let ts = now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO events (session_id, ts, kind, data) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, ts, kind, data.to_string()],
        )?;
        Ok(Event { id: conn.last_insert_rowid(), session_id: session_id.into(), ts, kind: kind.into(), data })
    }

    /// Events with `id > after`, optionally for a single session, ascending.
    pub fn events_after(&self, after: i64, session_id: Option<&str>, limit: i64) -> Result<Vec<Event>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, ts, kind, data FROM events
             WHERE id > ?1 AND (?2 IS NULL OR session_id = ?2)
             ORDER BY id LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![after, session_id, limit], row_to_event)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn last_event_id(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT MAX(id) FROM events", [], |r| r.get::<_, Option<i64>>(0))?
            .unwrap_or(0))
    }

    /// Permission requests that never got a matching `permission_resolved`.
    pub fn unresolved_permissions(&self, session_id: Option<&str>) -> Result<Vec<Event>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, ts, kind, data FROM events e
             WHERE kind = 'permission_request' AND (?1 IS NULL OR session_id = ?1)
               AND NOT EXISTS (
                 SELECT 1 FROM events r
                 WHERE r.kind = 'permission_resolved' AND r.session_id = e.session_id
                   AND json_extract(r.data, '$.request_id') = json_extract(e.data, '$.request_id'))
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![session_id], row_to_event)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The most recent agent message text in a session, for notifications.
    pub fn last_agent_text(&self, session_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let message_id: Option<Option<String>> = conn
            .query_row(
                "SELECT json_extract(data, '$.messageId') FROM events
                 WHERE session_id = ?1 AND kind = 'update'
                   AND json_extract(data, '$.sessionUpdate') = 'agent_message_chunk'
                 ORDER BY id DESC LIMIT 1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(Some(message_id)) = message_id else { return Ok(None) };
        let mut stmt = conn.prepare(
            "SELECT json_extract(data, '$.content.text') FROM events
             WHERE session_id = ?1 AND kind = 'update' AND json_extract(data, '$.messageId') = ?2
             ORDER BY id",
        )?;
        let parts = stmt.query_map(params![session_id, message_id], |r| r.get::<_, Option<String>>(0))?;
        let mut text = String::new();
        for p in parts {
            text.push_str(&p?.unwrap_or_default());
        }
        Ok(Some(text))
    }
}

fn row_to_event(r: &rusqlite::Row) -> rusqlite::Result<Event> {
    let data: String = r.get(4)?;
    Ok(Event {
        id: r.get(0)?,
        session_id: r.get(1)?,
        ts: r.get(2)?,
        kind: r.get(3)?,
        data: serde_json::from_str(&data).unwrap_or(Value::Null),
    })
}
