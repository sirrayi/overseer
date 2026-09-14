//! Append-only event log (Invariant 1, playbook Ch.2 §2.2).
//!
//! JSONL records with `id`/`parent_id` forming a tree — the single source of
//! truth for conversation, audit, replay, checkpoint/rewind, and resume.
//! The model-facing context is a *view* assembled from this log; nothing is
//! ever mutated in place.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ir::{Block, Usage};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    SessionStart {
        session_id: String,
        cwd: String,
        model: String,
        harness_version: String,
    },
    UserInput {
        text: String,
    },
    /// One provider response: assistant blocks + normalized usage + stop reason.
    ModelResponse {
        blocks: Vec<Block>,
        usage: Usage,
        stop_reason: String,
        latency_ms: u64,
        cost_usd: f64,
    },
    ToolCallStart {
        call_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        name: String,
        /// Result text (may itself be a truncation placeholder / spill ref).
        content: String,
        is_error: bool,
        /// Bytes before truncation; if spilled, the file it went to.
        raw_bytes: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        spilled_to: Option<String>,
    },
    /// Turn boundary — fsync point, durable-tail marker.
    TurnEnd {
        step: u32,
    },
    /// Agent finished a run: stop reason + budget tallies.
    RunEnd {
        stop_reason: String,
        steps: u32,
        total_cost_usd: f64,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: u64,
    pub parent_id: Option<u64>,
    pub ts_ms: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

/// Writer for a session's `events.jsonl`. IDs are monotonic per session;
/// `parent_id` chains to the previous event head (forks come later: a child
/// event can name any earlier id as parent).
pub struct EventLog {
    file: File,
    path: PathBuf,
    next_id: u64,
    head: Option<u64>,
    last: Option<Event>,
}

impl EventLog {
    /// Create a new log file (fails if it exists — sessions are unique dirs).
    pub fn create(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        Ok(EventLog {
            file,
            path,
            next_id: 1,
            head: None,
            last: None,
        })
    }

    /// Open an existing log for append (resume): replays to find next_id/head.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let events = Self::replay(&path)?;
        let (next_id, head, last) = match events.last() {
            Some(e) => (e.id + 1, Some(e.id), Some(e.clone())),
            None => (1, None, None),
        };
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(EventLog {
            file,
            path,
            next_id,
            head,
            last,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append an event; returns the assigned id. Buffer-flushed every call,
    /// fsync only on `flush()` (turn boundaries — durable-tail semantics).
    pub fn append(&mut self, kind: EventKind) -> std::io::Result<u64> {
        let id = self.next_id;
        let ev = Event {
            id,
            parent_id: self.head,
            ts_ms: now_ms(),
            kind,
        };
        let mut line = serde_json::to_string(&ev).map_err(std::io::Error::other)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        self.head = Some(id);
        self.next_id += 1;
        self.last = Some(ev);
        Ok(id)
    }

    /// The most recently appended event (for the live event sink).
    pub fn last(&self) -> Option<&Event> {
        self.last.as_ref()
    }

    /// Durable-tail point: fsync at turn boundaries.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.file.sync_data()
    }

    /// Replay the whole log.
    pub fn replay(path: impl AsRef<Path>) -> std::io::Result<Vec<Event>> {
        let f = File::open(path)?;
        let mut out = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(&line) {
                Ok(ev) => out.push(ev),
                // Tolerate a torn final line (crash mid-write): durable-tail
                // semantics mean everything before the last fsync is valid.
                Err(_) => break,
            }
        }
        Ok(out)
    }
}

/// Rebuild the provider-facing message list from the log.
/// UserInput → user message; ModelResponse → assistant message; consecutive
/// ToolResult events merge into one user message (Anthropic's rule: all results
/// for a tool_use batch live in a single user turn).
pub fn rehydrate_messages(events: &[Event]) -> Vec<crate::ir::Message> {
    use crate::ir::{Message, Role};
    let mut messages: Vec<Message> = Vec::new();
    let mut pending_results: Vec<Block> = Vec::new();

    let flush_results = |pending: &mut Vec<Block>, msgs: &mut Vec<Message>| {
        if !pending.is_empty() {
            msgs.push(Message::tool_results(std::mem::take(pending)));
        }
    };

    for ev in events {
        match &ev.kind {
            EventKind::UserInput { text } => {
                flush_results(&mut pending_results, &mut messages);
                messages.push(Message::user_text(text.clone()));
            }
            EventKind::ModelResponse { blocks, .. } => {
                flush_results(&mut pending_results, &mut messages);
                messages.push(Message {
                    role: Role::Assistant,
                    content: blocks.clone(),
                });
            }
            EventKind::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } => {
                pending_results.push(Block::ToolResult {
                    tool_use_id: call_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                });
            }
            _ => {}
        }
    }
    flush_results(&mut pending_results, &mut messages);
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Role;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn append_and_replay() {
        let dir = tmpdir();
        let path = dir.join("events.jsonl");
        {
            let mut log = EventLog::create(&path).unwrap();
            log.append(EventKind::SessionStart {
                session_id: "s1".into(),
                cwd: "/tmp".into(),
                model: "m".into(),
                harness_version: "0.1.0".into(),
            })
            .unwrap();
            log.append(EventKind::UserInput {
                text: "hello".into(),
            })
            .unwrap();
            log.flush().unwrap();
        }
        let events = EventLog::replay(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].parent_id, Some(1));
    }

    #[test]
    fn rehydrate_groups_tool_results() {
        let dir = tmpdir();
        let path = dir.join("events.jsonl");
        {
            let mut log = EventLog::create(&path).unwrap();
            log.append(EventKind::UserInput {
                text: "do it".into(),
            })
            .unwrap();
            log.append(EventKind::ModelResponse {
                blocks: vec![Block::ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    input: serde_json::json!({}),
                }],
                usage: Usage::default(),
                stop_reason: "tool_use".into(),
                latency_ms: 1,
                cost_usd: 0.0,
            })
            .unwrap();
            log.append(EventKind::ToolResult {
                call_id: "a".into(),
                name: "read".into(),
                content: "data".into(),
                is_error: false,
                raw_bytes: 4,
                spilled_to: None,
            })
            .unwrap();
        }
        let events = EventLog::replay(&path).unwrap();
        let msgs = rehydrate_messages(&events);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[2].role, Role::User);
        assert!(matches!(msgs[2].content[0], Block::ToolResult { .. }));
    }

    #[test]
    fn tolerates_torn_tail() {
        let dir = tmpdir();
        let path = dir.join("events.jsonl");
        {
            let mut log = EventLog::create(&path).unwrap();
            log.append(EventKind::UserInput { text: "ok".into() })
                .unwrap();
            log.flush().unwrap();
        }
        // Simulate crash mid-write.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"id\":2,\"partial").unwrap();
        drop(f);
        let events = EventLog::replay(&path).unwrap();
        assert_eq!(events.len(), 1);
    }
}
