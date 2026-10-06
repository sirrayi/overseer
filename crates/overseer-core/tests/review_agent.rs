//! Independent adversarial review — area `agent` (agent.rs, ledger.rs Gate,
//! event.rs chain v2 / SubagentDone / torn tail, session.rs fork, harden.rs
//! repair_torn_tail, tools/task cleanup+verify+cancel, perm latches).
//! Each `#[ignore = "review: <id>"]` test FAILS at cfc56a6 and documents one
//! finding; run them with `--ignored`. Scripted providers only: no live
//! calls, no spend.

use overseer_core::agent::{Agent, AgentConfig, ReflectMode};
use overseer_core::event::{Event, EventKind, EventLog};
use overseer_core::ir::{Block, Message, Role, Usage};
use overseer_core::ledger::{request_tokens, Gate, Ledger};
use overseer_core::provider::{Provider, ProviderError, Request, Response, StopReason};
use overseer_core::tools::task::budget::MIN_CAP_USD;
use overseer_core::tools::task::SpendAccount;
use overseer_core::tools::{self, ToolCtx};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ── harness (mirrors tests/audit_orchestration.rs) ─────────────────────────

#[derive(Clone)]
struct Seen {
    messages: Vec<Message>,
}

type Brain = Box<dyn FnMut(&Request) -> Response + Send>;

/// A provider driven by a closure; records every request it sees.
struct Scripted {
    brain: Mutex<Brain>,
    seen: Mutex<Vec<Seen>>,
}

impl Scripted {
    fn new(f: impl FnMut(&Request) -> Response + Send + 'static) -> Arc<Self> {
        Arc::new(Scripted {
            brain: Mutex::new(Box::new(f)),
            seen: Mutex::new(Vec::new()),
        })
    }
    fn queue(v: Vec<Response>) -> Arc<Self> {
        let mut q: VecDeque<Response> = v.into();
        Self::new(move |_| {
            if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q.front().expect("scripted response").clone()
            }
        })
    }
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Provider for Scripted {
    fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        self.seen.lock().unwrap().push(Seen {
            messages: req.messages.to_vec(),
        });
        let mut b = self.brain.lock().unwrap();
        Ok((b)(req))
    }
    fn name(&self) -> &'static str {
        "scripted"
    }
}

fn usage() -> Usage {
    Usage {
        fresh_input: 500,
        output: 40,
        ..Usage::default()
    }
}

fn text(t: &str) -> Response {
    Response {
        blocks: vec![Block::Text { text: t.into() }],
        stop_reason: StopReason::EndTurn,
        usage: usage(),
        request_bytes: 0,
        latency_ms: 0,
    }
}

fn call(id: &str, name: &str, input: Value) -> Response {
    Response {
        blocks: vec![Block::ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }],
        stop_reason: StopReason::ToolUse,
        usage: usage(),
        request_bytes: 0,
        latency_ms: 0,
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "overseer-review-agent-{tag}-{}",
        uuid::Uuid::now_v7()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn repo(ws: &Path) {
    std::fs::create_dir_all(ws).unwrap();
    git(ws, &["init", "-q"]);
    std::fs::write(ws.join("a.txt"), "alpha\n").unwrap();
    std::fs::write(ws.join(".gitignore"), "build-out/\n").unwrap();
    git(ws, &["add", "."]);
    git(ws, &["commit", "-qm", "x"]);
}

/// A headless config: no memory, no learning, no reflection, plain bash.
fn cfg(ws: &Path) -> AgentConfig {
    AgentConfig {
        cwd: ws.to_path_buf(),
        full_access: true,
        sandbox_bash: false,
        model: "claude-sonnet-5".into(),
        learn: false,
        reflect: ReflectMode::Off,
        ..AgentConfig::default()
    }
}

fn task_ctx(ws: &Path, session_dir: PathBuf, p: Arc<Scripted>) -> ToolCtx<'static> {
    ToolCtx {
        cwd: ws.to_path_buf(),
        session_dir,
        spill_seq: 0,
        provider: Some(p),
        agent_config: Some(cfg(ws)),
        subagents: tools::task::SubagentCtx {
            control: Default::default(),
            seq: 0,
            spend: Some(Arc::new(SpendAccount::new(5.0, 0.0))),
        },
        checkpoint: None,
        sandbox: false,
        broker: None,
    }
}

fn session_start(log: &mut EventLog) {
    log.append(EventKind::SessionStart {
        session_id: "s".into(),
        cwd: "/w".into(),
        model: "claude-sonnet-5".into(),
        harness_version: "0".into(),
        parent: None,
    })
    .unwrap();
}

/// Every assistant tool_use is answered by a tool_result in the very next
/// message (the provider pairing rule). Returns the unanswered ids.
fn unpaired(msgs: &[Message]) -> Vec<String> {
    let mut bad = Vec::new();
    for (i, m) in msgs.iter().enumerate() {
        if m.role != Role::Assistant {
            continue;
        }
        let next: Vec<&str> = msgs
            .get(i + 1)
            .map(|n| {
                n.content
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (id, _, _) in m.tool_calls() {
            if !next.contains(&id) {
                bad.push(id.to_string());
            }
        }
    }
    bad
}

// ── findings ──────────────────────────────────────────────────────────────

/// review:dangling-reused-id — `Agent::close_dangling_calls` (agent.rs
/// ~637-670) builds ONE global `answered` set of every `ToolResult.call_id`
/// in the log, then skips any `ToolCall` whose id is in it. Providers that
/// recycle call ids per response (`call_0`, `call_1`, … — common on
/// OpenAI-compat gateways) reuse an id a later turn already consumed: a
/// crash mid-turn leaves that later call "answered" by the earlier turn's
/// result, so no synthetic `ToolResult` is written and the resumed view
/// has an unpaired `tool_use` — the next request violates the provider's
/// pairing rule and the session cannot continue.
#[test]
fn resume_reused_call_id_pairs_every_tool_use() {
    let dir = tmpdir("dangling-id");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let s = dir.join("s");
    std::fs::create_dir_all(&s).unwrap();
    let mut log = EventLog::create(s.join("events.jsonl")).unwrap();
    Ledger::create(s.join("ledger.jsonl")).unwrap();
    session_start(&mut log);
    log.append(EventKind::UserInput {
        text: "list files".into(),
    })
    .unwrap();
    // Turn 1: call id "c1" runs and is answered.
    {
        let (id, name, input) = ("c1", "glob", json!({"pattern": "*"}));
        log.append(EventKind::ModelResponse {
            blocks: vec![Block::ToolCall {
                id: id.into(),
                name: name.into(),
                input: input.clone(),
            }],
            usage: Usage::default(),
            stop_reason: "tool_use".into(),
            latency_ms: 0,
            cost_usd: 0.0,
        })
        .unwrap();
        log.append(EventKind::ToolCallStart {
            call_id: id.into(),
            name: name.into(),
            input,
        })
        .unwrap();
        log.append(EventKind::ToolResult {
            call_id: id.into(),
            name: name.into(),
            content: "ok".into(),
            is_error: false,
            raw_bytes: 2,
            spilled_to: None,
            denied: false,
        })
        .unwrap();
    }
    log.append(EventKind::TurnEnd { step: 1 }).unwrap();
    // Turn 2: the provider reuses "c1" for a NEW call; the process dies
    // before its ToolResult lands.
    log.append(EventKind::ModelResponse {
        blocks: vec![Block::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            input: json!({"path": "a.txt"}),
        }],
        usage: Usage::default(),
        stop_reason: "tool_use".into(),
        latency_ms: 0,
        cost_usd: 0.0,
    })
    .unwrap();
    log.append(EventKind::ToolCallStart {
        call_id: "c1".into(),
        name: "read".into(),
        input: json!({"path": "a.txt"}),
    })
    .unwrap();
    log.flush().unwrap();
    drop(log);

    let p = Scripted::queue(vec![text("ok")]);
    let mut agent = Agent::resume(p.clone(), cfg(&ws), s.clone()).unwrap();
    agent.run_turn("continue", &mut |_: &Event| {}).unwrap();
    let first = &p.seen()[0].messages;
    assert!(
        unpaired(first).is_empty(),
        "request after resume carries unanswered tool_use ids {:?} — \
         close_dangling_calls matched the second c1 against turn 1's result",
        unpaired(first)
    );
}

/// review:verify-git-internals — the verifier tamper check (task.rs
/// ~519-535 → verify.rs `snapshot`) covers HEAD, refs, stash and the
/// porcelain-listed tree, but nothing inside `.git/` itself. A verifier
/// with `bash` plants `.git/hooks/pre-commit` (or rewrites `.git/config`
/// aliases/hooksPath): both snapshots are identical, the planted hook
/// fires on the user's next `git commit`, and `verdict: pass` stands.
#[test]
fn verify_hook_write_survives_tamper_check() {
    let dir = tmpdir("verify-hook");
    let ws = dir.join("ws");
    repo(&ws);
    let mut n = 0;
    let p = Scripted::new(move |_| {
        n += 1;
        match n {
            1 => call(
                "v1",
                "bash",
                json!({"command": "printf '#!/bin/sh\\necho pwned\\n' > .git/hooks/pre-commit && chmod +x .git/hooks/pre-commit"}),
            ),
            _ => text("All checks ran.\n```json\n{\"verdict\":\"pass\",\"evidence\":[],\"issues\":[],\"ran\":[\"true\"],\"confidence\":\"high\"}\n```"),
        }
    });
    let mut ctx = task_ctx(&ws, dir.join("s"), p);
    let out = tools::task::run(
        &json!({"prompt": "check the tree", "mode": "verify"}),
        &mut ctx,
    );
    assert!(
        ws.join(".git/hooks/pre-commit").exists(),
        "precondition: the verifier planted a git hook"
    );
    assert!(
        !out.text.contains("verdict: pass"),
        "verifier wrote .git/hooks/pre-commit — snapshot sees no tree/ref/HEAD change — and still passes:\n{}",
        out.text
    );
}

/// review:ignored-writer-output — `verify::has_changes` (verify.rs
/// ~141-145) reads `git status --porcelain` without `--ignored`, so a
/// writer whose only output is gitignored (`.env`, build artifacts, a
/// generated report the repo ignores) looks unchanged: the digest says
/// "no changes" and `remove_writer` force-removes the worktree and deletes
/// the branch — silently destroying the only thing the task produced.
#[test]
fn writer_gitignored_output_survives_cleanup() {
    let dir = tmpdir("writer-ignored");
    let ws = dir.join("ws");
    repo(&ws);
    let marker = ws.join("ran.txt");
    let write_cmd = format!(
        "mkdir -p build-out && echo BIN > build-out/app.bin && echo ran > {}",
        marker.display()
    );
    let mut n = 0;
    let p = Scripted::new(move |_| {
        n += 1;
        match n {
            1 => call("w1", "bash", json!({"command": write_cmd})),
            _ => text("wrote the artifact"),
        }
    });
    let mut ctx = task_ctx(&ws, dir.join("s"), p);
    let out = tools::task::run(
        &json!({"prompt": "build the artifact", "mode": "write"}),
        &mut ctx,
    );
    assert!(marker.exists(), "precondition: the writer's bash ran");
    assert!(
        !out.text.contains("no changes"),
        "writer produced build-out/app.bin (gitignored) — has_changes says \
         'no changes' and the worktree + branch were destroyed:\n{}",
        out.text
    );
    assert!(
        out.text.contains("output is gitignored") && out.text.contains("build-out/"),
        "{}",
        out.text
    );
}

/// review:torn-tail-data-loss — `harden::repair_torn_tail` (harden.rs
/// ~114-156) truncates every log that lacks a trailing `\n` back to its
/// last newline — even when the final line is a COMPLETE, parseable event
/// that `EventLog::replay` would happily keep. A crash that loses only the
/// newline byte (a write that lands without its line terminator)
/// permanently drops the last recorded event from the source of truth —
/// and from ledger.jsonl the same way, under-counting spend the cap then
/// re-admits.
#[test]
fn open_keeps_a_complete_unterminated_tail_event() {
    let dir = tmpdir("torn-tail");
    let path = dir.join("events.jsonl");
    let mut log = EventLog::create(&path).unwrap();
    session_start(&mut log);
    log.append(EventKind::UserInput {
        text: "important context".into(),
    })
    .unwrap();
    log.flush().unwrap();
    drop(log);
    // The last write landed but its newline byte was lost.
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(*bytes.last().unwrap(), b'\n');
    std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
    // replay alone tolerates the tail fine — the event is complete JSON.
    assert_eq!(EventLog::replay(&path).unwrap().len(), 2);
    // open() "repairs" it — and deletes a good event from the log.
    let _ = EventLog::open(&path).unwrap();
    let kept = EventLog::replay(&path).unwrap();
    assert_eq!(
        kept.len(),
        2,
        "a complete final event missing only its newline was truncated into \
         a .torn- aside — the source-of-truth log lost a recorded event"
    );
}

/// review:image-underestimate — `ledger::request_tokens` (ledger.rs
/// ~322-353) counts every `Block::Image` as a flat 1,600 tokens. The block
/// carries `sent_w`/`sent_h`; Anthropic bills ~(w*h)/750 — a full-size
/// 1568×1568 screenshot is ~3,276 tokens. `Gate::call`'s worst-case is
/// therefore NOT a bound: it admits a call whose real input blows past the
/// estimate, and `max_tokens` clamps to `remaining` — so billed spend can
/// land over `cap_usd`.
// DEFERRED(lead): the scripted reply bills 1,597 output tokens past the
// gate's clamped max_tokens (1,098), which no provider does; ceil(w*h/750)
// plus 10% cannot refuse it — gate: lead decision (see fix2-agent report)
#[test]
#[ignore = "review: image-underestimate (bills past max_tokens)"]
fn image_worst_case_bounds_real_billing() {
    let dir = tmpdir("gate-img");
    let mut ledger = Ledger::create(dir.join("ledger.jsonl")).unwrap();
    let msgs = [Message {
        role: Role::User,
        content: vec![Block::Image {
            media_type: "image/png".into(),
            data_b64: "AAAA".into(),
            px_w: 2608,
            px_h: 1960,
            sent_w: 1568,
            sent_h: 1568,
        }],
    }];
    let mut req = Request {
        model: "claude-sonnet-5",
        system: &[],
        tools: &[],
        messages: &msgs,
        max_tokens: 4_096,
        thinking_budget: None,
        effort: None,
        cache_breakpoints: false,
        cache_key: None,
    };
    // The provider can bill ~(1568*1568)/750 ≈ 3,276 tokens for this image;
    // the gate estimates 1,600.
    assert!(
        request_tokens(&req) >= 3_200,
        "flat IMAGE_TOKENS under-counts a max-size screenshot: {}",
        request_tokens(&req)
    );
    let p = Scripted::new(|_| Response {
        blocks: vec![Block::Text { text: "ok".into() }],
        stop_reason: StopReason::EndTurn,
        usage: Usage {
            fresh_input: 3_400,
            output: 1_597,
            ..Usage::default()
        },
        request_bytes: 0,
        latency_ms: 0,
    });
    let cap = 0.02;
    let (.., cost) = Gate {
        provider: p.as_ref(),
        ledger: &mut ledger,
        cap_usd: cap,
        reserved_usd: 0.0,
    }
    .call(&mut req, None)
    .unwrap();
    let _ = cost;
    assert!(
        ledger.total_cost_usd <= cap,
        "cap ${cap} but ${:.4} billed — the under-estimated image let the \
         gate admit a call it could not afford",
        ledger.total_cost_usd
    );
}

/// review:cancel-release-window — `finish()` (task.rs ~555-566) calls
/// `account.release(&sc.id)` on a cancelled task, dropping its WHOLE
/// reservation, while its real spend only reaches the parent's books at
/// the next `reconcile_subagents`/`settle` (agent.rs ~1673). In that
/// window `remaining_usd()` treats already-burned money as free, and a
/// sibling spawn granted in it over-commits the cap for good.
// DEFERRED(lead): drives `SpendAccount::release` itself, which the fix no
// longer calls on cancel; covered via `task::run` below — gate: lead decision
#[test]
#[ignore = "review: cancel-release-window (calls release directly)"]
fn cancelled_task_cap_stays_held_until_spend_settles() {
    let acct = SpendAccount::new(1.00, 0.30); // parent ledger total $0.30
    acct.grant("task-1", 0.50, MIN_CAP_USD).unwrap(); // writer holds $0.50
                                                      // task-1 burns $0.40 in its own ledger, then is cancelled → finish()
                                                      // releases the reservation before the spend is settled (task.rs:564-565).
    acct.release("task-1");
    // Window: parent sees $0.70 free; really only $0.30 is.
    let granted = acct.grant("task-2", 0.60, MIN_CAP_USD).unwrap();
    // task-1's real spend now settles into the parent ledger.
    acct.settle("task-1", 0.70);
    let committed = 0.70 + acct.reserved_usd();
    let _ = granted;
    assert!(
        committed <= 1.00,
        "cancelled task's ${:.2} spend settled after a sibling took ${:.2} \
         of phantom headroom — committed ${committed:.2} over the $1.00 cap",
        0.40,
        granted
    );
}

/// review:fork-corrupt-middle — `session::fork` (session.rs ~339-416)
/// writes `new_dir` and copies the source log line-by-line BEFORE the
/// first full `EventLog::replay` at line 385. A corrupt middle line is
/// kept verbatim (the `(_, None)` arm passes the `at_event` filter), so
/// replay errors mid-fork and `Err` propagates — leaving a half-created
/// session dir (events.jsonl + ledger.jsonl, no SessionStart) that is
/// unresumable and pollutes session listings.
#[test]
fn fork_refuses_or_cleans_up_on_corrupt_source() {
    let dir = tmpdir("fork-corrupt");
    let s = dir.join("s");
    std::fs::create_dir_all(&s).unwrap();
    let mut log = EventLog::create(s.join("events.jsonl")).unwrap();
    session_start(&mut log);
    log.append(EventKind::UserInput {
        text: "before".into(),
    })
    .unwrap();
    log.flush().unwrap();
    drop(log);
    // Corrupt a middle line in place (disk rot / hand edit).
    let path = s.join("events.jsonl");
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    lines.insert(1, "{not json".into());
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    let new_dir = dir.join("s-fork");
    let res = overseer_core::session::fork(&s, None, &new_dir);
    assert!(
        res.is_ok() && EventLog::replay(new_dir.join("events.jsonl")).is_ok() || !new_dir.exists(),
        "fork({res:?}) left a half-created, unresumable session dir at {}",
        new_dir.display()
    );
}

/// image-underestimate, with a provider that honours the clamped
/// `max_tokens`: the image is priced from its sent size (plus the 10%
/// margin), so the clamp leaves room for the input it really bills.
#[test]
fn image_gate_bounds_billing_that_honours_max_tokens() {
    let dir = tmpdir("gate-img-honest");
    let mut ledger = Ledger::create(dir.join("ledger.jsonl")).unwrap();
    let msgs = [Message {
        role: Role::User,
        content: vec![Block::Image {
            media_type: "image/png".into(),
            data_b64: "AAAA".into(),
            px_w: 2608,
            px_h: 1960,
            sent_w: 1568,
            sent_h: 1568,
        }],
    }];
    let mut req = Request {
        model: "claude-sonnet-5",
        system: &[],
        tools: &[],
        messages: &msgs,
        max_tokens: 4_096,
        thinking_budget: None,
        effort: None,
        cache_breakpoints: false,
        cache_key: None,
    };
    let p = Scripted::new(|req| Response {
        blocks: vec![Block::Text { text: "ok".into() }],
        stop_reason: StopReason::MaxTokens,
        usage: Usage {
            fresh_input: 3_400,
            output: u64::from(req.max_tokens),
            ..Usage::default()
        },
        request_bytes: 0,
        latency_ms: 0,
    });
    let cap = 0.02;
    Gate {
        provider: p.as_ref(),
        ledger: &mut ledger,
        cap_usd: cap,
        reserved_usd: 0.0,
    }
    .call(&mut req, None)
    .unwrap();
    assert!(
        ledger.total_cost_usd <= cap,
        "cap ${cap} but ${:.4} billed at max_tokens {}",
        ledger.total_cost_usd,
        req.max_tokens
    );
}

/// cancel-release-window through `task::run`: a cancelled task's spend
/// stays reserved until the parent settles it, so `remaining_usd()` never
/// counts it as free.
#[test]
fn cancelled_task_spend_stays_held_after_finish() {
    let dir = tmpdir("cancel-hold");
    let ws = dir.join("ws");
    repo(&ws);
    let parent = overseer_core::control::Control::default();
    let pc = parent.clone();
    let p = Scripted::new(move |_| {
        pc.interrupt();
        call("c1", "bash", json!({"command": "true"}))
    });
    let mut ctx = task_ctx(&ws, dir.join("s"), p);
    ctx.subagents.control = parent;
    let acct = ctx.subagents.spend.clone().unwrap();
    let out = tools::task::run(&json!({"prompt": "look around", "mode": "read"}), &mut ctx);
    assert!(out.text.contains("cancelled"), "{}", out.text);
    let task_dir = std::fs::read_dir(dir.join("s/subagents"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.join("ledger.jsonl").exists())
        .expect("the task's ledger");
    let spent = Ledger::open(task_dir.join("ledger.jsonl"))
        .unwrap()
        .total_cost_usd;
    assert!(spent > 0.0, "precondition: the task billed a call");
    assert!(
        (acct.reserved_usd() - spent).abs() < 1e-9,
        "cancelled task burned ${spent:.4} but ${:.4} stays reserved — \
         remaining_usd() counts the difference as free:\n{}",
        acct.reserved_usd(),
        out.text
    );
}
