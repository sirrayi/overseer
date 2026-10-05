//! Cross-feature integration: memory v2 × tool economy × subagent tiers.
//! Each test drives a real `Agent` (production registry, gate and task
//! paths) against a scripted provider with one response queue per lane —
//! a lane is picked by a marker in the request's first user message, so
//! background subagents stay deterministic.

use overseer_core::agent::{Agent, AgentConfig};
#[cfg(feature = "code-mode")]
use overseer_core::event::rehydrate_messages;
use overseer_core::event::{Event, EventKind, EventLog};
use overseer_core::ir::{Block, Usage};
#[cfg(feature = "code-mode")]
use overseer_core::ledger::Ledger;
use overseer_core::provider::{Provider, ProviderError, Request, Response, StopReason};
use overseer_core::tools::memory_tool::SUBAGENT_DENY;
#[cfg(feature = "code-mode")]
use overseer_core::tools::task::done_marker;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};

const PARENT: &str = "";

/// Every test runs this first: `computer` needs a configured backend to be
/// reachable at all (availability is checked before the gate), and the env
/// must be set before any registry in this binary detects it.
fn setup() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let driver =
            std::env::temp_dir().join(format!("overseer-xfeat-driver-{}", uuid::Uuid::now_v7()));
        std::fs::write(&driver, "#!/bin/sh\nexit 1\n").unwrap();
        std::env::set_var("OVERSEER_COMPUTER_DRIVER", &driver);
    });
}

struct Lanes {
    queues: Mutex<HashMap<&'static str, VecDeque<Response>>>,
    calls: Mutex<HashMap<&'static str, usize>>,
}

impl Lanes {
    fn new(lanes: Vec<(&'static str, Vec<Response>)>) -> Arc<Self> {
        Arc::new(Lanes {
            queues: Mutex::new(
                lanes
                    .into_iter()
                    .map(|(k, v)| (k, VecDeque::from(v)))
                    .collect(),
            ),
            calls: Mutex::new(HashMap::new()),
        })
    }

    fn calls(&self, lane: &'static str) -> usize {
        self.calls.lock().unwrap().get(lane).copied().unwrap_or(0)
    }
}

impl Provider for Lanes {
    fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let first = req.messages.first().map(|m| m.text()).unwrap_or_default();
        let mut queues = self.queues.lock().unwrap();
        let lane = queues
            .keys()
            .copied()
            .filter(|k| !k.is_empty())
            .find(|k| first.contains(k))
            .unwrap_or(PARENT);
        *self.calls.lock().unwrap().entry(lane).or_default() += 1;
        let q = queues.get_mut(lane).expect("lane");
        Ok(if q.len() > 1 {
            q.pop_front().unwrap()
        } else {
            q.front().expect("scripted response").clone()
        })
    }
    fn name(&self) -> &'static str {
        "lanes"
    }
}

fn usage(fresh_input: u64) -> Usage {
    Usage {
        fresh_input,
        ..Usage::default()
    }
}

fn tool_calls(calls: Vec<(&str, &str, Value)>, fresh_input: u64) -> Response {
    Response {
        blocks: calls
            .into_iter()
            .map(|(id, name, input)| Block::ToolCall {
                id: id.into(),
                name: name.into(),
                input,
            })
            .collect(),
        stop_reason: StopReason::ToolUse,
        usage: usage(fresh_input),
        request_bytes: 0,
        latency_ms: 0,
    }
}

fn text(t: &str, fresh_input: u64) -> Response {
    Response {
        blocks: vec![Block::Text { text: t.into() }],
        stop_reason: StopReason::EndTurn,
        usage: usage(fresh_input),
        request_bytes: 0,
        latency_ms: 0,
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("overseer-xfeat-{tag}-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn git(dir: &Path, args: &[&str]) {
    let st = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .output()
        .unwrap()
        .status;
    assert!(st.success(), "git {args:?}");
}

/// A committed repo as the workspace (writers need a worktree base).
fn repo(ws: &Path) {
    std::fs::create_dir_all(ws).unwrap();
    git(ws, &["init", "-q"]);
    std::fs::write(ws.join("a.txt"), "alpha\n").unwrap();
    git(ws, &["add", "."]);
    git(ws, &["commit", "-qm", "x"]);
}

/// Memory v2 stores outside the workspace, the session dir beside them.
fn v2_cfg(dir: &Path, full_access: bool) -> AgentConfig {
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (user, project) = (
        dir.join("home/memory"),
        dir.join("home/projects/ws-0/memory"),
    );
    overseer_core::memory::ensure(&user).unwrap();
    overseer_core::memory::ensure(&project).unwrap();
    AgentConfig {
        cwd: ws,
        full_access,
        model: "claude-fable-5".into(),
        memory_dir: Some(project),
        user_memory_dir: Some(user),
        ..AgentConfig::default()
    }
}

fn note(store: &Path, rel: &str, body: &str) {
    std::fs::write(store.join(rel), body).unwrap();
    let idx = store.join("INDEX.md");
    let mut index = std::fs::read_to_string(&idx).unwrap_or_default();
    if !index.is_empty() && !index.ends_with('\n') {
        index.push('\n');
    }
    index.push_str(&format!("{rel} — note\n"));
    std::fs::write(idx, index).unwrap();
}

fn results(events: &[Event]) -> HashMap<String, (String, bool, bool)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolResult {
                call_id,
                content,
                is_error,
                denied,
                ..
            } => Some((call_id.clone(), (content.clone(), *is_error, *denied))),
            _ => None,
        })
        .collect()
}

fn task_events(session: &Path, id: &str) -> Vec<Event> {
    EventLog::replay(session.join("subagents").join(id).join("events.jsonl")).unwrap()
}

#[cfg(feature = "code-mode")]
fn wait_for(p: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !p.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(p.exists(), "{} never appeared", p.display());
}

/// `memory` and `task` are excluded from scripts: each throws a clear
/// error naming the tool, and neither runs (no sub-call, no subagent).
#[cfg(feature = "code-mode")]
#[test]
fn run_code_cannot_call_memory_or_task() {
    setup();
    let dir = tmpdir("excl");
    let cfg = v2_cfg(&dir, false);
    let code = "const out = [];\n\
        for (const n of ['memory', 'task']) {\n\
          try { tools[n]({op: 'search', query: 'x', prompt: 'x'}); out.push(n + ' ran'); }\n\
          catch (e) { out.push(e.message); }\n\
        }\n\
        return out.join('\\n');";
    let provider = Lanes::new(vec![(
        PARENT,
        vec![
            tool_calls(vec![("c1", "run_code", json!({ "code": code }))], 0),
            text("done", 0),
        ],
    )]);
    let session = dir.join("s");
    let mut agent = Agent::start(provider.clone(), cfg, session.clone(), "s".into()).unwrap();
    agent.run_turn("go", &mut |_: &Event| {}).unwrap();
    let events = EventLog::replay(session.join("events.jsonl")).unwrap();
    let (content, _, denied) = &results(&events)["c1"];
    assert!(!denied, "{content}");
    for name in ["memory", "task"] {
        assert!(
            content.contains(&format!(
                "run_code: `{name}` cannot be called from a script — call it as its own tool."
            )),
            "{content}"
        );
        assert!(!content.contains(&format!("{name} ran")), "{content}");
    }
    assert!(!events
        .iter()
        .any(|e| matches!(e.kind, EventKind::ScriptCall { .. })));
    assert!(!session.join("subagents").exists());
    assert_eq!(provider.calls(PARENT), 2);
}

/// A read subagent searches memory (a filtered copy) but cannot write it —
/// under the gate (policy flag) and under `--full-access` (the tool's own
/// refusal) — and a resumed read subagent still cannot.
#[test]
fn read_subagent_searches_memory_but_never_remembers_even_resumed() {
    setup();
    for full_access in [false, true] {
        let dir = tmpdir("readmem");
        let cfg = v2_cfg(&dir, full_access);
        let project = cfg.memory_dir.clone().unwrap();
        note(
            &project,
            "semantic/deploy.md",
            "# Deploy\nstaging uses blue green\n",
        );
        let remember = |id: &str| {
            (
                id.to_string(),
                "memory",
                json!({"op": "remember", "layer": "semantic", "name": "leak", "text": "sub fact"}),
            )
        };
        let sub = vec![
            tool_calls(
                vec![(
                    "m1",
                    "memory",
                    json!({"op": "search", "query": "blue green"}),
                )],
                0,
            ),
            {
                let (id, n, i) = remember("m2");
                tool_calls(vec![(id.as_str(), n, i)], 0)
            },
            text("digest one", 0),
            {
                let (id, n, i) = remember("m3");
                tool_calls(vec![(id.as_str(), n, i)], 0)
            },
            text("digest two", 0),
        ];
        let parent = vec![
            tool_calls(
                vec![(
                    "t1",
                    "task",
                    json!({"prompt": "SUB-READ look up deploys", "mode": "read"}),
                )],
                0,
            ),
            tool_calls(
                vec![(
                    "t2",
                    "task",
                    json!({"prompt": "try again", "resume": "task-1"}),
                )],
                0,
            ),
            text("done", 0),
        ];
        let provider = Lanes::new(vec![(PARENT, parent), ("SUB-READ", sub)]);
        let session = dir.join("s");
        let mut agent = Agent::start(provider.clone(), cfg, session.clone(), "s".into()).unwrap();
        agent.run_turn("go", &mut |_: &Event| {}).unwrap();

        let parent_results = results(&EventLog::replay(session.join("events.jsonl")).unwrap());
        for t in ["t1", "t2"] {
            assert!(!parent_results[t].1, "{t}: {:?}", parent_results[t]);
        }
        let got = results(&task_events(&session, "task-1"));
        let (hit, err, _) = &got["m1"];
        assert!(!err && hit.contains("deploy"), "search: {hit}");
        for m in ["m2", "m3"] {
            let (msg, err, denied) = &got[m];
            assert!(*err && msg.contains(SUBAGENT_DENY), "{m}: {msg}");
            assert_eq!(*denied, !full_access, "{m}: gate vs tool refusal");
        }
        let semantic: Vec<_> = std::fs::read_dir(project.join("semantic"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(semantic, ["deploy.md"], "parent store untouched");
        assert_eq!(provider.calls("SUB-READ"), 5);
    }
}

/// `tools op=call computer` inside a write subagent meets the same gate as
/// a direct `computer` call: same verdict, same reason, nothing executed.
#[test]
fn tools_call_computer_in_a_write_subagent_is_gated_like_a_direct_call() {
    setup();
    let dir = tmpdir("computer");
    let cfg = v2_cfg(&dir, false);
    repo(&cfg.cwd);
    let nav = json!({"action": "navigate", "url": "https://example.com"});
    let provider = Lanes::new(vec![
        (
            PARENT,
            vec![
                tool_calls(
                    vec![(
                        "t1",
                        "task",
                        json!({"prompt": "SUB-WRITE open the page", "mode": "write"}),
                    )],
                    0,
                ),
                text("done", 0),
            ],
        ),
        (
            "SUB-WRITE",
            vec![
                tool_calls(
                    vec![
                        ("d1", "computer", nav.clone()),
                        (
                            "v1",
                            "tools",
                            json!({"op": "call", "name": "computer", "args": nav}),
                        ),
                    ],
                    0,
                ),
                text("digest", 0),
            ],
        ),
    ]);
    let session = dir.join("s");
    let mut agent = Agent::start(provider, cfg, session.clone(), "s".into()).unwrap();
    agent.run_turn("go", &mut |_: &Event| {}).unwrap();
    let events = task_events(&session, "task-1");
    let got = results(&events);
    let (direct, via) = (&got["d1"], &got["v1"]);
    assert!(direct.2, "direct call must be denied: {direct:?}");
    assert_eq!(direct, via, "tools op=call must gate like the direct call");
    assert!(!events
        .iter()
        .any(|e| matches!(e.kind, EventKind::ComputerAct { .. })));
}

/// A writer's `run_code` sub-calls cost no provider calls; its two model
/// calls are its whole spend, and that spend settles into the parent.
#[cfg(feature = "code-mode")]
#[test]
fn subagent_run_code_sub_calls_are_free_and_model_calls_settle_into_the_parent() {
    setup();
    let dir = tmpdir("settle");
    let cfg = v2_cfg(&dir, true);
    repo(&cfg.cwd);
    let code = "tools.read({path: 'a.txt'});\n\
        tools.glob({pattern: '*.txt'});\n\
        tools.grep({pattern: 'alpha'});\n\
        return 'ok';";
    let provider = Lanes::new(vec![
        (
            PARENT,
            vec![
                tool_calls(
                    vec![(
                        "t1",
                        "task",
                        json!({"prompt": "SUB-CODE survey", "mode": "write"}),
                    )],
                    0,
                ),
                text("done", 0),
            ],
        ),
        (
            "SUB-CODE",
            vec![
                tool_calls(vec![("c1", "run_code", json!({ "code": code }))], 40_000),
                text("digest", 60_000),
            ],
        ),
    ]);
    let session = dir.join("s");
    let mut agent = Agent::start(provider.clone(), cfg, session.clone(), "s".into()).unwrap();
    let mut run_end = None;
    agent
        .run_turn("go", &mut |e: &Event| {
            if let EventKind::RunEnd {
                subagent_cost_usd, ..
            } = e.kind
            {
                run_end = Some(subagent_cost_usd);
            }
        })
        .unwrap();

    assert_eq!(
        provider.calls("SUB-CODE"),
        2,
        "sub-calls are not model calls"
    );
    assert_eq!(provider.calls(PARENT), 2);
    let task_dir = session.join("subagents/task-1");
    let sub_events = task_events(&session, "task-1");
    let script_calls = sub_events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ScriptCall { .. }))
        .count();
    assert_eq!(script_calls, 3);
    let (out, err, _) = &results(&sub_events)["c1"];
    assert!(!err, "{out}");
    let sub_rows = Ledger::read_all(task_dir.join("ledger.jsonl"));
    assert_eq!(sub_rows.len(), 2, "one ledger row per model call");
    let spent: f64 = sub_rows.iter().map(|r| r.cost_usd).sum();
    assert!(spent > 0.0);
    let settled: f64 = Ledger::read_all(session.join("ledger.jsonl"))
        .iter()
        .filter(|r| r.subagent.as_deref() == Some("task-1"))
        .map(|r| r.cost_usd)
        .sum();
    assert!((settled - spent).abs() < 1e-9, "{settled} vs {spent}");
    let run_end = run_end.expect("RunEnd");
    assert!((run_end - spent).abs() < 1e-9, "{run_end} vs {spent}");
}

/// A session that recalls a note, fires a `path:` reminder, and also used
/// `tools`, `run_code` and a background task replays byte-identically.
#[cfg(feature = "code-mode")]
#[test]
fn memory_notices_replay_byte_identically_beside_tools_run_code_and_a_bg_task() {
    setup();
    let dir = tmpdir("replay");
    let cfg = v2_cfg(&dir, true);
    let project = cfg.memory_dir.clone().unwrap();
    std::fs::create_dir_all(cfg.cwd.join("docs")).unwrap();
    std::fs::write(cfg.cwd.join("docs/a.md"), "hello\n").unwrap();
    note(
        &project,
        "semantic/deploy.md",
        "# Deploy\nstaging deploy uses blue green\n",
    );
    note(
        &project,
        "prospective/docs.md",
        "---\ntrigger: path:docs/*.md\n---\nbump the docs version\n",
    );
    let provider = Lanes::new(vec![
        (
            PARENT,
            vec![
                tool_calls(
                    vec![
                        ("r1", "read", json!({"path": "docs/a.md"})),
                        ("s1", "tools", json!({"op": "search", "query": "symbol"})),
                        (
                            "c1",
                            "run_code",
                            json!({"code": "return tools.glob({pattern: 'docs/*.md'});"}),
                        ),
                        (
                            "t1",
                            "task",
                            json!({"prompt": "SUB-BG scan the docs", "background": true}),
                        ),
                    ],
                    0,
                ),
                text("turn one done", 0),
                text("turn two done", 0),
            ],
        ),
        ("SUB-BG", vec![text("bg digest", 0)]),
    ]);
    let session = dir.join("s");
    let mut agent =
        Agent::start(provider.clone(), cfg.clone(), session.clone(), "s".into()).unwrap();
    agent
        .run_turn("check the staging deploy", &mut |_: &Event| {})
        .unwrap();
    wait_for(&session.join("subagents/task-1").join(done_marker(1)));
    agent
        .run_turn("anything new?", &mut |_: &Event| {})
        .unwrap();

    let events = EventLog::replay(session.join("events.jsonl")).unwrap();
    let kinds: Vec<(String, Vec<String>)> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::MemoryNotice { kind, notes, .. } => Some((kind.clone(), notes.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        [
            (
                "recall".to_string(),
                vec!["project:semantic/deploy.md".to_string()]
            ),
            (
                "reminder".to_string(),
                vec!["project:prospective/docs.md".to_string()]
            ),
        ]
    );
    assert!(events
        .iter()
        .any(|e| matches!(e.kind, EventKind::ScriptCall { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e.kind, EventKind::SubagentDone { .. })));
    let live = serde_json::to_string(agent.messages()).unwrap();
    assert_eq!(
        live,
        serde_json::to_string(&rehydrate_messages(&events)).unwrap()
    );
    drop(agent);
    let resumed = Agent::resume(provider, cfg, session).unwrap();
    assert_eq!(live, serde_json::to_string(resumed.messages()).unwrap());
}
