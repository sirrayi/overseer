//! Append-only event log (Invariant 1, playbook Ch.2 §2.2).
//!
//! JSONL records with `id`/`parent_id` forming a tree — the single source of
//! truth for conversation, audit, replay, checkpoint/rewind, and resume.
//! The model-facing context is a *view* assembled from this log; nothing is
//! ever mutated in place.

use std::collections::HashSet;
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
        /// Cache tokens billed inside this run only (end-of-run
        /// `Agent::cache_stats()` minus the run-start snapshot) — the
        /// session ledger keeps the cumulative totals. Absent on
        /// pre-extension logs, hence `default`.
        #[serde(default)]
        cache: crate::ledger::CacheStats,
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
    /// A computer-use act (P7-3): which tier served it and the pre/post
    /// observation digests — the act's diff record. Audit-only, like
    /// `Tainted`; `suppressed` marks a metadata-only capture (credential
    /// field / watch mode), where no pixels were taken at all.
    ComputerAct {
        action: String,
        tier: String,
        #[serde(default)]
        pre: Option<String>,
        #[serde(default)]
        post: Option<String>,
        #[serde(default)]
        suppressed: bool,
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
    MemoryUpdated {
        files: Vec<String>,
    },
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
    /// The engine switched the run's model mid-session (P8-B crush
    /// `set_model` port): `from`/`to` model ids plus the to-model's list
    /// prices, so the audit trail shows what a switch costs without a
    /// profile lookup at read time. Audit-only — never rehydrates into
    /// messages (the request itself carries the new model).
    ModelSwitch {
        from: String,
        to: String,
        price_in: f64,
        price_out: f64,
    },
    /// The L4 permission gate decided a tool call (P6-4 audit trail):
    /// which tool, the verdict (`allow`/`ask`/`deny`), and why.
    /// Audit-only — never rehydrates into messages.
    PermissionDecision {
        tool: String,
        verdict: String,
        reason: String,
    },
    /// Policy rule files were loaded for this session: where from and
    /// how many rules took effect. Audit-only, like `MemoryUpdated`;
    /// carries paths/counts only, never secret material.
    PolicyLoad {
        path: String,
        rules: u64,
    },
    /// A sandbox backend refused to run a command (pinned runtime
    /// unavailable, seatbelt deny, seccomp violation…). Audit-only —
    /// the failed call itself is recorded by its ToolResult.
    SandboxDenial {
        backend: String,
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: u64,
    pub parent_id: Option<u64>,
    pub ts_ms: u64,
    /// Hash chain over (id, parent_id, kind tag, prev_hash) — FNV-1a,
    /// see `event_hash`. `#[serde(default)]` keeps pre-chain logs
    /// loadable; old events verify as chain genesis (prev 0).
    #[serde(default)]
    pub prev_hash: u64,
    /// `event_hash(id, parent_id, kind tag, prev_hash)` at append time.
    #[serde(default)]
    pub hash: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

/// Structural chain hash for one event (FNV-1a, 64-bit, not
/// cryptographic).
///
/// Feeds `id` (LE bytes), `parent_id` (`u64::MAX` for `None` — distinct
/// from any real id), the serde `type` tag of `kind`, and `prev` (0 for the
/// chain head). It detects structural tampering (reordered, inserted,
/// dropped or re-typed events, broken parent links); it does NOT detect a
/// payload edit — payload bytes are not hashed. Deterministic, std-only.
// DEFERRED(owner): payload hashing — needs a versioned hash format so existing logs still verify.
pub fn event_hash(id: u64, parent_id: Option<u64>, type_str: &str, prev: u64) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut h = FNV_OFFSET;
    let mut mix = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
    };
    mix(&id.to_le_bytes());
    mix(&parent_id.unwrap_or(u64::MAX).to_le_bytes());
    mix(type_str.as_bytes());
    mix(&prev.to_le_bytes());
    h
}

/// The serde `type` tag for an [`EventKind`] — the stable string
/// [`event_hash`] chains over. One arm per variant; update with the enum.
pub fn event_type_str(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::SessionStart { .. } => "session_start",
        EventKind::UserInput { .. } => "user_input",
        EventKind::ModelResponse { .. } => "model_response",
        EventKind::ToolCallStart { .. } => "tool_call_start",
        EventKind::ToolResult { .. } => "tool_result",
        EventKind::TurnEnd { .. } => "turn_end",
        EventKind::RunEnd { .. } => "run_end",
        EventKind::SubagentDone { .. } => "subagent_done",
        EventKind::Tainted { .. } => "tainted",
        EventKind::ComputerAct { .. } => "computer_act",
        EventKind::StuckDetected { .. } => "stuck_detected",
        EventKind::Nudge { .. } => "nudge",
        EventKind::Compaction { .. } => "compaction",
        EventKind::MemoryUpdated { .. } => "memory_updated",
        EventKind::ConsentGranted { .. } => "consent_granted",
        EventKind::Error { .. } => "error",
        EventKind::ModelSwitch { .. } => "model_switch",
        EventKind::PermissionDecision { .. } => "permission_decision",
        EventKind::PolicyLoad { .. } => "policy_load",
        EventKind::SandboxDenial { .. } => "sandbox_denial",
    }
}

/// Verify the hash chain over a replayed log: each event's `hash` must
/// equal `event_hash(id, parent_id, type tag, prev_hash)`, and each
/// `prev_hash` must equal the previous event's `hash` (0 for the head).
/// Empty logs verify. Pre-chain events (both hashes 0) verify as genesis;
/// a mixed log verifies while the chain is unbroken from the first
/// hashed event on.
pub fn verify_chain(events: &[Event]) -> bool {
    let mut prev = 0u64;
    let mut hashed_seen = false;
    for e in events {
        if e.prev_hash == 0 && e.hash == 0 {
            // Pre-chain event: only valid before any hashed event.
            if hashed_seen {
                return false;
            }
            continue;
        }
        if e.prev_hash != prev {
            return false;
        }
        if e.hash != event_hash(e.id, e.parent_id, event_type_str(&e.kind), e.prev_hash) {
            return false;
        }
        prev = e.hash;
        hashed_seen = true;
    }
    true
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
#[cfg(test)]
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
    /// Running chain hash: the `hash` of the last appended (or replayed)
    /// event, 0 for an empty / pre-chain log. Seeds `prev_hash` on append.
    prev_hash: u64,
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
            prev_hash: 0,
        })
    }

    /// Open an existing log for append (resume): replays to find next_id/head.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let events = Self::replay(&path)?;
        let (next_id, head, last, prev_hash) = match events.last() {
            Some(e) => (e.id + 1, Some(e.id), Some(e.clone()), e.hash),
            None => (1, None, None, 0),
        };
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(EventLog {
            file,
            path,
            next_id,
            head,
            last,
            prev_hash,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append an event; returns the assigned id. Buffer-flushed every call,
    /// fsync only on `flush()` (turn boundaries — durable-tail semantics).
    /// Stamps the hash chain (`prev_hash` + `hash`); Invariant 1 still
    /// holds — past lines are never rewritten.
    pub fn append(&mut self, kind: EventKind) -> std::io::Result<u64> {
        let id = self.next_id;
        let prev_hash = self.prev_hash;
        let hash = event_hash(id, self.head, event_type_str(&kind), prev_hash);
        let ev = Event {
            id,
            parent_id: self.head,
            ts_ms: now_ms(),
            prev_hash,
            hash,
            kind,
        };
        let mut line = serde_json::to_string(&ev).map_err(std::io::Error::other)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        self.head = Some(id);
        self.next_id += 1;
        self.prev_hash = hash;
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

    /// Replay the whole log. Strict: only the final non-empty line may be
    /// unparseable (a torn tail from a crash mid-write — durable-tail
    /// semantics mean everything before the last fsync is valid). A corrupt
    /// line anywhere else is `InvalidData` naming its 1-based line number,
    /// never a silent truncation of the events after it.
    pub fn replay(path: impl AsRef<Path>) -> std::io::Result<Vec<Event>> {
        let path = path.as_ref();
        let f = File::open(path)?;
        let mut lines = Vec::new();
        for (i, line) in BufReader::new(f).lines().enumerate() {
            let line = line?;
            if !line.trim().is_empty() {
                lines.push((i + 1, line));
            }
        }
        let last = lines.len().saturating_sub(1);
        let mut out = Vec::with_capacity(lines.len());
        for (k, (lineno, line)) in lines.iter().enumerate() {
            match serde_json::from_str::<Event>(line) {
                Ok(ev) => out.push(ev),
                Err(_) if k == last => break,
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{}: corrupt event at line {lineno}: {e}", path.display()),
                    ));
                }
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

/// How many of the most recent `tools` op=search results stay pinned in the
/// view: they carry the deferred schemas the model is calling against.
pub const PINNED_SEARCHES: usize = 3;

/// P1.2 stale tool-result clearing: keep the last `keep` `ToolResult` blocks
/// verbatim; replace older ones' content with `CLEARED_RESULT`. The block
/// itself (and its `tool_use_id`) survives so tool_use/tool_result pairing
/// stays valid for the provider. The last [`PINNED_SEARCHES`] results of
/// `tools` op=search calls are never cleared — identified by their
/// `tool_use_id` matching an assistant `ToolCall` block, never by content.
///
/// Pure view transform over the message view — events on disk are never
/// touched, so a cleared session rehydrates the full history and re-clears
/// deterministically. Returns how many results were (re)written.
pub fn clear_stale_tool_results(messages: &mut [crate::ir::Message], keep: usize) -> usize {
    let searches: HashSet<&str> = messages
        .iter()
        .filter(|m| m.role == crate::ir::Role::Assistant)
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            Block::ToolCall { id, name, input }
                if name == "tools" && crate::tools::tools_tool::op_is(input, "search") =>
            {
                Some(id.as_str())
            }
            _ => None,
        })
        .collect();
    let results: Vec<&str> = messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            Block::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    let stale = results.len().saturating_sub(keep);
    if stale == 0 {
        return 0;
    }
    let pinned: HashSet<String> = results
        .iter()
        .rev()
        .filter(|id| searches.contains(*id))
        .take(PINNED_SEARCHES)
        .map(|id| id.to_string())
        .collect();
    let mut seen = 0usize;
    let mut cleared = 0usize;
    for m in messages.iter_mut() {
        for b in m.content.iter_mut() {
            if let Block::ToolResult {
                tool_use_id,
                content,
                ..
            } = b
            {
                if seen < stale && !pinned.contains(tool_use_id.as_str()) {
                    *content = CLEARED_RESULT.to_string();
                    cleared += 1;
                }
                seen += 1;
            }
        }
    }
    cleared
}

/// Placeholder that replaces an elided image block. Fixed text so the
/// transform is idempotent.
pub const ELIDED_IMAGE: &str = "[screenshot elided — take a new one if needed]";

/// F5: keep at most the `keep` most recent `Image` blocks in the model
/// view; older ones become [`ELIDED_IMAGE`] text. Every image in the
/// view is a computer capture (the only producer is
/// `tools::computer::image_block`), and each still rides its own
/// tool-result's envelope text — tool_use/tool_result pairing is
/// untouched.
///
/// Same view-transform contract as [`clear_stale_tool_results`]: events
/// are never mutated, so a resumed session's rebuilt view behaves
/// identically (images never rehydrate — only their envelopes do, which
/// the `(image not persisted across resume)` note in the envelope
/// already says).
pub fn cap_image_blocks(messages: &mut [crate::ir::Message], keep: usize) -> usize {
    let total = messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| matches!(b, crate::ir::Block::Image { .. }))
        .count();
    let stale = total.saturating_sub(keep);
    if stale == 0 {
        return 0;
    }
    let mut seen = 0usize;
    for m in messages.iter_mut() {
        for b in m.content.iter_mut() {
            if seen < stale {
                if let crate::ir::Block::Image { .. } = b {
                    *b = crate::ir::Block::Text {
                        text: ELIDED_IMAGE.to_string(),
                    };
                    seen += 1;
                }
            } else {
                return stale;
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

    fn img() -> crate::ir::Block {
        crate::ir::Block::Image {
            media_type: "image/png".into(),
            data_b64: "aGVsbG8=".into(),
            px_w: 2560,
            px_h: 1600,
            sent_w: 1280,
            sent_h: 800,
        }
    }

    #[test]
    fn cap_image_blocks_keeps_the_last_two_and_preserves_pairing() {
        // Five captures across five tool-result messages (as the agent
        // loop pushes them: ToolResult then sibling Image).
        let mut messages: Vec<crate::ir::Message> = (0..5)
            .map(|i| {
                crate::ir::Message::tool_results(vec![
                    crate::ir::Block::ToolResult {
                        tool_use_id: format!("toolu_{i}"),
                        content: format!("envelope-{i}"),
                        is_error: false,
                    },
                    img(),
                ])
            })
            .collect();
        assert_eq!(cap_image_blocks(&mut messages, 2), 3);
        // Only the last two image blocks remain; older ones are the
        // fixed placeholder text — and every ToolResult survived.
        let mut images = 0;
        let mut tool_results = 0;
        let mut elided = 0;
        for (i, m) in messages.iter().enumerate() {
            for b in &m.content {
                match b {
                    crate::ir::Block::Image { .. } => {
                        images += 1;
                        assert!(i >= 3, "image {i} should have been elided");
                    }
                    crate::ir::Block::ToolResult { tool_use_id, .. } => {
                        tool_results += 1;
                        assert_eq!(tool_use_id, &format!("toolu_{i}"), "pairing intact");
                    }
                    crate::ir::Block::Text { text } => {
                        assert_eq!(text, ELIDED_IMAGE);
                        elided += 1;
                    }
                    _ => panic!("unexpected block"),
                }
            }
        }
        assert_eq!((images, tool_results, elided), (2, 5, 3));
        // Idempotent — a second pass is a byte-identical no-op.
        assert_eq!(cap_image_blocks(&mut messages, 2), 0);
        // Under the cap, nothing changes.
        let mut few = vec![crate::ir::Message::tool_results(vec![img()])];
        assert_eq!(cap_image_blocks(&mut few, 2), 0);
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
    fn clear_stale_pins_the_last_three_tools_searches_by_call_id() {
        let call = |id: &str, name: &str, input: serde_json::Value| crate::ir::Message {
            role: Role::Assistant,
            content: vec![Block::ToolCall {
                id: id.into(),
                name: name.into(),
                input,
            }],
        };
        let result = |id: &str, text: &str| {
            crate::ir::Message::tool_results(vec![Block::ToolResult {
                tool_use_id: id.into(),
                content: text.into(),
                is_error: false,
            }])
        };
        let search = serde_json::json!({"op": "search", "query": "x"});
        let mut msgs = Vec::new();
        for i in 0..4 {
            msgs.push(call(&format!("s{i}"), "tools", search.clone()));
            msgs.push(result(&format!("s{i}"), &format!("schemas-{i}")));
        }
        // Content that merely looks like a search result is not pinned.
        msgs.push(call(
            "c",
            "tools",
            serde_json::json!({"op": "call", "name": "plan"}),
        ));
        msgs.push(result("c", "{\"name\":\"plan\",\"input_schema\":{}}"));
        msgs.push(call("b", "bash", serde_json::json!({"command": "ls"})));
        msgs.push(result("b", "recent"));
        let cleared = clear_stale_tool_results(&mut msgs, 1);
        let text = |m: &crate::ir::Message| match &m.content[0] {
            Block::ToolResult { content, .. } => content.clone(),
            _ => panic!("expected ToolResult"),
        };
        assert_eq!(text(&msgs[1]), CLEARED_RESULT, "4th-newest search clears");
        for i in 1..4 {
            assert_eq!(text(&msgs[2 * i + 1]), format!("schemas-{i}"));
        }
        assert_eq!(text(&msgs[9]), CLEARED_RESULT, "no content sniffing");
        assert_eq!(text(&msgs[11]), "recent");
        assert_eq!(cleared, 2);
        // Idempotent.
        let snapshot = msgs.clone();
        clear_stale_tool_results(&mut msgs, 1);
        assert_eq!(msgs, snapshot);
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
                prev_hash: 0,
                hash: 0,
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
                prev_hash: 0,
                hash: 0,
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
                prev_hash: 0,
                hash: 0,
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
                prev_hash: 0,
                hash: 0,
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

    #[test]
    fn chain_append_verifies_and_tamper_fails() {
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
            log.append(EventKind::PermissionDecision {
                tool: "bash".into(),
                verdict: "deny".into(),
                reason: "exfil gate".into(),
            })
            .unwrap();
            log.flush().unwrap();
        }
        let events = EventLog::replay(&path).unwrap();
        assert_eq!(events.len(), 3);
        assert!(verify_chain(&events), "fresh appends must verify");
        assert!(verify_chain(&[]), "empty log verifies");
        // Resume continues the chain (open seeds prev_hash from the tail).
        {
            let mut log = EventLog::open(&path).unwrap();
            log.append(EventKind::SandboxDenial {
                backend: "seatbelt".into(),
                reason: "deny net".into(),
            })
            .unwrap();
        }
        let resumed = EventLog::replay(&path).unwrap();
        assert_eq!(resumed.len(), 4);
        assert!(
            verify_chain(&resumed),
            "resumed append must extend the chain"
        );
        // Tamper with a payload tag (kind swap keeps the envelope): fails.
        let mut tampered = resumed.clone();
        tampered[1].kind = EventKind::UserInput {
            text: "forged".into(),
        };
        // Same-tag content edit does NOT trip the tag chain (payloads are
        // not hashed by design) — the tag swap below is what must fail.
        assert!(verify_chain(&tampered));
        tampered[1].kind = EventKind::Nudge {
            text: "forged".into(),
        };
        assert!(!verify_chain(&tampered), "kind swap must fail verify");
        // Splice (drop an event): prev link breaks.
        let mut spliced = resumed.clone();
        spliced.remove(1);
        assert!(!verify_chain(&spliced), "splice must fail verify");
        // Reorder: id/prev links break.
        let mut reordered = resumed.clone();
        reordered.swap(0, 1);
        assert!(!verify_chain(&reordered), "reorder must fail verify");
    }

    #[test]
    fn old_logs_load_and_verify_as_genesis() {
        // Pre-chain line: no prev_hash/hash keys at all (defaults to 0).
        let dir = tmpdir();
        let path = dir.join("events.jsonl");
        std::fs::write(
            &path,
            "{\"id\":1,\"ts_ms\":1,\"type\":\"session_start\",\"session_id\":\"old\",\"cwd\":\"/w\",\"model\":\"m\",\"harness_version\":\"0\"}\n\
             {\"id\":2,\"parent_id\":1,\"ts_ms\":2,\"type\":\"user_input\",\"text\":\"hi\"}\n",
        )
        .unwrap();
        let events = EventLog::replay(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].prev_hash, 0);
        assert_eq!(events[0].hash, 0);
        assert!(verify_chain(&events), "old logs verify as genesis");
        // Appending to an old log starts the chain from 0 (prev 0).
        let mut log = EventLog::open(&path).unwrap();
        log.append(EventKind::PolicyLoad {
            path: "rules".into(),
            rules: 3,
        })
        .unwrap();
        let events = EventLog::replay(&path).unwrap();
        assert_eq!(events.len(), 3);
        assert!(verify_chain(&events));
        assert_eq!(events[2].prev_hash, 0);
    }

    #[test]
    fn new_kinds_round_trip() {
        let dir = tmpdir();
        let path = dir.join("events.jsonl");
        {
            let mut log = EventLog::create(&path).unwrap();
            log.append(EventKind::PermissionDecision {
                tool: "write".into(),
                verdict: "ask".into(),
                reason: "outside workspace".into(),
            })
            .unwrap();
            log.append(EventKind::PolicyLoad {
                path: "/tmp/rules".into(),
                rules: 7,
            })
            .unwrap();
            log.append(EventKind::SandboxDenial {
                backend: "bubblewrap".into(),
                reason: "pinned runtime missing".into(),
            })
            .unwrap();
        }
        let events = EventLog::replay(&path).unwrap();
        assert_eq!(events.len(), 3);
        assert!(verify_chain(&events));
        match &events[0].kind {
            EventKind::PermissionDecision {
                tool,
                verdict,
                reason,
            } => {
                assert_eq!(tool, "write");
                assert_eq!(verdict, "ask");
                assert_eq!(reason, "outside workspace");
            }
            other => panic!("expected PermissionDecision, got {other:?}"),
        }
        match &events[1].kind {
            EventKind::PolicyLoad { path, rules } => {
                assert_eq!(path, "/tmp/rules");
                assert_eq!(*rules, 7);
            }
            other => panic!("expected PolicyLoad, got {other:?}"),
        }
        match &events[2].kind {
            EventKind::SandboxDenial { backend, reason } => {
                assert_eq!(backend, "bubblewrap");
                assert_eq!(reason, "pinned runtime missing");
            }
            other => panic!("expected SandboxDenial, got {other:?}"),
        }
        // Audit-only: none rehydrate into model messages.
        assert!(rehydrate_messages(&events).is_empty());
    }

    /// C6: a corrupt NON-final line is an error naming its 1-based line
    /// number — not a silent truncation of everything after it.
    #[test]
    fn corrupt_middle_line_errors_with_line_number() {
        let dir = tmpdir();
        let path = dir.join("events.jsonl");
        {
            let mut log = EventLog::create(&path).unwrap();
            log.append(EventKind::UserInput { text: "ok".into() })
                .unwrap();
            log.flush().unwrap();
        }
        let good = std::fs::read_to_string(&path).unwrap();
        let good = good.trim_end();
        std::fs::write(&path, format!("{good}\n{{garbage\n\n{good}\n")).unwrap();
        let err = EventLog::replay(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("line 2"), "{err}");
    }

    /// C6: a torn tail followed only by blank lines is still the tail.
    #[test]
    fn torn_tail_before_trailing_blank_lines_is_tolerated() {
        let dir = tmpdir();
        let path = dir.join("events.jsonl");
        {
            let mut log = EventLog::create(&path).unwrap();
            log.append(EventKind::UserInput { text: "a".into() })
                .unwrap();
            log.append(EventKind::UserInput { text: "b".into() })
                .unwrap();
            log.flush().unwrap();
        }
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"id\":3,\"partial\n\n  \n").unwrap();
        drop(f);
        let events = EventLog::replay(&path).unwrap();
        assert_eq!(events.len(), 2);
    }

    /// K6 guard: a pre-extension `run_end` line (no cache fields) must keep
    /// replaying once RunEnd grows `#[serde(default)]` fields.
    #[test]
    fn old_shape_run_end_replays() {
        let old = r#"{"type":"run_end","stop_reason":"end_turn","steps":3,"total_cost_usd":0.0}"#;
        let kind: EventKind = serde_json::from_str(old).unwrap();
        assert!(matches!(kind, EventKind::RunEnd { steps: 3, .. }));
        let dir = tmpdir();
        let path = dir.join("events.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n",
                old.replacen('{', r#"{"id":1,"parent_id":null,"ts_ms":0,"#, 1)
            ),
        )
        .unwrap();
        let events = EventLog::replay(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].kind, EventKind::RunEnd { steps: 3, .. }));
    }
}
