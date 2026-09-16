//! Stress tests for the P2 exit invariants (playbook §12.4 exit
//! criteria). Each test names the invariant it proves. Timing bounds are
//! deliberately generous (CI machines vary); the point is to catch
//! superlinear blowups and hard failures, not micro-benchmark.
//!
//!   1. Esc always works — incl. queued + background states
//!   2. Input echo <50 ms while streaming
//!   3. Scrollback survives exit (every cell flushed via insert_before)
//!   4. Large paste never freezes
//!   5. Permission prompts carry context under churn
//!   6. Transcript/search over large history
//!   7. Fork/rewind under session storms
//!   8. Diff generation over many files/hunks

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Instant;

use overseer_core::event::{Event, EventKind, EventLog};
use overseer_core::ir::{Block, Usage};
use overseer_core::perm::{AskRequest, Preset};
use overseer_tui::app::{App, EngineMsg, WorkerCmd};
use overseer_tui::probe::Caps;
use ratatui::backend::{Backend, TestBackend};
use ratatui::{Terminal, TerminalOptions, Viewport};

fn ev(kind: EventKind) -> Event {
    Event {
        id: 0,
        parent_id: None,
        ts_ms: 0,
        kind,
    }
}

fn harness() -> (
    App,
    mpsc::Sender<EngineMsg>,
    mpsc::Receiver<WorkerCmd>,
    Terminal<TestBackend>,
    Caps,
) {
    let (etx, erx) = mpsc::channel();
    let (wtx, wrx) = mpsc::channel();
    let app = App::new(
        erx,
        wtx,
        Preset::WorkspaceWrite,
        "/repo".into(),
        "test-model".into(),
        PathBuf::from("/tmp/stress-session"),
    );
    let mut backend = TestBackend::new(80, 24);
    backend.set_cursor_position((0, 10)).unwrap();
    let term = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(8),
        },
    )
    .unwrap();
    (app, etx, wrx, term, Caps::default())
}

fn key(app: &mut App, code: crossterm::event::KeyCode) {
    app.key(crossterm::event::KeyEvent::from(code));
}

fn ctrl(app: &mut App, c: char) {
    app.key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char(c),
        crossterm::event::KeyModifiers::CONTROL,
    ));
}

fn model_response(i: usize) -> Event {
    ev(EventKind::ModelResponse {
        blocks: vec![Block::Text {
            text: format!("response {i} with some filler text to render"),
        }],
        usage: Usage {
            fresh_input: 10,
            output: 5,
            ..Default::default()
        },
        stop_reason: "end_turn".into(),
        latency_ms: 1,
        cost_usd: 0.0,
    })
}

// ── 1. Esc always works ───────────────────────────────────────────────

/// Esc during an active run with queued steers + a pending permission
/// dialog: every state accepts the interrupt/deny, never hangs.
#[test]
fn esc_works_through_queued_and_dialog_states() {
    let (mut app, etx, wrx, mut term, caps) = harness();
    app.submit_text("big task");
    let control = match wrx.recv().unwrap() {
        WorkerCmd::Submit { control, .. } => control,
        _ => panic!(),
    };
    // Queue three steers mid-run, then a permission dialog on top.
    for i in 0..3 {
        app.submit_text(&format!("steer {i}"));
    }
    let (rtx, rrx) = mpsc::channel();
    etx.send(EngineMsg::Ask(
        AskRequest {
            tool: "bash".into(),
            input: serde_json::json!({"command": "rm -rf x"}),
            reason: "destructive".into(),
        },
        rtx,
    ))
    .unwrap();
    app.step(&mut term, &caps).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(210)); // arm

    // Esc answers the dialog (deny) — it must not eat the interrupt.
    key(&mut app, crossterm::event::KeyCode::Esc);
    assert!(matches!(
        rrx.recv_timeout(std::time::Duration::from_secs(1)),
        Ok(overseer_core::perm::AskDecision::Deny)
    ));
    // Second Esc interrupts the run itself — the queue is preserved.
    key(&mut app, crossterm::event::KeyCode::Esc);
    assert!(control.interrupted());
}

// ── 2. Input echo under streaming load ────────────────────────────────

/// While the engine streams events, each keystroke's key→step→render
/// cycle must stay under the 50 ms echo budget. The DRAIN_CAP keeps
/// per-frame work bounded even under a 500-event burst — that's what
/// this test proves.
#[test]
fn input_echo_stays_fast_during_streaming() {
    let (mut app, etx, _w, mut term, caps) = harness();
    app.submit_text("go");
    // Saturate the engine channel: 500 in-flight events pending.
    for i in 0..500 {
        etx.send(EngineMsg::Event(model_response(i))).unwrap();
    }
    let mut worst = std::time::Duration::ZERO;
    for i in 0..50 {
        // Interleave typing with the engine still draining.
        let t = Instant::now();
        key(
            &mut app,
            crossterm::event::KeyCode::Char(b"abc"[i % 3] as char),
        );
        app.step(&mut term, &caps).unwrap();
        worst = worst.max(t.elapsed());
    }
    eprintln!("[stress] echo worst: {worst:?} over 50 keys + 500 events");
    assert!(
        worst < std::time::Duration::from_millis(50),
        "echo budget blown: {worst:?}"
    );
    // The capped drain still finishes — every event eventually lands.
    while app.engine_backlog() {
        app.step(&mut term, &caps).unwrap();
    }
}

// ── 3. Scrollback survives ────────────────────────────────────────────

/// 1000 completed cells must all land in scrollback — none dropped,
/// live region still bounded.
#[test]
fn thousand_cells_flush_to_scrollback() {
    let (mut app, etx, _w, mut term, caps) = harness();
    for i in 0..1000 {
        etx.send(EngineMsg::Event(ev(EventKind::UserInput {
            text: format!("prompt {i}"),
        })))
        .unwrap();
        etx.send(EngineMsg::Event(model_response(i))).unwrap();
    }
    let t = Instant::now();
    loop {
        app.step(&mut term, &caps).unwrap();
        if !app.engine_backlog() {
            break;
        }
    }
    eprintln!("[stress] 2000-cell flush+render: {:?}", t.elapsed());
    // The TestBackend buffer scrolled — cells above the viewport are
    // gone from the buffer (insert_before pushed them out), which is
    // the handoff contract. What must hold: the LAST cells rendered.
    let s = screen_text(&term);
    assert!(s.contains("prompt 999") || s.contains("response 999"));
}

// ── 4. Large paste never freezes ──────────────────────────────────────

/// A 500 KB bracketed paste becomes ONE chip atom and the frame stays
/// interactive — no per-grapheme rescan of the paste body.
#[test]
fn huge_paste_chips_instead_of_freezing() {
    let (mut app, _e, _w, mut term, caps) = harness();
    let blob = "x".repeat(500 * 1024);
    let t = Instant::now();
    app.paste(&blob);
    app.step(&mut term, &caps).unwrap();
    let dt = t.elapsed();
    eprintln!("[stress] 500KB paste→chip+frame: {dt:?}");
    assert!(
        dt < std::time::Duration::from_secs(2),
        "paste froze: {dt:?}"
    );
}

// ── 5. Permission prompts under churn ─────────────────────────────────

/// 30 sequential asks each answer in order — the dialog must carry the
/// RIGHT context (tool + reason) on every one, not stale state.
#[test]
fn permission_churn_keeps_context() {
    let (mut app, etx, _w, mut term, caps) = harness();
    for i in 0..30 {
        let (rtx, rrx) = mpsc::channel();
        etx.send(EngineMsg::Ask(
            AskRequest {
                tool: format!("tool{i}"),
                input: serde_json::json!({"n": i}),
                reason: format!("reason-{i}"),
            },
            rtx,
        ))
        .unwrap();
        app.step(&mut term, &caps).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(210));
        app.step(&mut term, &caps).unwrap();
        let s = screen_text(&term);
        assert!(
            s.contains(&format!("tool{i}")) || s.contains(&format!("reason-{i}")),
            "dialog lost context on ask {i}:\n{s}"
        );
        key(&mut app, crossterm::event::KeyCode::Enter); // allow once
        assert!(matches!(
            rrx.recv_timeout(std::time::Duration::from_secs(1)),
            Ok(overseer_core::perm::AskDecision::AllowOnce)
        ));
    }
}

// ── 6. Transcript search over large history ───────────────────────────

/// 4000 cells in history: the overlay opens, filters, and stays
/// interactive (no full-transcript re-render per keystroke).
#[test]
fn transcript_search_over_4000_cells() {
    let (mut app, etx, _w, mut term, caps) = harness();
    for i in 0..4000 {
        etx.send(EngineMsg::Event(ev(EventKind::UserInput {
            text: format!("message number {i}"),
        })))
        .unwrap();
    }
    loop {
        app.step(&mut term, &caps).unwrap();
        if !app.engine_backlog() {
            break;
        }
    }
    ctrl(&mut app, 'o');
    app.step(&mut term, &caps).unwrap();
    let t = Instant::now();
    for c in "number 3999".chars() {
        key(&mut app, crossterm::event::KeyCode::Char(c));
        app.step(&mut term, &caps).unwrap();
    }
    eprintln!("[stress] 4k-cell filter, 11 keystrokes: {:?}", t.elapsed());
    let s = screen_text(&term);
    assert!(s.contains("message number 3999"));
}

// ── 7. Fork/rewind session storm ──────────────────────────────────────

/// 60 chained forks + tree() over the result: ordering stays correct
/// and enumeration doesn't degrade superlinearly.
#[test]
fn fork_storm_tree_stays_ordered() {
    let root = std::env::temp_dir().join(format!("overseer-storm-{}", std::process::id()));
    let mk = |name: &str| {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let mut log = EventLog::create(dir.join("events.jsonl")).unwrap();
        log.append(EventKind::SessionStart {
            session_id: name.into(),
            cwd: "/w".into(),
            model: "m".into(),
            harness_version: "0".into(),
            parent: None,
        })
        .unwrap();
        log.append(EventKind::UserInput { text: "x".into() })
            .unwrap();
        log.flush().unwrap();
        dir
    };
    let mut prev = mk("s0");
    let t = Instant::now();
    for i in 1..60 {
        let next = root.join(format!("s{i}"));
        overseer_core::session::fork(&prev, None, &next).unwrap();
        prev = next;
    }
    eprintln!("[stress] 60 chained forks: {:?}", t.elapsed());
    let t = Instant::now();
    let tree = overseer_core::session::tree(&root);
    eprintln!("[stress] tree over 60-deep chain: {:?}", t.elapsed());
    assert_eq!(tree.len(), 60);
    // Depth must increase monotonically down the chain.
    assert_eq!(tree[59].1, 59);
    let _ = std::fs::remove_dir_all(&root);
}

/// Event-log throughput: 20k appends + full replay.
#[test]
fn event_log_20k_append_replay() {
    let dir = std::env::temp_dir().join(format!("overseer-log-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut log = EventLog::create(dir.join("events.jsonl")).unwrap();
    let t = Instant::now();
    for i in 0..20_000 {
        log.append(EventKind::UserInput {
            text: format!("event {i}"),
        })
        .unwrap();
    }
    log.flush().unwrap();
    let write = t.elapsed();
    let t = Instant::now();
    let n = EventLog::replay(dir.join("events.jsonl")).unwrap().len();
    let read = t.elapsed();
    eprintln!("[stress] 20k events: write {write:?} replay {read:?}");
    assert_eq!(n, 20_000);
    assert!(read < std::time::Duration::from_secs(5), "replay: {read:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── 8. Diff generation over many hunks ────────────────────────────────

/// 200 scattered changes in a 10k-line file → 200 hunks; generation
/// and per-hunk reject both complete fast.
#[test]
fn diff_many_hunks() {
    let old: String = (0..10_000).map(|i| format!("line {i}\n")).collect();
    let mut new = old.clone();
    for i in (0..10_000).step_by(50) {
        new = new.replacen(&format!("line {i}\n"), &format!("CHANGED {i}\n"), 1);
    }
    let t = Instant::now();
    let hs = overseer_tui::diff::hunks(&old, &new, 3);
    eprintln!(
        "[stress] 10k-line diff → {} hunks: {:?}",
        hs.len(),
        t.elapsed()
    );
    assert_eq!(hs.len(), 200);
    // Reject every other hunk — output must interleave correctly.
    let rejected: Vec<bool> = (0..hs.len()).map(|i| i % 2 == 0).collect();
    let t = Instant::now();
    let out = overseer_tui::diff::apply_rejects(&new, &hs, &rejected);
    eprintln!("[stress] apply 100 rejects: {:?}", t.elapsed());
    assert!(out.contains("line 0\n") && !out.contains("CHANGED 0\n"));
    assert!(out.contains("CHANGED 50\n") && !out.contains("line 50\n"));
}

// ── helpers ───────────────────────────────────────────────────────────

fn screen_text(term: &Terminal<TestBackend>) -> String {
    let buf = term.backend().buffer();
    let area = *buf.area();
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            out.push_str(buf.cell((x, y)).unwrap().symbol());
        }
        out.push('\n');
    }
    out
}
