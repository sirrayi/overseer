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
const NOTE_CAP: usize = 400;
const ERROR_CAP: usize = 300;
const PENDING_CAP: usize = 1_000;
const MAX_LISTED_FILES: usize = 50;
const MAX_NOTES: usize = 3;
const MAX_ERRORS: usize = 3;

/// Result of a salience pre-filter (selective-context pattern, arsenal B2):
/// the kept text plus what it cost, so the caller can report the loss
/// instead of hiding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Salient {
    pub text: String,
    pub kept: usize,
    pub dropped: usize,
}

/// Sentence-level salience pre-filter: keep the most informative `ratio` of
/// a long text before it enters the context, preserving the original order.
///
/// Score per sentence (deterministic, no model): the summed inverse document
/// frequency of its tokens within this text (a token that appears once is
/// what makes a sentence specific), plus a bonus for identifier-shaped
/// tokens (digits, `_`, CamelCase, paths) and for a leading position.
/// The first sentence is always kept — it is the one that says what the
/// text is about.
///
/// `ratio >= 1.0`, a text with fewer than three sentences, or an empty text
/// returns the input unchanged (a pre-filter that rewrites short text is
/// all cost and no benefit). This is a *lossy* view: callers must treat
/// `dropped > 0` as a signal worth surfacing, never as a silent trim.
pub fn salience_filter(text: &str, ratio: f64) -> Salient {
    let sentences = split_sentences(text);
    if !ratio.is_finite() || ratio >= 1.0 || sentences.len() < 3 {
        return Salient {
            kept: sentences.len(),
            dropped: 0,
            text: text.to_string(),
        };
    }
    let ratio = ratio.max(0.0);
    let keep_n = (sentences.len() as f64 * ratio).ceil() as usize;
    let keep_n = keep_n.clamp(1, sentences.len());
    if keep_n == sentences.len() {
        return Salient {
            kept: sentences.len(),
            dropped: 0,
            text: text.to_string(),
        };
    }

    // Document frequency over sentences (the whole "document" is `text`).
    let toks: Vec<Vec<String>> = sentences.iter().map(|s| words(s)).collect();
    let mut df: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for t in &toks {
        let uniq: std::collections::HashSet<&str> = t.iter().map(String::as_str).collect();
        for w in uniq {
            *df.entry(w).or_insert(0) += 1;
        }
    }
    let scored: Vec<(usize, f64)> = sentences
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let mut score = 0.0f64;
            for w in &toks[i] {
                let freq = df.get(w.as_str()).copied().unwrap_or(1).max(1);
                score += 1.0 / freq as f64;
                if is_identifier_like(w) {
                    score += 0.5;
                }
            }
            // Head bias: the opening sentence frames the rest.
            if i == 0 {
                score += 1.0;
            }
            (i, score)
        })
        .collect();
    // Select the top `keep_n` by score; ties break on the earlier index so
    // the result is stable (selection must never depend on map order).
    let mut by_score = scored.clone();
    by_score.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    by_score.truncate(keep_n);
    // Always keep the first sentence, whatever the scoring said.
    if !by_score.iter().any(|(i, _)| *i == 0) {
        by_score.pop();
        by_score.push((0, scored[0].1));
    }
    let mut keep: Vec<usize> = by_score.into_iter().map(|(i, _)| i).collect();
    keep.sort_unstable();
    let kept_text = keep
        .iter()
        .map(|i| sentences[*i].trim())
        .collect::<Vec<_>>()
        .join("\n");
    Salient {
        text: kept_text,
        kept: keep.len(),
        dropped: sentences.len() - keep.len(),
    }
}

/// Sentence-ish split: on `.`/`!`/`?` followed by whitespace or end, and on
/// hard newlines. Blank fragments are dropped.
fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\n' {
            if !cur.trim().is_empty() {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
            continue;
        }
        cur.push(c);
        if matches!(c, '.' | '!' | '?') {
            let next_ws = chars.peek().is_none_or(|n| n.is_whitespace());
            if next_ws {
                chars.next_if(|n| *n == ' ');
                if !cur.trim().is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Lowercased word tokens.
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// Identifier-shaped token: has a digit, an underscore, a path separator, or
/// an interior capital. These are the tokens a coding context must not lose.
fn is_identifier_like(raw: &str) -> bool {
    let has_digit = raw.chars().any(|c| c.is_ascii_digit());
    let has_us = raw.contains('_');
    let has_path = raw.contains('/') || raw.contains('.');
    let has_inner_cap = raw.chars().skip(1).any(|c| c.is_ascii_uppercase());
    has_digit || has_us || has_path || has_inner_cap
}

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

    let mut goal: Option<String> = None;
    let mut modified: Vec<String> = Vec::new();
    let mut read_only: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;
    let mut n_compactions = 0u32;

    for e in covered {
        match &e.kind {
            EventKind::UserInput { text } => {
                if goal.is_none() {
                    goal = Some(truncate(text, GOAL_CAP));
                }
            }
            EventKind::ToolCallStart { name, input, .. } => {
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
    if let Some(g) = goal {
        out.push_str(&format!("\n## Goal (first user message, verbatim)\n{g}\n"));
    }
    if !modified.is_empty() {
        out.push_str("\n## Files modified\n");
        out.push_str(&file_list_block(&modified));
    }
    if !read_only.is_empty() {
        out.push_str("\n## Files read\n");
        out.push_str(&file_list_block(&read_only));
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

/// Vendor-native compaction seam (Phase-C gate).
///
/// A provider adapter implements [`NativeCompaction`] when the vendor API
/// offers a native compaction/condensation endpoint. The default is
/// opt-out: `supports_native()` returns `false` and `native_compact()`
/// returns `None`, so the engine always falls back to the deterministic
/// [`summarize`]/[`tail_anchor`] path. Zero behavior change to the existing
/// compaction path while no adapter opts in.
// DEFERRED(anthropic): Anthropic native-compaction endpoint hook — gate: vendor API + keys.
// DEFERRED(gemini): Gemini native-compaction (context-condensation) endpoint hook — gate: vendor API + keys.
pub trait NativeCompaction {
    /// Run vendor-native compaction over `events`. Returns
    /// `(summary, tail_from)` on success, `None` when the provider cannot
    /// or declines to compact (engine falls back to [`summarize`]).
    /// Default: `None` (opt-out).
    fn native_compact(&self, events: &[Event]) -> Option<(String, u64)> {
        let _ = events;
        None
    }

    /// True when this provider implements a native compaction endpoint.
    /// Default: `false` (opt-out).
    fn supports_native(&self) -> bool {
        false
    }
}

/// Explicit vendor-native-compaction allowlist (model prefixes).
///
/// Empty: no model advertises native compaction yet, so the gate below is
/// fail-closed `false` for every input. Entries are added only together
/// with a real adapter-side `NativeCompaction` impl behind the DEFERRED
/// vendor hooks above.
const NATIVE_COMPACT_ALLOWLIST: &[&str] = &[];

/// True when `model` may use vendor-native compaction.
///
/// Consults the profile registry (`known` models only; unknown models
/// resolve to FALLBACK and stay `false`) and additionally requires a prefix
/// hit in `NATIVE_COMPACT_ALLOWLIST`. Fail-closed default `false`; the local
/// deterministic path (`summarize` + `tail_anchor`) remains the only active
/// compaction until a vendor hook lands.
// DEFERRED(profile): move this allowlist into `ModelProfile.native_compact` — gate: profile.rs owned by ParamFilter slice.
pub fn provider_compact_capability(model: &str) -> bool {
    if !crate::profile::known(model) {
        return false;
    }
    NATIVE_COMPACT_ALLOWLIST
        .iter()
        .any(|prefix| model.starts_with(prefix))
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

    struct OptOut;
    impl NativeCompaction for OptOut {}

    /// Blanket default: an adapter that does not opt in declines natively
    /// and reports no native support — the engine keeps the deterministic path.
    #[test]
    fn native_compact_defaults_to_none() {
        let events: Vec<Event> = vec![];
        let p = OptOut;
        assert!(!p.supports_native());
        assert_eq!(p.native_compact(&events), None);
    }

    /// Fail-closed gate: false for known, unknown, and empty model ids
    /// until a vendor hook + allowlist entry land.
    #[test]
    fn compact_capability_defaults_false() {
        for model in [
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
            "no-such-model-xyz",
            "",
        ] {
            assert!(
                !provider_compact_capability(model),
                "native compaction must stay off for {model:?}"
            );
        }
    }

    /// The seam is additive: the deterministic path ignores it entirely.
    #[test]
    fn summarize_ignores_native_seam() {
        let events = vec![ev(
            1,
            EventKind::UserInput {
                text: "goal stays local".into(),
            },
        )];
        let s = summarize(&events, 10);
        assert!(s.contains("goal stays local"));
        assert!(!provider_compact_capability("claude-sonnet-4-5"));
    }
}
