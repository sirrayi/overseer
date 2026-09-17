//! Stable-prefix prompt assembler (playbook Ch.5 §5, Ch.3 §9.3 — P1.1).
//!
//! The system prompt is a *pipeline of named sections*, not a string. The
//! STATIC/DYNAMIC boundary is explicit: every cacheable section precedes
//! every non-cacheable one, and nothing volatile (timestamps, session ids,
//! counters) may appear above the boundary — the documented cache killers.
//! The memory index sits in the LAST static slot: its content can change,
//! but an edit only invalidates cache from that segment onward.
//!
//! `assemble()` is deterministic: same config → byte-identical output, so a
//! diff between turns can only come from a section that declared it.

use crate::agent::AgentConfig;
use crate::provider::SystemSegment;

/// Who/what the engine is. Fixed across all sessions.
const IDENTITY: &str = "\
You are Overseer, an agentic coding engine running as a CLI on the user's machine.";

/// How to use the toolset. Fixed across all sessions.
const CONTRACT: &str = "\
Use the tools to accomplish the task. Prefer dedicated tools over bash for file operations.\n\
Keep prose between tool calls under 25 words. Verify work with builds/tests when available.\n\
Large tool outputs are spilled to files — read or grep them by path for more.";

/// Appended to CONTRACT unless the `task` tool is ablated — a disabled arm
/// must not advertise a tool the model can never call (P4.3 confound).
const CONTRACT_TASK: &str = "\
\nDelegate read-heavy subtasks to the `task` subagent — it investigates in an \
isolated context and returns a compact digest.";

/// Non-negotiable behavior constraints. Fixed across all sessions.
const SAFETY: &str = "\
Never claim a file was edited, created, or verified unless a tool call \
actually did it. If a tool errors, read the error before retrying differently.";

/// Computer use (P7-3). Static — backend state is deliberately absent
/// (it would be a cache killer); the tool reports an unconfigured backend
/// itself when it is called.
const COMPUTER: &str = "\
Computer use is tiered: a structured API first, then element lookups by name/role, then pixel acts.\n\
Captures report the sent and native frame sizes — give coordinates in the frame you were shown; they are scaled for you.\n\
A suppressed capture returns metadata only, and credential fields are never typed into.";

/// Assemble the ordered system segments for a request. Section order is the
/// wire order — reordering a cacheable section is a cache-breaking change
/// and must be deliberate.
pub fn assemble(config: &AgentConfig) -> Vec<SystemSegment> {
    let contract = if config.disabled_tools.iter().any(|t| t == "task") {
        CONTRACT.to_string()
    } else {
        format!("{CONTRACT}{CONTRACT_TASK}")
    };
    let mut segments = vec![
        seg("identity", IDENTITY),
        seg("contract", &contract),
        seg("safety", SAFETY),
    ];
    // Last static slot: the memory index (volatile content, stable position —
    // cache invalidates only from here when the index changes).
    if let Some(dir) = &config.memory_dir {
        segments.push(SystemSegment {
            name: "memory",
            text: crate::memory::index_segment(dir),
            cacheable: true,
        });
    }
    // Skill metadata (P3.5): resident pointer lines; bodies load on
    // demand via the `skill` tool. Static tail — installs are rare.
    // Suppressed when `skill` is ablated: the index advertises a tool the
    // arm removed.
    if !config.disabled_tools.iter().any(|t| t == "skill") {
        if let Some(seg) = crate::skills::index_segment(&config.cwd) {
            segments.push(SystemSegment {
                name: "skills",
                text: seg,
                cacheable: true,
            });
        }
    }
    // Computer use (P7-3): advertised only while the `computer` tool is
    // resident — an ablated arm must not describe a tool the model cannot
    // call (P4.3 confound). Last static slot, after `skills`.
    if !config.disabled_tools.iter().any(|t| t == "computer") {
        segments.push(seg("computer", COMPUTER));
    }
    // --- DYNAMIC boundary: non-cacheable per-turn sections go below. ---
    segments
}

fn seg(name: &'static str, text: &str) -> SystemSegment {
    SystemSegment {
        name,
        text: text.to_string(),
        cacheable: true,
    }
}

/// Frozen static section order — the P6/P7 union (R1-F2): the static
/// sections assemble identity→contract→safety→memory→skills with each
/// branch adding only its own segment (`persona` on P6, `computer` on
/// P7). Absent optionals are skipped; order among the present must be
/// preserved. Reordering a cacheable section breaks prefix-cache hits and
/// must be deliberate.
pub const ORDER: &[&str] = &[
    "identity", "contract", "safety", "memory", "skills", "persona", "computer",
];

/// Boundary lint used by tests and future assemblers: every cacheable
/// segment must precede every non-cacheable one.
pub fn boundary_ok(segments: &[SystemSegment]) -> bool {
    // Cacheable-before-dynamic boundary.
    let mut dynamic_seen = false;
    for s in segments {
        if !s.cacheable {
            dynamic_seen = true;
        } else if dynamic_seen {
            return false;
        }
    }
    // Frozen static order.
    let mut last_rank: Option<usize> = None;
    for s in segments {
        if !s.cacheable {
            continue;
        }
        let Some(rank) = ORDER.iter().position(|n| *n == s.name) else {
            return false; // unknown static section: fail closed
        };
        if last_rank.is_some_and(|r| rank <= r) && last_rank != Some(rank) {
            return false;
        }
        // Equal ranks (same section twice) are tolerated; regressions fail.
        if last_rank.is_some_and(|r| rank < r) {
            return false;
        }
        last_rank = Some(rank);
    }
    true
}

/// Structural fingerprint of the prompt prefix (B1-10): sha256 over the
/// ordered `name:cacheable` pairs — structure, not content. Stable across
/// identical assembles; changes on reorder/rename. Recorded in the manifest.
pub fn prefix_fingerprint(segments: &[SystemSegment]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for s in segments {
        h.update(s.name.as_bytes());
        h.update(b":");
        h.update(if s.cacheable { b"1" } else { b"0" });
        h.update(b";");
    }
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_is_deterministic() {
        let cfg = AgentConfig::default();
        let a = assemble(&cfg);
        let b = assemble(&cfg);
        let ta: Vec<&str> = a.iter().map(|s| s.text.as_str()).collect();
        let tb: Vec<&str> = b.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(ta, tb);
    }

    #[test]
    fn order_lint_rejects_reorder() {
        // B1-10: frozen identity→contract→safety order; a swap fails.
        let bad = vec![
            seg("contract", "c"),
            seg("identity", "i"),
            seg("safety", "s"),
        ];
        assert!(!boundary_ok(&bad), "reordered static sections must fail");
        let good = vec![
            seg("identity", "i"),
            seg("contract", "c"),
            seg("safety", "s"),
        ];
        assert!(boundary_ok(&good));
        // Unknown static section names fail closed.
        let unknown = vec![SystemSegment {
            name: "mystery",
            text: "x".into(),
            cacheable: true,
        }];
        assert!(!boundary_ok(&unknown));
    }

    #[test]
    fn fingerprint_stable_and_order_sensitive() {
        // B1-10: identical assembles share a fingerprint; reorder changes it.
        let cfg = AgentConfig::default();
        let a = assemble(&cfg);
        let b = assemble(&cfg);
        assert_eq!(prefix_fingerprint(&a), prefix_fingerprint(&b));
        let mut swapped = a.clone();
        swapped.swap(0, 1);
        assert_ne!(prefix_fingerprint(&a), prefix_fingerprint(&swapped));
    }

    #[test]
    fn boundary_and_no_volatile_content() {
        let mut cfg = AgentConfig::default();
        let dir = std::env::temp_dir().join(format!("overseer-prompt-{}", uuid::Uuid::now_v7()));
        cfg.memory_dir = Some(dir);
        let segs = assemble(&cfg);
        assert!(boundary_ok(&segs));
        // Union-safe shape check (replaces the branch-local `segs.len()==4`
        // pin): the memory index is present, every static section is named
        // in the frozen union ORDER, and the boundary holds. P6 adds
        // `persona`, P7 adds `computer` — neither branch may pin a count.
        assert!(segs.iter().any(|s| s.name == "memory"));
        assert!(segs.iter().any(|s| s.name == "computer"));
        for s in &segs {
            if s.cacheable {
                assert!(
                    ORDER.contains(&s.name),
                    "static section '{}' not in ORDER",
                    s.name
                );
            }
        }
        // Nothing volatile may live above the boundary.
        for s in segs.iter().filter(|s| s.cacheable) {
            for needle in ["ts_ms", "timestamp", "session_id", "uuid"] {
                assert!(
                    !s.text.contains(needle),
                    "volatile '{needle}' in static section"
                );
            }
        }
    }
}
