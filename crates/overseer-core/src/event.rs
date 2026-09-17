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
        /// Session this one forked from (its session_id) — the JSONL
        /// tree's cross-session edge. `#[serde(default)]` keeps
        /// pre-fork logs loadable.
        #[serde(default)]
        parent: Option<String>,
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
        /// True when the permission gate denied the call (never executed).
        #[serde(default)]
        denied: bool,
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
    /// A background subagent finished (P3.4 fire-and-notify): its bounded
    /// digest is injected as a user message at the next step boundary;
    /// `trace` is the subagent's session dir for audit.
    SubagentDone {
        /// Named `task_id` — `id` collides with the envelope field under
        /// serde flatten.
        task_id: String,
        trace: String,
    },
    /// A Rule-of-Two taint latch flipped (P3.10): untrusted content or
    /// sensitive data entered context. Audit-only — does not rehydrate
    /// into messages.
    Tainted {
        detail: String,
    },
    /// Stuck detector tripped — records which of the five patterns fired.
    StuckDetected {
        pattern: String,
    },
    /// Harness-injected course-correction, delivered as a user-role message.
    /// Rehydrates into context like UserInput but is provably not user-typed.
    Nudge {
        text: String,
    },
    /// Compaction boundary (playbook Ch.3 §9.5): a view marker, not a
    /// mutation. Events with `id < tail_from` are represented by `summary`;
    /// `id >= tail_from` replay verbatim (the recency tail). `tail_from`
    /// always points at a ModelResponse so tool pairing survives the cut.
    Compaction {
        summary: String,
        tail_from: u64,
    },
    /// Memory changed on disk at a turn boundary (P6-2 audit signal):
    /// dirty git status in the memory dir after the engine commit.
    /// Audit-only — never rehydrates into messages, never injected.
    MemoryUpdated { files: Vec<String> },
    /// A user consent grant was loaded for this session (P6-4): the OAuth
    /// shape's paper trail — who may exercise which scopes until when, and
    /// which human approved it. Audit-only, like `MemoryUpdated`; carries
    /// no secret material (client/scope metadata only).
    ConsentGranted {
        client: String,
        scopes: Vec<String>,
        /// Absolute expiry, ms since the unix epoch; 0 = no expiry.
        expires_ms: u64,
        actor: String,
        approved_by: String,
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
/// OTel GenAI projection (B1-8, openllmetry attribute schema).
///
/// Pure naming map over `&[Event]` — no package, no exporter, zero tokens.
/// events.jsonl stays the source of truth; this projection mutates nothing
/// and is safe to point at any OTLP backend (Jaeger/Grafana/Phoenix/Langfuse)
/// with zero parsers. Attribute table documented in `eval/LEDGER.md`.
///
/// Mapping:
/// - session_id → `gen_ai.trace.id` (from SessionStart.session_id)
/// - ModelResponse → span `gen_ai.span.kind="llm"`, model → `gen_ai.request.model`
/// - Usage fresh/cache/output/reasoning → `gen_ai.usage.*`
/// - latency_ms → `gen_ai.latency_ms`, cost_usd → `gen_ai.cost_usd`
/// - ToolCallStart/ToolResult → span `gen_ai.span.kind="tool"`
pub fn otel_spans(events: &[Event]) -> Vec<serde_json::Value> {
    let trace_id = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::SessionStart { session_id, .. } => Some(session_id.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let mut spans = Vec::new();
    for e in events {
        match &e.kind {
            EventKind::ModelResponse {
                blocks,
                usage,
                stop_reason,
                latency_ms,
                cost_usd,
            } => {
                let model = events
                    .iter()
                    .find_map(|s| match &s.kind {
                        EventKind::SessionStart { model, .. } => Some(model.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                spans.push(serde_json::json!({
                    "gen_ai.trace.id": trace_id,
                    "gen_ai.span.id": e.id,
                    "gen_ai.span.kind": "llm",
                    "gen_ai.request.model": model,
                    "gen_ai.response.stop_reason": stop_reason,
                    "gen_ai.usage.input_tokens": usage.fresh_input,
                    "gen_ai.usage.cache_write_tokens": usage.cache_write,
                    "gen_ai.usage.cache_read_tokens": usage.cache_read,
                    "gen_ai.usage.output_tokens": usage.output,
                    "gen_ai.usage.reasoning_tokens": usage.reasoning,
                    "gen_ai.latency_ms": latency_ms,
                    "gen_ai.cost_usd": cost_usd,
                    "gen_ai.tool_calls": blocks.iter().filter(|b| matches!(b, Block::ToolCall { .. })).count(),
                }));
            }
            EventKind::ToolCallStart { call_id, name, .. } => {
                spans.push(serde_json::json!({
                    "gen_ai.trace.id": trace_id,
                    "gen_ai.span.id": e.id,
                    "gen_ai.span.kind": "tool",
                    "gen_ai.tool.call_id": call_id,
                    "gen_ai.tool.name": name,
                }));
            }
            EventKind::ToolResult {
                call_id,
                name,
                is_error,
                denied,
                raw_bytes,
                ..
            } => {
                spans.push(serde_json::json!({
                    "gen_ai.trace.id": trace_id,
                    "gen_ai.span.id": e.id,
                    "gen_ai.span.kind": "tool",
                    "gen_ai.tool.call_id": call_id,
                    "gen_ai.tool.name": name,
                    "gen_ai.tool.is_error": is_error,
                    "gen_ai.tool.denied": denied,
                    "gen_ai.tool.raw_bytes": raw_bytes,
                }));
            }
            _ => {}
        }
    }
    spans
}

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
///
/// Compaction is honored as a view marker: only the latest `Compaction`
/// matters — its summary becomes a user message and verbatim replay resumes
/// at `tail_from`. Everything older folds into the summary and is skipped.
pub fn rehydrate_messages(events: &[Event]) -> Vec<crate::ir::Message> {
    use crate::ir::{Message, Role};

    let (summary, tail_from) = crate::compact::latest(events)
        .map(|(s, t)| (Some(s), t))
        .unwrap_or((None, 0));

    let mut messages: Vec<Message> = Vec::new();
    if let Some(s) = summary {
        messages.push(Message::user_text(s));
    }
    let mut pending_results: Vec<Block> = Vec::new();

    let flush_results = |pending: &mut Vec<Block>, msgs: &mut Vec<Message>| {
        if !pending.is_empty() {
            msgs.push(Message::tool_results(std::mem::take(pending)));
        }
    };

    // B1-7: collect tagged reflection Nudges first so only the last 1
    // replays verbatim (view rule; the log is untouched). Untagged Nudges
    // (verify-tail, empty, stuck) always replay.
    let last_reflection = events
        .iter()
        .filter(|e| e.id >= tail_from)
        .filter_map(|e| match &e.kind {
            EventKind::Nudge { text } if text.starts_with(crate::agent::Agent::REFLECTION_TAG) => {
                Some(e.id)
            }
            _ => None,
        })
        .max();
    for ev in events {
        if ev.id < tail_from {
            continue;
        }
        match &ev.kind {
            EventKind::UserInput { text } => {
                flush_results(&mut pending_results, &mut messages);
                messages.push(Message::user_text(text.clone()));
            }
            EventKind::Nudge { text } => {
                if text.starts_with(crate::agent::Agent::REFLECTION_TAG)
                    && Some(ev.id) != last_reflection
                {
                    continue; // superseded critique: view-only eviction
                }
                flush_results(&mut pending_results, &mut messages);
                messages.push(Message::user_text(text.clone()));
            }
            // Background-subagent notices rehydrate as user text — same
            // pairing position as the live drain (after tool results).
            EventKind::SubagentDone { task_id: id, trace } => {
                flush_results(&mut pending_results, &mut messages);
                let digest = std::fs::read_to_string(std::path::Path::new(trace).join("done.txt"))
                    .unwrap_or_else(|_| "(digest missing)".into());
                messages.push(Message::user_text(format!(
                    "[subagent {id} finished]\n{digest}"
                )));
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
                name,
                content,
                is_error,
                ..
            } => {
                pending_results.push(Block::ToolResult {
                    tool_use_id: call_id.clone(),
                    // Re-wrap on replay — the live view wraps at push
                    // time, so both paths produce identical bytes.
                    content: crate::tools::provenance_wrap(name, content),
                    is_error: *is_error,
                });
            }
            _ => {}
        }
    }
    flush_results(&mut pending_results, &mut messages);
    messages
}

/// Placeholder that replaces a cleared tool result's content. Fixed text so
/// the transform is idempotent — re-clearing an already-cleared result is a
/// no-op byte-wise.
pub const CLEARED_RESULT: &str = "[tool result cleared — re-read the file if needed]";

/// P1.2 stale tool-result clearing: keep the last `keep` `ToolResult` blocks
/// verbatim; replace older ones' content with `CLEARED_RESULT`. The block
/// itself (and its `tool_use_id`) survives so tool_use/tool_result pairing
/// stays valid for the provider.
///
/// Pure view transform over the message view — events on disk are never
/// touched, so a cleared session rehydrates the full history and re-clears
/// deterministically. Returns how many results were (re)written.
pub fn clear_stale_tool_results(messages: &mut [crate::ir::Message], keep: usize) -> usize {
    let total = messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| matches!(b, crate::ir::Block::ToolResult { .. }))
        .count();
    let stale = total.saturating_sub(keep);
    if stale == 0 {
        return 0;
    }
    let mut seen = 0usize;
    for m in messages.iter_mut() {
        for b in m.content.iter_mut() {
            if let crate::ir::Block::ToolResult { content, .. } = b {
                if seen < stale {
                    *content = CLEARED_RESULT.to_string();
                }
                seen += 1;
            }
        }
    }
    stale
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
                parent: None,
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
                denied: false,
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
    fn clear_stale_tool_results_keeps_recent_tail() {
        let result = |id: &str, text: &str| Block::ToolResult {
            tool_use_id: id.into(),
            content: text.into(),
            is_error: false,
        };
        let mut msgs = vec![
            crate::ir::Message::tool_results(vec![result("a", "old-1"), result("b", "old-2")]),
            crate::ir::Message::user_text("middle"),
            crate::ir::Message::tool_results(vec![
                result("c", "recent-1"),
                result("d", "recent-2"),
            ]),
        ];
        let cleared = clear_stale_tool_results(&mut msgs, 2);
        assert_eq!(cleared, 2);
        // Older two cleared but blocks/tool_use_ids survive (pairing intact).
        match &msgs[0].content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "a");
                assert_eq!(content, CLEARED_RESULT);
            }
            _ => panic!("expected ToolResult"),
        }
        match &msgs[0].content[1] {
            Block::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "b");
                assert_eq!(content, CLEARED_RESULT);
            }
            _ => panic!("expected ToolResult"),
        }
        // Recent tail untouched.
        match &msgs[2].content[0] {
            Block::ToolResult { content, .. } => assert_eq!(content, "recent-1"),
            _ => panic!("expected ToolResult"),
        }
        // Idempotent: a second pass changes nothing.
        let snapshot = msgs.clone();
        clear_stale_tool_results(&mut msgs, 2);
        assert_eq!(msgs, snapshot);
        // Under the cap → no-op.
        let mut small = vec![crate::ir::Message::tool_results(vec![result("x", "keep")])];
        assert_eq!(clear_stale_tool_results(&mut small, 2), 0);
        match &small[0].content[0] {
            Block::ToolResult { content, .. } => assert_eq!(content, "keep"),
            _ => panic!("expected ToolResult"),
        }
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

    #[test]
    fn otel_projection_maps_gen_ai_aliases() {
        // B1-8: every alias present; trace id flows from SessionStart.
        // Projection mutates nothing — events.jsonl stays source of truth
        // (asserted by the no-mutation test below).
        let events = vec![
            Event {
                id: 0,
                parent_id: None,
                ts_ms: 1,
                kind: EventKind::SessionStart {
                    session_id: "s1".into(),
                    cwd: "/tmp".into(),
                    model: "m".into(),
                    harness_version: "0.1.0".into(),
                    parent: None,
                },
            },
            Event {
                id: 1,
                parent_id: Some(0),
                ts_ms: 2,
                kind: EventKind::ModelResponse {
                    blocks: vec![],
                    usage: Usage {
                        fresh_input: 10,
                        cache_read: 90,
                        ..Usage::default()
                    },
                    stop_reason: "end_turn".into(),
                    latency_ms: 5,
                    cost_usd: 0.1,
                },
            },
            Event {
                id: 2,
                parent_id: Some(1),
                ts_ms: 3,
                kind: EventKind::ToolCallStart {
                    call_id: "c1".into(),
                    name: "read".into(),
                    input: serde_json::json!({}),
                },
            },
            Event {
                id: 3,
                parent_id: Some(2),
                ts_ms: 4,
                kind: EventKind::ToolResult {
                    call_id: "c1".into(),
                    name: "read".into(),
                    content: "x".into(),
                    is_error: false,
                    raw_bytes: 1,
                    spilled_to: None,
                    denied: false,
                },
            },
        ];
        let before = serde_json::to_string(&events).unwrap();
        let spans = otel_spans(&events);
        let after = serde_json::to_string(&events).unwrap();
        assert_eq!(before, after, "projection must not mutate events");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0]["gen_ai.trace.id"], "s1");
        assert_eq!(spans[0]["gen_ai.span.kind"], "llm");
        assert_eq!(spans[0]["gen_ai.request.model"], "m");
        assert_eq!(spans[0]["gen_ai.usage.cache_read_tokens"], 90);
        assert_eq!(spans[0]["gen_ai.latency_ms"], 5);
        assert_eq!(spans[0]["gen_ai.cost_usd"], 0.1);
        assert_eq!(spans[1]["gen_ai.tool.name"], "read");
        assert_eq!(spans[2]["gen_ai.tool.is_error"], false);
        // Snapshot: the exported key set is the contract OTLP backends read.
        let mut keys: Vec<&str> = spans[0]
            .as_object()
            .unwrap()
            .keys()
            .map(|s| s.as_str())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "gen_ai.cost_usd",
                "gen_ai.latency_ms",
                "gen_ai.request.model",
                "gen_ai.response.stop_reason",
                "gen_ai.span.id",
                "gen_ai.span.kind",
                "gen_ai.tool_calls",
                "gen_ai.trace.id",
                "gen_ai.usage.cache_read_tokens",
                "gen_ai.usage.cache_write_tokens",
                "gen_ai.usage.input_tokens",
                "gen_ai.usage.output_tokens",
                "gen_ai.usage.reasoning_tokens",
            ]
        );
    }
}
