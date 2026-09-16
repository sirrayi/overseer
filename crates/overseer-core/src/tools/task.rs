//! Subagents (playbook Ch.3 §9.6, P3.4).
//!
//! Any subtask expected to read >~20–30K tokens of material runs in an
//! *isolated* context and returns ≤1–2K tokens: the parent gets a compact
//! digest plus a path to the subagent's full event log (share traces, not
//! summaries — Cognition).
//!
//! Three modes on one tool:
//! - `read` (default): read/grep/glob only — writes stay single-threaded
//!   in the parent so two agents can never race on the same file.
//! - `write`: full tool registry inside an isolated `git worktree` under
//!   the session dir — the subagent's edits land on a scratch branch,
//!   never in the user's checkout; the digest carries a bounded diffstat.
//! - `background: true` + either mode: fire-and-notify. The call returns
//!   a task id immediately; the subagent runs on a thread and its digest
//!   lands in the parent's context at the next step boundary via the
//!   `SubagentDone` event. Fan-out is bounded (MAX_CONCURRENT_BG).
//!
//! One model per subagent: the subagent inherits the parent's model —
//! no model arg is accepted. The subagent is just `Agent` with a swapped
//! registry — same loop, same budgets, same event sourcing.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use serde_json::{json, Value};

use super::{middle_truncate, need_str, opt_u64, schema, ToolCtx, ToolOutput, ToolRegistry};
use crate::provider::ToolSpec;

/// Digest cap ≈2K tokens — the quarantine contract: the parent receives a
/// bounded result plus the trace path, never the raw transcript.
const RETURN_CAP: usize = 8_000;
const DEFAULT_STEPS: u64 = 10;
const MAX_STEPS: u64 = 20;
/// Bounded fan-out (P3.4): at most this many background subagents may be
/// in flight at once; further spawns are refused with a repair hint.
pub const MAX_CONCURRENT_BG: usize = 4;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "task".into(),
        description: concat!(
            "Spawn a subagent for a self-contained subtask in an isolated ",
            "context. It returns a compact digest (~2K tokens) plus the path ",
            "to its full trace. mode=read (default) has read/grep/glob only ",
            "and cannot modify files. mode=write gets full tools inside an ",
            "isolated git worktree (its changes land on a scratch branch, ",
            "never your checkout). background=true returns immediately and ",
            "the digest arrives as a notice when done (max 4 in flight)."
        )
        .into(),
        input_schema: schema(
            json!({
                "prompt": {
                    "type": "string",
                    "description": "Self-contained subtask: what to find out or change, and where."
                },
                "mode": {
                    "type": "string",
                    "enum": ["read", "write"],
                    "description": "read (default): read-only quarantine. write: full tools in an isolated git worktree."
                },
                "background": {
                    "type": "boolean",
                    "description": "true: run on a thread and notify when done (max 4 concurrent). default: false (block for the digest)."
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

/// Shared subtask config: tight step ceiling, no shared memory view,
/// same model + effort as the parent (one model per subagent).
fn sub_cfg(ctx: &ToolCtx, input: &Value, cwd: &Path) -> crate::agent::AgentConfig {
    let mut cfg = ctx.agent_config.clone().unwrap_or_default();
    cfg.max_steps = opt_u64(input, "max_steps")
        .unwrap_or(DEFAULT_STEPS)
        .clamp(1, MAX_STEPS) as u32;
    cfg.memory_dir = None;
    cfg.cwd = cwd.to_path_buf();
    cfg.auto_compact = false; // 20-step ceiling can't fill a window
    cfg
}

/// Run one subagent to completion; returns (digest, outcome_debug).
fn run_subagent(
    provider: Arc<dyn crate::provider::Provider>,
    cfg: crate::agent::AgentConfig,
    registry: ToolRegistry,
    dir: &Path,
    id: &str,
    prompt: &str,
) -> (String, String) {
    let mut sub = match crate::agent::Agent::start(provider, cfg, dir.to_path_buf(), id.to_string())
    {
        Ok(a) => a.with_tools(registry),
        Err(e) => return (format!("cannot start subagent: {e}"), "spawn-error".into()),
    };
    let mut sink = |_: &crate::event::Event| {};
    let outcome = sub.run_turn(prompt, &mut sink);
    let tail_text = sub.messages().last().map(|m| m.text()).unwrap_or_default();
    let digest = middle_truncate(&tail_text, RETURN_CAP);
    match outcome {
        Ok(o) => (digest, format!("{o:?}")),
        Err(e) => (digest, format!("run-error: {e}")),
    }
}

/// Count background tasks still in flight: `bg-*` dirs without a
/// `done.txt` marker. Deterministic and crash-safe — no in-memory state.
fn bg_in_flight(subagents_dir: &Path) -> usize {
    std::fs::read_dir(subagents_dir)
        .map(|d| {
            d.flatten()
                .filter(|e| {
                    e.file_name().to_string_lossy().starts_with("bg-")
                        && !e.path().join("done.txt").exists()
                })
                .count()
        })
        .unwrap_or(0)
}

/// `git worktree add` for a writer subagent. Returns the worktree path
/// and the scratch branch name. Fails cleanly when cwd isn't a repo.
fn add_worktree(cwd: &Path, dir: &Path, seq: u64) -> Result<(std::path::PathBuf, String), String> {
    let branch = format!("overseer-task-{seq}");
    let wt = dir.join("wt");
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["worktree", "add", "-b", &branch])
        .arg(&wt)
        .output()
        .map_err(|e| format!("git worktree: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok((wt, branch))
}

/// Bounded diffstat of what a writer subagent changed in its worktree.
fn worktree_diffstat(wt: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["diff", "--stat", "HEAD"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            middle_truncate(&s, 4_000)
        }
        _ => "(diffstat unavailable)".into(),
    }
}

pub fn run(input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    let prompt = match need_str(input, "prompt") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(provider) = ctx.provider.clone() else {
        return ToolOutput::err("task: no provider handle in this tool context");
    };
    if ctx.agent_config.is_none() {
        return ToolOutput::err("task: no agent config in this tool context");
    }
    let write_mode = input
        .get("mode")
        .and_then(Value::as_str)
        .map(|m| m == "write")
        .unwrap_or(false);
    let background = input
        .get("background")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    ctx.subagent_seq += 1;
    let seq = ctx.subagent_seq;
    let id = format!("task-{seq}");
    let subagents_dir = ctx.session_dir.join("subagents");

    // Bounded fan-out — the check is filesystem-derived so it survives
    // resumes and can't drift from actual thread state.
    if background && bg_in_flight(&subagents_dir) >= MAX_CONCURRENT_BG {
        return ToolOutput::err(format!(
            "task: {MAX_CONCURRENT_BG} background subagents already in flight — \
             wait for a notice or run this one with background=false"
        ));
    }

    // Writer subagents isolate into a git worktree (P3.4); readers share
    // the parent's cwd — they can't write anyway.
    let mut worktree_branch = None;
    let sub_cwd = if write_mode {
        let dir = subagents_dir.join(format!("wt-{seq}"));
        match add_worktree(&ctx.cwd, &dir, seq) {
            Ok((wt, branch)) => {
                worktree_branch = Some(branch);
                wt
            }
            Err(e) => {
                return ToolOutput::err(format!("task: mode=write needs a git worktree — {e}"))
            }
        }
    } else {
        ctx.cwd.clone()
    };

    let dir = if background {
        subagents_dir.join(format!("bg-{seq}"))
    } else {
        subagents_dir.join(&id)
    };
    let cfg = sub_cfg(ctx, input, &sub_cwd);
    let policy = if cfg.full_access {
        crate::perm::Policy::allow_all()
    } else {
        crate::perm::Policy::preset(cfg.policy_preset, sub_cwd.clone())
    };
    let registry = if write_mode {
        ToolRegistry::core(policy)
    } else {
        ToolRegistry::readonly(policy)
    };

    if background {
        // Claim the slot in the parent before spawning — the in-flight
        // count is derived from these dirs, so a burst of spawns would
        // otherwise all pass the bound before any thread started.
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return ToolOutput::err(format!("task: cannot create subagent dir — {e}"));
        }
        let dir2 = dir.clone();
        let prompt = prompt.to_string();
        let id2 = id.clone();
        std::thread::spawn(move || {
            let (digest, outcome) = run_subagent(provider, cfg, registry, &dir2, &id2, &prompt);
            // Marker last: done.txt is the parent loop's notification.
            let _ = std::fs::write(
                dir2.join("done.txt"),
                format!("{digest}\n\n[subagent {id2} {outcome}]"),
            );
        });
        return ToolOutput::ok(format!(
            "Background task {id} started (trace: {}). Its digest arrives \
             as a notice at the next step — do not block waiting for it.",
            dir.display()
        ));
    }

    let (mut digest, outcome) = run_subagent(provider, cfg, registry, &dir, &id, prompt);
    if let Some(branch) = &worktree_branch {
        let wt = subagents_dir.join(format!("wt-{seq}/wt"));
        digest.push_str(&format!(
            "\n\n[worktree branch `{branch}` at {} — changes:\n{}]",
            wt.display(),
            worktree_diffstat(&wt)
        ));
    }
    ToolOutput::ok(format!(
        "{digest}\n\n[subagent {outcome} — full trace: {}]",
        dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Usage;
    use crate::provider::{Provider, ProviderError, Request, Response, StopReason};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct Mock {
        responses: Mutex<VecDeque<Response>>,
    }

    impl Provider for Mock {
        fn complete(&self, _req: &Request) -> Result<Response, ProviderError> {
            let mut q = self.responses.lock().unwrap();
            let r = if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q.front().unwrap().clone()
            };
            Ok(r)
        }
        fn name(&self) -> &'static str {
            "mock"
        }
    }

    fn done_text(t: &str) -> Response {
        Response {
            blocks: vec![crate::ir::Block::Text { text: t.into() }],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        }
    }

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-task-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ctx(dir: &Path) -> ToolCtx<'static> {
        ToolCtx {
            cwd: dir.to_path_buf(),
            session_dir: dir.join("session"),
            spill_seq: 0,
            provider: Some(Arc::new(Mock {
                responses: Mutex::new(VecDeque::from(vec![done_text("digest body")])),
            })),
            agent_config: Some(crate::agent::AgentConfig {
                cwd: dir.to_path_buf(),
                full_access: true,
                ..Default::default()
            }),
            subagent_seq: 0,
            checkpoint: None,
            sandbox: false,
        }
    }

    #[test]
    fn read_subagent_returns_bounded_digest() {
        let dir = tmpdir();
        let mut c = ctx(&dir);
        let out = run(&json!({"prompt": "look around"}), &mut c);
        assert!(!out.is_error);
        assert!(out.text.contains("digest body"));
        assert!(out.text.contains("full trace"));
        assert!(dir.join("session/subagents/task-1").exists());
    }

    #[test]
    fn background_spawns_and_marks_done() {
        let dir = tmpdir();
        let mut c = ctx(&dir);
        let out = run(&json!({"prompt": "bg work", "background": true}), &mut c);
        assert!(!out.is_error);
        assert!(out.text.contains("Background task task-1 started"));
        // Wait for the thread to write the marker.
        let marker = dir.join("session/subagents/bg-1/done.txt");
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(marker.exists(), "bg subagent must write done.txt");
        assert!(std::fs::read_to_string(&marker)
            .unwrap()
            .contains("digest body"));
        // Fan-out count drops to zero once done.
        assert_eq!(bg_in_flight(&dir.join("session/subagents")), 0);
    }

    #[test]
    fn fan_out_is_bounded() {
        let dir = tmpdir();
        let sub = dir.join("session/subagents");
        for i in 0..MAX_CONCURRENT_BG {
            std::fs::create_dir_all(sub.join(format!("bg-{i}"))).unwrap();
        }
        assert_eq!(bg_in_flight(&sub), MAX_CONCURRENT_BG);
        let mut c = ctx(&dir);
        let out = run(&json!({"prompt": "x", "background": true}), &mut c);
        assert!(out.is_error);
        assert!(out.text.contains("in flight"));
        // A done marker frees the slot.
        std::fs::write(sub.join("bg-0/done.txt"), "done").unwrap();
        assert_eq!(bg_in_flight(&sub), MAX_CONCURRENT_BG - 1);
    }

    #[test]
    fn write_mode_uses_worktree() {
        let dir = tmpdir();
        // Needs a git repo at cwd.
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "x",
                "--allow-empty",
            ],
        ] {
            let st = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(&args)
                .output()
                .unwrap()
                .status;
            assert!(st.success());
        }
        let mut c = ctx(&dir);
        let out = run(&json!({"prompt": "write stuff", "mode": "write"}), &mut c);
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("overseer-task-1"));
        assert!(dir.join("session/subagents/wt-1/wt").exists());
    }

    #[test]
    fn write_mode_fails_cleanly_outside_git() {
        let dir = tmpdir(); // not a repo
        let mut c = ctx(&dir);
        let out = run(&json!({"prompt": "x", "mode": "write"}), &mut c);
        assert!(out.is_error);
        assert!(out.text.contains("git worktree"));
    }
}
