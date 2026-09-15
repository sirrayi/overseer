//! Read-only subagent (playbook Ch.3 §9.6 — read-quarantine).
//!
//! Any subtask expected to read >~20–30K tokens of material runs in an
//! *isolated* context and returns ≤1–2K tokens: the parent gets a compact
//! digest plus a path to the subagent's full event log (share traces, not
//! summaries — Cognition). The subagent's registry is read-only
//! (read/grep/glob): writes stay single-threaded in the parent, so two
//! agents can never race on the same file.
//!
//! The subagent is just `Agent` with a swapped registry — same loop, same
//! budgets, same event sourcing — spawned under the parent's session dir.

use serde_json::{json, Value};

use super::{middle_truncate, need_str, opt_u64, schema, ToolCtx, ToolOutput, ToolRegistry};
use crate::provider::ToolSpec;

/// Digest cap ≈2K tokens — the quarantine contract: the parent receives a
/// bounded result plus the trace path, never the raw transcript.
const RETURN_CAP: usize = 8_000;
const DEFAULT_STEPS: u64 = 10;
const MAX_STEPS: u64 = 20;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "task".into(),
        description: concat!(
            "Spawn a read-only subagent to investigate a self-contained subtask ",
            "in an isolated context. Use for work that would read a lot of ",
            "material — it returns only a compact digest (~2K tokens) plus the ",
            "path to its full trace. It has read/grep/glob only and CANNOT ",
            "modify files; do not use it for edits."
        )
        .into(),
        input_schema: schema(
            json!({
                "prompt": {
                    "type": "string",
                    "description": "Self-contained subtask: what to find out and where to look."
                },
                "max_steps": {
                    "type": "integer",
                    "description": "Step budget for the subagent (default 10, max 20)."
                }
            }),
            &["prompt"],
        ),
    }
}

pub fn run(input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    let prompt = match need_str(input, "prompt") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(provider) = ctx.provider else {
        return ToolOutput::err("task: no provider handle in this tool context");
    };
    let Some(mut sub_cfg) = ctx.agent_config.clone() else {
        return ToolOutput::err("task: no agent config in this tool context");
    };

    ctx.subagent_seq += 1;
    let dir = ctx
        .session_dir
        .join("subagents")
        .join(format!("task-{}", ctx.subagent_seq));

    // Quarantine constraints: read-only registry, no shared memory view,
    // tight step ceiling, same model + cwd as the parent.
    sub_cfg.max_steps = opt_u64(input, "max_steps")
        .unwrap_or(DEFAULT_STEPS)
        .clamp(1, MAX_STEPS) as u32;
    sub_cfg.memory_dir = None;
    sub_cfg.cwd = ctx.cwd.clone();
    sub_cfg.auto_compact = false; // 20-step ceiling can't fill a window
    let policy = if sub_cfg.full_access {
        crate::perm::Policy::allow_all()
    } else {
        crate::perm::Policy::headless(ctx.cwd.clone())
    };

    let mut sub = match crate::agent::Agent::start(
        provider,
        sub_cfg,
        dir.clone(),
        format!("task-{}", ctx.subagent_seq),
    ) {
        Ok(a) => a.with_tools(ToolRegistry::readonly(policy)),
        Err(e) => return ToolOutput::err(format!("task: cannot start subagent: {e}")),
    };

    let mut sink = |_: &crate::event::Event| {};
    let outcome = sub.run_turn(prompt, &mut sink);
    let tail_text = sub.messages().last().map(|m| m.text()).unwrap_or_default();
    let digest = middle_truncate(&tail_text, RETURN_CAP);

    match outcome {
        Ok(o) => ToolOutput::ok(format!(
            "{digest}\n\n[subagent {o:?} — full trace: {}]",
            dir.display()
        )),
        Err(e) => ToolOutput::err(format!("task: subagent failed: {e}")),
    }
}
