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
                text: seg,
                cacheable: true,
            });
        }
    }
    // --- DYNAMIC boundary: non-cacheable per-turn sections go below. ---
    segments
}

fn seg(_name: &'static str, text: &str) -> SystemSegment {
    SystemSegment {
        text: text.to_string(),
        cacheable: true,
    }
}

/// Boundary lint used by tests and future assemblers: every cacheable
/// segment must precede every non-cacheable one.
pub fn boundary_ok(segments: &[SystemSegment]) -> bool {
    let mut dynamic_seen = false;
    for s in segments {
        if !s.cacheable {
            dynamic_seen = true;
        } else if dynamic_seen {
            return false;
        }
    }
    true
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
    fn boundary_and_no_volatile_content() {
        let mut cfg = AgentConfig::default();
        let dir = std::env::temp_dir().join(format!("overseer-prompt-{}", uuid::Uuid::now_v7()));
        cfg.memory_dir = Some(dir);
        let segs = assemble(&cfg);
        assert!(boundary_ok(&segs));
        assert_eq!(segs.len(), 4); // 3 static + memory index
                                   // Nothing volatile may live above the boundary.
        for s in &segs[..3] {
            for needle in ["ts_ms", "timestamp", "session_id", "uuid"] {
                assert!(
                    !s.text.contains(needle),
                    "volatile '{needle}' in static section"
                );
            }
        }
    }
}
