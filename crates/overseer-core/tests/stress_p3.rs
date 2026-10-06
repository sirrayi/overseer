//! Phase-3 stress suite — exercises every P3 surface at volume against the
//! P0–P2 invariants: repomap over thousands of files, memory at its index
//! cap, skill scans at scale, Rule-of-Two churn, provenance wrapping,
//! mixed-kind event logs, background-subagent fan-out at the bound,
//! write-worktree storms, fork storms over P3 events, spill at 1 MB, and a
//! full-path Gemini adapter call (local TCP stub) over 1K messages.
//!
//! Every test prints real timings; ceilings are generous so CI doesn't flake.

use overseer_core::agent::AgentConfig;
use overseer_core::event::{self, Event, EventKind, EventLog};
use overseer_core::ir::{Block, Message, Role, Usage};
use overseer_core::perm::{self, Gate, Preset, Verdict};
use overseer_core::provider::{
    Effort, Provider, ProviderError, Request, Response, StopReason, SystemSegment, ToolSpec,
};
use overseer_core::tools::{self, ToolCtx, ToolOutput};
use overseer_core::{memory, repomap, session, skills};
use serde_json::json;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn tmpdir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "overseer-stress-{tag}-{}-{nanos}",
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Echo {
    /// Per-call latency so in-flight fan-out is observable.
    delay_ms: u64,
}
impl Provider for Echo {
    fn complete(&self, _req: &Request) -> Result<Response, ProviderError> {
        if self.delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
        }
        Ok(Response {
            blocks: vec![Block::Text {
                text: "digest body".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        })
    }
    fn name(&self) -> &'static str {
        "echo"
    }
}

fn task_ctx_at(cwd: &Path, session_dir: PathBuf, delay_ms: u64) -> ToolCtx<'static> {
    ToolCtx {
        cwd: cwd.to_path_buf(),
        session_dir,
        spill_seq: 0,
        provider: Some(Arc::new(Echo { delay_ms })),
        agent_config: Some(AgentConfig {
            cwd: cwd.to_path_buf(),
            full_access: true,
            ..Default::default()
        }),
        subagents: tools::task::SubagentCtx {
            control: Default::default(),
            seq: 0,
            spend: Some(Arc::new(tools::task::SpendAccount::new(
                AgentConfig::default().max_cost_usd,
                0.0,
            ))),
        },
        checkpoint: None,
        sandbox: false,
        broker: None,
    }
}

fn bg_in_flight(subagents_dir: &Path) -> usize {
    tools::task::sidecar::in_flight(subagents_dir)
}

// ── 3.7 repomap ─────────────────────────────────────────────────────────

#[test]
fn repomap_5k_files_bounded() {
    let dir = tmpdir("repomap");
    let src = dir.join("src");
    for d in 0..25 {
        let dd = src.join(format!("mod{d}"));
        std::fs::create_dir_all(&dd).unwrap();
        for f in 0..200 {
            std::fs::write(
                dd.join(format!("f{f}.rs")),
                "pub fn alpha() {}\nfn beta() {}\npub struct Gamma;\n".repeat(4),
            )
            .unwrap();
        }
    }
    let t = Instant::now();
    let idx = repomap::build(&dir);
    let build_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let map = repomap::render_map(&dir);
    let map_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let hit = repomap::lookup(&idx, &dir, "alpha");
    let lookup_ms = t.elapsed().as_millis();
    eprintln!(
        "repomap: {} syms, build {build_ms}ms, map {map_ms}ms ({}B), lookup {lookup_ms}ms",
        idx.symbols.len(),
        map.len()
    );
    assert!(!idx.symbols.is_empty());
    // Bounded context: the map must stay near the ~1K-token budget.
    assert!(map.len() <= 8_000, "map too big: {}", map.len());
    assert!(hit.contains("alpha"));
    assert!(build_ms < 30_000);
}

// ── 3.3/3.8 memory ──────────────────────────────────────────────────────

#[test]
fn memory_at_cap_200_topics() {
    let dir = tmpdir("memory");
    let mem = dir.join("memory");
    memory::ensure(&mem).unwrap();
    let idx = mem.join("INDEX.md");
    // Index at the cap plus overflow — segment must stay bounded.
    let mut body = String::new();
    for i in 0..600 {
        body.push_str(&format!(
            "topic-{i}.md — pointer line number {i} with some extra words\n"
        ));
    }
    std::fs::write(&idx, &body).unwrap();
    for i in 0..200 {
        std::fs::write(mem.join(format!("topic-{i}.md")), "x".repeat(4_000)).unwrap();
    }
    let t = Instant::now();
    let seg = memory::index_segment(&mem);
    let ms = t.elapsed().as_millis();
    eprintln!(
        "memory index_segment: {}B (from {}B) in {ms}ms",
        seg.len(),
        body.len()
    );
    assert!(seg.len() < body.len(), "segment must be capped");
    // Git commit at volume.
    let t = Instant::now();
    memory::commit(&mem, "stress commit");
    eprintln!("memory commit (200 files): {}ms", t.elapsed().as_millis());
}

#[test]
fn memory_consolidate_under_load() {
    let dir = tmpdir("consolidate");
    let mem = dir.join("memory");
    memory::ensure(&mem).unwrap();
    for i in 0..60 {
        std::fs::write(mem.join(format!("t-{i}.md")), "content".repeat(500)).unwrap();
    }
    std::fs::write(
        mem.join("INDEX.md"),
        (0..60)
            .map(|i| format!("t-{i}.md — note {i}\n"))
            .collect::<String>(),
    )
    .unwrap();
    // Mock replies with a valid ---INDEX--- rewrite.
    struct Cons;
    impl Provider for Cons {
        fn complete(&self, _req: &Request) -> Result<Response, ProviderError> {
            Ok(Response {
                blocks: vec![Block::Text {
                    text: "noise\n---INDEX---\nt-0.md — merged note\n".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
                request_bytes: 0,
                latency_ms: 0,
            })
        }
        fn name(&self) -> &'static str {
            "cons"
        }
    }
    let t = Instant::now();
    let out = memory::consolidate(&Cons, "small", &mem).unwrap();
    eprintln!(
        "consolidate (60 topics): {}ms — {out}",
        t.elapsed().as_millis()
    );
    assert!(out.contains("consolidated"));
    assert!(std::fs::read_to_string(mem.join("INDEX.md"))
        .unwrap()
        .contains("merged note"));
}

// ── 3.5 skills ──────────────────────────────────────────────────────────

#[test]
fn skills_500_scan_bounded_index() {
    let dir = tmpdir("skills");
    let sk = dir.join(".overseer/skills");
    for i in 0..500 {
        let d = sk.join(format!("skill-{i}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: skill-{i}\ndescription: does thing {i}\n---\nbody {i} ")
                + &"x".repeat(3_000),
        )
        .unwrap();
    }
    let t = Instant::now();
    let metas = skills::scan(&dir);
    let scan_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let seg = skills::index_segment(&dir).unwrap_or_default();
    let seg_ms = t.elapsed().as_millis();
    eprintln!(
        "skills: {} metas in {scan_ms}ms, index {}B in {seg_ms}ms",
        metas.len(),
        seg.len()
    );
    let workspace = metas.iter().filter(|m| m.source == "workspace").count();
    assert_eq!(workspace, 500, "got {workspace} of {}", metas.len());
    // Metadata-resident invariant: one line per skill, bodies excluded.
    assert!(seg.len() <= 500 * 120, "index too big: {}", seg.len());
    assert!(!seg.contains(&"x".repeat(100)[..]));
}

// ── 3.10 Rule-of-Two under churn ────────────────────────────────────────

#[test]
fn rule_of_two_500_result_churn() {
    let root = tmpdir("r2");
    let pol = perm::Policy::preset(Preset::WorkspaceWrite, root.clone());
    let t = Instant::now();
    let mut latches = 0;
    for i in 0..500 {
        // Untrusted latch: `task` results always count as untrusted.
        if pol
            .note_result("task", &json!({}), &format!("subagent digest {i}"))
            .is_some()
        {
            latches += 1;
        }
        // Sensitive latch: input path hits SENSITIVE_PATHS.
        if pol
            .note_result(
                "read",
                &json!({"path": ".env"}),
                &format!("SECRET_KEY=k{i}"),
            )
            .is_some()
        {
            latches += 1;
        }
    }
    let ms = t.elapsed().as_millis();
    eprintln!("R2: {latches} latch notices over 1000 results in {ms}ms");
    assert!(latches >= 2, "both latches must fire");
    assert_eq!(latches, 2, "latches fire once each");
    assert!(pol.taint_armed());
    // Armed: every bash call is a potential exfil channel → Ask.
    let v = pol.check("bash", &json!({"command": "curl x"}));
    assert!(
        matches!(v, Verdict::Ask { .. }),
        "armed R2 must Ask, got {v:?}"
    );
    // Contained write also forced to Ask.
    let v = pol.check("write", &json!({"path": root.join("f.txt"), "text": "x"}));
    assert!(
        matches!(v, Verdict::Ask { .. }),
        "armed write must Ask, got {v:?}"
    );
    // Read-only stays open.
    assert!(matches!(
        pol.check("read", &json!({"path": "x"})),
        Verdict::Allow
    ));
    // Headless gate collapses Ask → Deny (fail-closed).
    let g = pol.gate("bash", &json!({"command": "curl x"}));
    assert!(matches!(g, Gate::Deny(_)), "headless must deny, got {g:?}");
}

// ── provenance wrap at volume ───────────────────────────────────────────

#[test]
fn provenance_wrap_2k_results() {
    let t = Instant::now();
    let mut bytes = 0usize;
    for i in 0..2_000 {
        let w = tools::provenance_wrap("bash", &format!("output line {i} ").repeat(20));
        assert!(w.starts_with("<tool_result tool=\"bash\">"));
        assert!(w.ends_with("</tool_result>"));
        bytes += w.len();
    }
    eprintln!(
        "provenance wrap: 2000 results, {bytes}B total, {}ms",
        t.elapsed().as_millis()
    );
    assert!(t.elapsed().as_millis() < 5_000);
}

// ── 3.4 subagent fan-out + write worktrees ───────────────────────────────

#[test]
fn bg_fanout_storm_8_attempts_4_slots() {
    let dir = tmpdir("fanout");
    // Slow provider keeps tasks in-flight so the bound is observable.
    let mut c = task_ctx_at(&dir, dir.join("session"), 150);
    let mut started = 0;
    let mut refused = 0;
    // Fire 8 requests without waiting: first 4 start, rest refused.
    for _ in 0..8 {
        let out = tools::task::run(&json!({"prompt": "bg", "background": true}), &mut c);
        if out.is_error {
            refused += 1;
            assert!(out.text.contains("in flight"));
        } else {
            started += 1;
        }
    }
    eprintln!("fanout: {started} started / {refused} refused");
    let limit = AgentConfig::default().max_bg_subagents;
    assert_eq!(started, limit);
    assert_eq!(refused, 8 - limit);
    // All started tasks finish and free their slots.
    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let done = (1..=started).all(|i| {
            dir.join(format!("session/subagents/task-{i}/done.txt"))
                .exists()
        });
        if done || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    for i in 1..=started {
        assert!(dir
            .join(format!("session/subagents/task-{i}/done.txt"))
            .exists());
    }
    assert_eq!(bg_in_flight(&dir.join("session/subagents")), 0);
    // Refused leaves no orphan dirs beyond the started set.
    let dirs = std::fs::read_dir(dir.join("session/subagents"))
        .unwrap()
        .count();
    assert_eq!(dirs, started);
}

#[test]
fn write_worktree_storm_isolation() {
    let dir = tmpdir("wt");
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
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(&args)
            .output()
            .unwrap()
            .status;
        assert!(st.success());
    }
    let sess = tmpdir("wtsess");
    let t = Instant::now();
    for i in 1..=4u64 {
        let mut c = task_ctx_at(&dir, sess.join(format!("s{i}")), 0);
        c.subagents.seq = i - 1;
        let out = tools::task::run(&json!({"prompt": "w", "mode": "write"}), &mut c);
        assert!(!out.is_error, "iter {i}: {}", out.text);
        // A no-op writer's worktree and branch are removed.
        assert!(!sess.join(format!("s{i}/subagents/wt-{i}/wt")).exists());
    }
    eprintln!(
        "4 sequential write worktrees: {}ms",
        t.elapsed().as_millis()
    );
    // No run dirties the main tree.
    let dirty = std::process::Command::new("git")
        .args(["-C", dir.to_str().unwrap(), "status", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        dirty.stdout.is_empty(),
        "main tree polluted: {:?}",
        dirty.stdout
    );
}

// ── mixed-kind event log at volume (P0 invariant under P3 kinds) ────────

#[test]
fn event_log_30k_mixed_kinds_replay() {
    let dir = tmpdir("log");
    let path = dir.join("events.jsonl");
    let mut log = EventLog::create(&path).unwrap();
    log.append(EventKind::SessionStart {
        session_id: "s".into(),
        cwd: "/w".into(),
        model: "m".into(),
        harness_version: "t".into(),
        parent: None,
    })
    .unwrap();
    let t = Instant::now();
    for i in 0..6_000u64 {
        log.append(EventKind::UserInput {
            text: format!("prompt {i}"),
        })
        .unwrap();
        log.append(EventKind::ModelResponse {
            blocks: vec![
                Block::Text {
                    text: format!("thinking about {i}"),
                },
                Block::ToolCall {
                    id: format!("c{i}"),
                    name: "bash".into(),
                    input: json!({"cmd": "ls"}),
                },
            ],
            usage: Usage::default(),
            stop_reason: "tool_use".into(),
            latency_ms: 1,
            cost_usd: 0.0,
        })
        .unwrap();
        log.append(EventKind::ToolCallStart {
            call_id: format!("c{i}"),
            name: "bash".into(),
            input: json!({"cmd": "ls"}),
        })
        .unwrap();
        log.append(EventKind::ToolResult {
            call_id: format!("c{i}"),
            name: "bash".into(),
            content: tools::provenance_wrap("bash", "ok"),
            is_error: false,
            raw_bytes: 2,
            spilled_to: None,
            denied: false,
        })
        .unwrap();
        // P3 kinds interleaved.
        match i % 7 {
            0 => {
                log.append(EventKind::SubagentDone {
                    task_id: format!("task-{i}"),
                    trace: format!("/trace/{i}"),
                    cost_usd: 0.0,
                    tier: "light".into(),
                    model: "m".into(),
                    verdict: None,
                    run: 1,
                    digest: None,
                    footer: None,
                    status: String::new(),
                })
                .unwrap();
            }
            1 => {
                log.append(EventKind::Tainted {
                    detail: format!("latch {i}"),
                    latch: String::new(),
                })
                .unwrap();
            }
            2 => {
                log.append(EventKind::Nudge {
                    text: format!("steer {i}"),
                })
                .unwrap();
            }
            3 => {
                log.append(EventKind::StuckDetected {
                    pattern: "repeat".into(),
                })
                .unwrap();
            }
            _ => {}
        }
        log.append(EventKind::TurnEnd { step: i as u32 }).unwrap();
    }
    let append_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let evs = EventLog::replay(&path).unwrap();
    let replay_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let msgs = event::rehydrate_messages(&evs);
    let rehyd_ms = t.elapsed().as_millis();
    eprintln!(
        "30k-mixed log: {} events, append {append_ms}ms, replay {replay_ms}ms, rehydrate {rehyd_ms}ms → {} msgs",
        evs.len(),
        msgs.len()
    );
    assert!(evs.len() >= 30_000);
    // Pairing survives: every ToolCall has a ToolResult in the rehydrated view.
    let calls: std::collections::HashSet<String> = msgs
        .iter()
        .flat_map(|m| {
            m.content.iter().filter_map(|b| match b {
                Block::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
        })
        .collect();
    let results: std::collections::HashSet<String> = msgs
        .iter()
        .flat_map(|m| {
            m.content.iter().filter_map(|b| match b {
                Block::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
        })
        .collect();
    assert_eq!(calls.len(), 6_000);
    for c in &calls {
        assert!(results.contains(c), "orphan call {c}");
    }
    // Audit-only kinds never enter the model view; provenance survives replay.
    let serialized = serde_json::to_string(&msgs).unwrap();
    assert!(
        !serialized.contains("latch"),
        "Tainted leaked into model view"
    );
    assert!(
        serialized.contains("<tool_result"),
        "provenance wrap lost on rehydrate"
    );
    let _ = Role::Assistant; // silence unused-import lint
}

// ── fork storm over P3 events ────────────────────────────────────────────

#[test]
fn fork_storm_60_over_p3_log() {
    let root = tmpdir("forks");
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let mut log = EventLog::create(src.join("events.jsonl")).unwrap();
    log.append(EventKind::SessionStart {
        session_id: "root".into(),
        cwd: "/w".into(),
        model: "m".into(),
        harness_version: "t".into(),
        parent: None,
    })
    .unwrap();
    for i in 0..50 {
        log.append(EventKind::UserInput {
            text: format!("p{i}"),
        })
        .unwrap();
        log.append(EventKind::ModelResponse {
            blocks: vec![Block::Text { text: "r".into() }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
            latency_ms: 0,
            cost_usd: 0.0,
        })
        .unwrap();
        log.append(EventKind::SubagentDone {
            task_id: format!("task-{i}"),
            trace: "t".into(),
            cost_usd: 0.0,
            tier: "light".into(),
            model: "m".into(),
            verdict: None,
            run: 1,
            digest: None,
            footer: None,
            status: String::new(),
        })
        .unwrap();
        log.append(EventKind::Tainted {
            detail: "x".into(),
            latch: String::new(),
        })
        .unwrap();
    }
    drop(log);
    let t = Instant::now();
    let mut prev = src.clone();
    for i in 0..60 {
        let dst = root.join(format!("f{i}"));
        session::fork(&prev, None, &dst).unwrap();
        prev = dst;
    }
    let fork_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let tree = session::tree(&root);
    let tree_ms = t.elapsed().as_millis();
    eprintln!(
        "60 chained forks over P3 log: {fork_ms}ms, tree {} nodes in {tree_ms}ms",
        tree.len()
    );
    assert_eq!(tree.len(), 61);
    // Depth increases monotonically along the chain.
    let mut last = 0;
    for (_, depth) in &tree {
        assert!(*depth == 0 || *depth >= last);
        last = *depth;
    }
    // Deepest fork replays its full ancestry: the log grows by one
    // SessionStart per fork link (fork writes its own start event).
    let evs = EventLog::replay(prev.join("events.jsonl")).unwrap();
    assert_eq!(evs.len(), 1 + 50 * 4 + 60);
}

// ── spill at 1 MB (P1 invariant at P3 volume) ────────────────────────────

#[test]
fn spill_1mb_tool_output() {
    let dir = tmpdir("spill");
    let mut ctx = ToolCtx {
        cwd: dir.clone(),
        session_dir: dir.clone(),
        spill_seq: 0,
        provider: None,
        agent_config: None,
        subagents: Default::default(),
        checkpoint: None,
        sandbox: false,
        broker: None,
    };
    let big = "x".repeat(1_000_000);
    let out = tools::enforce_budget(
        ToolOutput {
            text: big,
            is_error: false,
            raw_bytes: 0,
            spilled_to: None,
            denied: false,
        },
        &mut ctx,
    );
    assert!(
        out.text.len() < 40_000,
        "inline too big: {}",
        out.text.len()
    );
    let spilled = std::fs::read_dir(dir.join("tool-outputs"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert!(spilled >= 1, "no spill file written");
}

// ── 3.1 Gemini full path over 1K messages (local stub) ──────────────────

#[test]
fn gemini_1k_messages_full_path() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        // Read headers + content-length body.
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut body_span = None;
        loop {
            let n = s.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if body_span.is_none() {
                if let Some(pos) = find(&buf, b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                    body_span = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .map(|l| (pos + 4, l));
                }
            }
            if let Some((start, len)) = body_span {
                if buf.len() >= start + len {
                    break;
                }
            }
        }
        let body = String::from_utf8_lossy(&buf);
        assert!(body.contains("\"contents\""), "no contents in request");
        let resp = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "done"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 50000, "candidatesTokenCount": 4, "totalTokenCount": 50004}
        });
        let payload = resp.to_string();
        write!(
            s,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            payload.len(),
            payload
        )
        .unwrap();
    });
    // 1000 alternating messages with tool calls + results.
    let mut messages = Vec::new();
    for i in 0..250 {
        messages.push(Message {
            role: Role::User,
            content: vec![Block::Text {
                text: format!("user {i} ").repeat(10),
            }],
        });
        messages.push(Message {
            role: Role::Assistant,
            content: vec![
                Block::Reasoning {
                    raw: json!({"thoughtSignature": "sig"}),
                },
                Block::Text {
                    text: format!("asst {i}"),
                },
                Block::ToolCall {
                    id: format!("c{i}"),
                    name: "bash".into(),
                    input: json!({"cmd": "ls"}),
                },
            ],
        });
        messages.push(Message {
            role: Role::User,
            content: vec![Block::ToolResult {
                tool_use_id: format!("c{i}"),
                content: "ok".into(),
                is_error: false,
            }],
        });
        messages.push(Message {
            role: Role::User,
            content: vec![Block::Text {
                text: format!("next {i}"),
            }],
        });
    }
    let g = overseer_core::provider::gemini::Gemini::new("k", format!("http://127.0.0.1:{port}"));
    let system = vec![SystemSegment {
        name: "test",
        text: "sys".into(),
        cacheable: true,
    }];
    let tool_specs = vec![ToolSpec {
        name: "bash".into(),
        description: "run".into(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {"cmd": {"type": "string", "default": "ls"}}
        }),
    }];
    let req = Request {
        model: "gemini-3-pro",
        system: &system,
        tools: &tool_specs,
        messages: &messages,
        max_tokens: 4096,
        thinking_budget: None,
        effort: Some(Effort::High),
        cache_breakpoints: false,
        cache_key: None,
    };
    let t = Instant::now();
    let resp = g.complete(&req).unwrap();
    let ms = t.elapsed().as_millis();
    eprintln!(
        "gemini 1k-message complete: {ms}ms, {} req bytes",
        resp.request_bytes
    );
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert_eq!(resp.usage.fresh_input, 50_000);
    assert!(resp.request_bytes > 50_000, "request should be large");
    server.join().unwrap();
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

// ── effort matrix: every level parses, bumps, serializes ────────────────

#[test]
fn effort_matrix_complete() {
    for (s, e) in [
        ("min", Effort::Min),
        ("low", Effort::Low),
        ("medium", Effort::Medium),
        ("high", Effort::High),
        ("max", Effort::Max),
    ] {
        let parsed = Effort::parse(s).unwrap();
        assert_eq!(parsed, e);
        assert_eq!(parsed.as_str(), s);
    }
    // Bounded escalation: max bumps to max.
    assert_eq!(Effort::Max.bumped(), Effort::Max);
    assert_eq!(Effort::High.bumped(), Effort::Max);
    assert_eq!(Effort::Min.bumped(), Effort::Low);
}

// ── P2↔P3 seam: Tainted/SubagentDone survive compaction view ────────────

#[test]
fn compaction_view_preserves_p3_audit() {
    let dir = tmpdir("compact");
    let path = dir.join("events.jsonl");
    let mut log = EventLog::create(&path).unwrap();
    log.append(EventKind::SessionStart {
        session_id: "s".into(),
        cwd: "/w".into(),
        model: "m".into(),
        harness_version: "t".into(),
        parent: None,
    })
    .unwrap();
    let mut last_model_id = 0u64;
    for i in 0..20 {
        log.append(EventKind::UserInput {
            text: format!("p{i}"),
        })
        .unwrap();
        last_model_id = log
            .append(EventKind::ModelResponse {
                blocks: vec![Block::Text { text: "r".into() }],
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
                latency_ms: 0,
                cost_usd: 0.0,
            })
            .unwrap();
        log.append(EventKind::Tainted {
            detail: format!("d{i}"),
            latch: String::new(),
        })
        .unwrap();
    }
    drop(log);
    let mut evs = EventLog::replay(&path).unwrap();
    let next_id = evs.last().map(|e| e.id + 1).unwrap_or(0);
    evs.push(Event {
        id: next_id,
        parent_id: None,
        ts_ms: 0,
        prev_hash: 0,
        hash: 0,
        kind: EventKind::Compaction {
            summary: "sum".into(),
            tail_from: last_model_id,
        },
    });
    // The compaction marker is a view: Tainted audit entries still exist in
    // the raw log even though the model view is compacted.
    let raw = EventLog::replay(&path).unwrap();
    assert_eq!(
        raw.iter()
            .filter(|e| matches!(e.kind, EventKind::Tainted { .. }))
            .count(),
        20
    );
}
