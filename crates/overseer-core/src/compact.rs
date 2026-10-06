//! Deterministic structured compaction (playbook Ch.3 §9 items 1, 2, 5).
//!
//! Compaction is a *view* over the immutable event log, never a mutation:
//! a `Compaction` event records `{summary, tail_from}` — events with
//! `id < tail_from` are represented by the summary, events `>= tail_from`
//! replay verbatim (the recency tail). On resume, `rehydrate_messages`
//! reconstructs exactly the view the live loop was using.
//!
//! The summary is derived mechanically from the raw event log — goal,
//! files touched, open errors, pending state. It is NEVER produced by
//! summarizing a previous summary (ACE's context-collapse warning): each
//! compaction re-derives from all covered events, so no information is
//! progressively lost.
//!
//! Boundary rule: `tail_from` always points at a `ModelResponse` event, so
//! the verbatim tail begins with an assistant message and every tool_use in
//! it is answered by a tool_result inside the tail (Anthropic pairing).

use crate::event::{Event, EventKind};
use crate::ir::Block;

/// How many recent turns survive compaction verbatim.
pub const TAIL_TURNS: usize = 2;

const GOAL_CAP: usize = 4_000;
/// Total cap on the "Earlier requests" section (chars).
const EARLIER_CAP: usize = 1_500;
const NOTE_CAP: usize = 400;
const ERROR_CAP: usize = 300;
const PENDING_CAP: usize = 1_000;
const MAX_LISTED_FILES: usize = 50;
const MAX_NOTES: usize = 3;
const MAX_ERRORS: usize = 3;

/// Choose the tail anchor: the event id of the `ModelResponse` that begins
/// the last `keep` turns. Returns `None` when there aren't enough turns to
/// make compaction worthwhile (dropping nothing is a no-op).
///
/// `floor` is the previous compaction's `tail_from` (0 if none): the anchor
/// must exceed it so a second compaction can never re-include already-
/// summarized events in its tail.
pub fn tail_anchor(events: &[Event], keep: usize, floor: u64) -> Option<u64> {
    let turn_starts: Vec<u64> = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ModelResponse { .. }) && e.id > floor)
        .map(|e| e.id)
        .collect();
    // Need at least keep+1 turns: `keep` stay verbatim and ≥1 gets dropped —
    // otherwise compaction would be a no-op.
    if turn_starts.len() <= keep {
        return None;
    }
    Some(turn_starts[turn_starts.len() - keep])
}

/// The most recent compaction boundary in the log, if any.
pub fn latest(events: &[Event]) -> Option<(String, u64)> {
    events.iter().rev().find_map(|e| match &e.kind {
        EventKind::Compaction { summary, tail_from } => Some((summary.clone(), *tail_from)),
        _ => None,
    })
}

/// Build the fixed-schema summary over all events with `id < tail_from`.
/// Mechanical extraction only — no model call, no freeform prose.
pub fn summarize(events: &[Event], tail_from: u64) -> String {
    let covered = events.iter().filter(|e| e.id < tail_from);

    let mut requests: Vec<&str> = Vec::new();
    let mut modified: Vec<String> = Vec::new();
    let mut read_only: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;
    let mut n_compactions = 0u32;
    let mut deferred: Vec<String> = Vec::new();

    for e in covered {
        match &e.kind {
            EventKind::UserInput { text } => requests.push(text),
            EventKind::ToolCallStart { name, input, .. } => {
                // Deferred tools the model called through `tools` — their
                // schemas were loaded by an op=search now condensed away.
                if name == "tools" && crate::tools::tools_tool::op_is(input, "call") {
                    if let Some(inner) = input
                        .get("name")
                        .and_then(|n| n.as_str())
                        .filter(|n| is_deferred_name(n))
                    {
                        if !deferred.iter().any(|d| d == inner) {
                            deferred.push(inner.to_string());
                        }
                    }
                }
                if let Some(path) = input.get("path").and_then(|p| p.as_str()) {
                    let list = match name.as_str() {
                        "write" | "edit" => &mut modified,
                        "read" => &mut read_only,
                        _ => continue,
                    };
                    if !list.iter().any(|p| p == path) {
                        list.push(path.to_string());
                    }
                }
            }
            EventKind::ModelResponse { blocks, .. } => {
                let text: String = blocks
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let text = text.trim().to_string();
                if !text.is_empty() {
                    pending = Some(truncate(&text, PENDING_CAP));
                    notes.push(truncate(&text, NOTE_CAP));
                    if notes.len() > MAX_NOTES {
                        notes.remove(0);
                    }
                }
            }
            EventKind::ToolResult {
                name,
                content,
                is_error,
                denied,
                ..
            } if *is_error => {
                let tag = if *denied { "denied" } else { "error" };
                errors.push(format!(
                    "`{name}` ({tag}): {}",
                    truncate(content, ERROR_CAP)
                ));
                if errors.len() > MAX_ERRORS {
                    errors.remove(0);
                }
            }
            EventKind::Compaction { .. } => n_compactions += 1,
            _ => {}
        }
    }

    let mut out = String::from(
        "[overseer compaction v1 — earlier history condensed from the event \
         log; the raw log is unchanged and fully replayable]\n",
    );
    if n_compactions > 0 {
        out.push_str(&format!(
            "(supersedes {n_compactions} earlier compaction(s))\n"
        ));
    }
    if let Some((latest, earlier)) = requests.split_last() {
        out.push_str(&format!(
            "\n## Goal (latest user request, verbatim)\n{}\n",
            truncate(latest, GOAL_CAP)
        ));
        if !earlier.is_empty() {
            out.push_str("\n## Earlier requests (newest first)\n");
            out.push_str(&earlier_block(earlier));
        }
    }
    if !modified.is_empty() {
        out.push_str("\n## Files modified\n");
        out.push_str(&file_list_block(&modified));
    }
    if !read_only.is_empty() {
        out.push_str("\n## Files read\n");
        out.push_str(&file_list_block(&read_only));
    }
    if !deferred.is_empty() {
        out.push_str(&format!(
            "\n## Deferred tools used (search `tools` again for their schemas)\n{}\n",
            deferred.join(", ")
        ));
    }
    if !notes.is_empty() {
        out.push_str("\n## Recent assistant notes (verbatim)\n");
        for (i, n) in notes.iter().enumerate() {
            out.push_str(&format!("{}. {n}\n", i + 1));
        }
    }
    if !errors.is_empty() {
        out.push_str("\n## Open errors (most recent)\n");
        for e in &errors {
            out.push_str(&format!("- {e}\n"));
        }
    }
    if let Some(p) = pending {
        out.push_str(&format!(
            "\n## Pending state (last assistant text, verbatim)\n{p}\n"
        ));
    }
    out
}

/// Earlier user requests, newest first, `EARLIER_CAP` chars in total;
/// the entry that crosses the cap is cut and the rest are counted.
fn earlier_block(earlier: &[&str]) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for (i, text) in earlier.iter().rev().enumerate() {
        let left = EARLIER_CAP.saturating_sub(used);
        if left == 0 {
            out.push_str(&format!("(+{} older)\n", earlier.len() - i));
            break;
        }
        let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let n = one.chars().count();
        let line = if n > left {
            let head: String = one.chars().take(left).collect();
            format!("{head}…[{n} chars]")
        } else {
            one
        };
        used += n.min(left);
        out.push_str(&format!("- {line}\n"));
    }
    out
}

/// Render a file list for the compaction summary (B1-3 TOON path).
/// Bullet list below 10 entries (header overhead would not pay); TOON
/// single-column table at 10+ (header once + rows, ~40% byte cut).
fn file_list_block(files: &[String]) -> String {
    let listed: Vec<&String> = files.iter().take(MAX_LISTED_FILES).collect();
    if listed.len() < 10 {
        let mut out = String::new();
        for p in listed {
            out.push_str(&format!("- {p}\n"));
        }
        return out;
    }
    let rows: Vec<serde_json::Value> = listed
        .iter()
        .map(|p| serde_json::json!({"path": p}))
        .collect();
    match crate::toon::encode_table(&rows) {
        Some(t) => t,
        None => {
            let mut out = String::new();
            for p in listed {
                out.push_str(&format!("- {p}\n"));
            }
            out
        }
    }
}

fn truncate(s: &str, cap: usize) -> String {
    let n = s.chars().count();
    if n <= cap {
        s.to_string()
    } else {
        let head: String = s.chars().take(cap).collect();
        format!("{head}…[{n} chars total]")
    }
}

// DEFERRED(owner): vendor-native compaction (Anthropic/Gemini condensation endpoints) — the always-false NativeCompaction/provider_compact_capability seam was removed as dead code; reintroduce it with the first real adapter impl — gate: vendor API + keys.

/// A deferred tool's inner name (`mcp` is the router, not a tool) or an
/// MCP tool's `mcp__server__tool` name.
fn is_deferred_name(name: &str) -> bool {
    name.starts_with("mcp__")
        || (name != "mcp" && crate::tools::tools_tool::DEFERRED.contains(&name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, Usage};

    fn ev(id: u64, kind: EventKind) -> Event {
        Event {
            id,
            parent_id: id.checked_sub(1),
            ts_ms: 0,
            prev_hash: 0,
            hash: 0,
            kind,
        }
    }

    fn model_resp(blocks: Vec<Block>) -> EventKind {
        EventKind::ModelResponse {
            blocks,
            usage: Usage::default(),
            stop_reason: "tool_use".into(),
            latency_ms: 0,
            cost_usd: 0.0,
        }
    }

    fn tool_turn(id: u64, name: &str) -> Vec<Event> {
        vec![
            ev(
                id,
                model_resp(vec![Block::ToolCall {
                    id: format!("c{id}"),
                    name: name.into(),
                    input: serde_json::json!({"path": format!("/f/{id}.txt"), "command": "true"}),
                }]),
            ),
            ev(
                id + 1,
                EventKind::ToolResult {
                    call_id: format!("c{id}"),
                    name: name.into(),
                    content: "ok".into(),
                    is_error: false,
                    raw_bytes: 2,
                    spilled_to: None,
                    denied: false,
                },
            ),
            ev(
                id + 2,
                EventKind::TurnEnd {
                    step: (id / 10) as u32,
                },
            ),
        ]
    }

    /// 4 tool turns → anchor lands on the 3rd ModelResponse; 2-turn tail.
    #[test]
    fn tail_anchor_keeps_last_turns() {
        let mut events = vec![
            ev(
                1,
                EventKind::SessionStart {
                    session_id: "s".into(),
                    cwd: "/t".into(),
                    model: "m".into(),
                    harness_version: "0".into(),
                    parent: None,
                },
            ),
            ev(
                2,
                EventKind::UserInput {
                    text: "do it".into(),
                },
            ),
        ];
        for t in 0..4 {
            events.extend(tool_turn(10 + t * 10, "read"));
        }
        // ModelResponse ids: 10, 20, 30, 40 → anchor = 30 (keep last 2 turns).
        assert_eq!(tail_anchor(&events, 2, 0), Some(30));
        // Only 2 turns exist → nothing droppable → no-op.
        assert_eq!(tail_anchor(&events[..8], 2, 0), None);
        // Floor excludes earlier anchors: only turn 40 survives it.
        assert_eq!(tail_anchor(&events, 2, 30), None);
    }

    #[test]
    fn summary_lists_deferred_tools_called_through_tools() {
        let start = |id: u64, name: &str, input: serde_json::Value| {
            ev(
                id,
                EventKind::ToolCallStart {
                    call_id: format!("c{id}"),
                    name: name.into(),
                    input,
                },
            )
        };
        let events = vec![
            ev(1, EventKind::UserInput { text: "go".into() }),
            start(
                2,
                "tools",
                serde_json::json!({"op": "search", "query": "struct"}),
            ),
            // A search result naming a tool is never parsed (Invariant 8).
            ev(
                3,
                EventKind::ToolResult {
                    call_id: "c2".into(),
                    name: "tools".into(),
                    content: "{\"name\":\"struct_search\"}".into(),
                    is_error: false,
                    raw_bytes: 1,
                    spilled_to: None,
                    denied: false,
                },
            ),
            start(
                4,
                "tools",
                serde_json::json!({"op": "call", "name": "repo_map", "args": {}}),
            ),
            start(
                5,
                "tools",
                serde_json::json!({"op": "call", "name": "mcp__gh__issue", "args": {}}),
            ),
            start(
                6,
                "tools",
                serde_json::json!({"op": "call", "name": "repo_map", "args": {}}),
            ),
            // Resident, router and unknown names are not deferred tools,
            // even when the call went through `tools` (here: and failed).
            start(
                7,
                "tools",
                serde_json::json!({"op": "call", "name": "read", "args": {"path": "a"}}),
            ),
            ev(
                8,
                EventKind::ToolResult {
                    call_id: "c7".into(),
                    name: "tools".into(),
                    content: "tools: 'read' is resident; call it directly".into(),
                    is_error: true,
                    raw_bytes: 1,
                    spilled_to: None,
                    denied: false,
                },
            ),
            start(
                9,
                "tools",
                serde_json::json!({"op": "call", "name": "mcp", "args": {}}),
            ),
            start(
                10,
                "tools",
                serde_json::json!({"op": "call", "name": "bogus", "args": {}}),
            ),
        ];
        let s = summarize(&events, 100);
        assert!(
            s.contains("## Deferred tools used (search `tools` again for their schemas)\nrepo_map, mcp__gh__issue\n"),
            "{s}"
        );
        assert!(!s.contains("struct_search"), "{s}");
        let none = summarize(&events[..3], 100);
        assert!(!none.contains("Deferred tools"), "{none}");
    }

    #[test]
    fn summarize_extracts_schema() {
        let mut events = vec![
            ev(
                1,
                EventKind::UserInput {
                    text: "fix the parser bug".into(),
                },
            ),
            ev(
                2,
                EventKind::ToolCallStart {
                    call_id: "a".into(),
                    name: "edit".into(),
                    input: serde_json::json!({"path": "src/parser.rs"}),
                },
            ),
            ev(
                3,
                EventKind::ToolCallStart {
                    call_id: "b".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "src/lib.rs"}),
                },
            ),
            ev(
                4,
                EventKind::ToolResult {
                    call_id: "b".into(),
                    name: "read".into(),
                    content: "ENOENT".into(),
                    is_error: true,
                    raw_bytes: 6,
                    spilled_to: None,
                    denied: false,
                },
            ),
            ev(
                5,
                model_resp(vec![Block::Text {
                    text: "editing parser now".into(),
                }]),
            ),
        ];
        events.extend(tool_turn(6, "read"));
        let s = summarize(&events, 6);
        assert!(s.contains("fix the parser bug"));
        assert!(s.contains("src/parser.rs"));
        assert!(s.contains("src/lib.rs"));
        assert!(s.contains("`read` (error): ENOENT"));
        assert!(s.contains("editing parser now"));
        // Events >= tail_from are not folded into the summary.
        assert!(!s.contains("/f/6.txt"));
    }

    #[test]
    fn file_list_block_toon_threshold() {
        // B1-3: <10 files → bullets; >=10 → TOON table with every path.
        let few: Vec<String> = (0..3).map(|i| format!("src/a{i}.rs")).collect();
        let b = file_list_block(&few);
        assert!(b.contains("- src/a0.rs"), "bullets below threshold");
        assert!(!b.contains("TOON"), "no table below threshold");
        let many: Vec<String> = (0..12).map(|i| format!("src/a{i}.rs")).collect();
        let t = file_list_block(&many);
        assert!(t.contains("TOON"), "table at threshold, got:\n{t}");
        for i in 0..12 {
            assert!(t.contains(&format!("src/a{i}.rs")), "path {i} survives");
        }
    }

    #[test]
    fn compaction_never_resummarizes() {
        // A second compaction re-derives from raw events — the old summary
        // text contributes nothing but a supersede counter.
        let events = vec![
            ev(
                1,
                EventKind::UserInput {
                    text: "goal".into(),
                },
            ),
            ev(
                2,
                EventKind::Compaction {
                    summary: "OLD SUMMARY".into(),
                    tail_from: 5,
                },
            ),
            ev(
                3,
                EventKind::ToolCallStart {
                    call_id: "a".into(),
                    name: "write".into(),
                    input: serde_json::json!({"path": "new.txt"}),
                },
            ),
        ];
        let s = summarize(&events, 10);
        assert!(s.contains("supersedes 1 earlier compaction"));
        assert!(!s.contains("OLD SUMMARY"));
        assert!(s.contains("new.txt"));
    }
}
