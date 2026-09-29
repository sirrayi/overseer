//! Snapshot tests driving the real render pipeline: EngineMsg channel →
//! cells → `insert_before` scrollback + live region on `TestBackend`.

use std::path::PathBuf;
use std::sync::mpsc;

use overseer_core::event::{Event, EventKind};
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
        prev_hash: 0,
        hash: 0,
        kind,
    }
}

fn model_response(text: &str) -> Event {
    ev(EventKind::ModelResponse {
        blocks: vec![Block::Text {
            text: text.to_string(),
        }],
        usage: Usage {
            fresh_input: 100,
            output: 20,
            ..Default::default()
        },
        stop_reason: "end_turn".into(),
        latency_ms: 500,
        cost_usd: 0.001,
    })
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
        PathBuf::from("/tmp/session"),
    );
    let mut backend = TestBackend::new(60, 20);
    backend.set_cursor_position((0, 10)).unwrap();
    let term = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(6),
        },
    )
    .unwrap();
    (app, etx, wrx, term, Caps::default())
}

/// Visible screen text: backend buffer rows joined with newlines,
/// trailing whitespace trimmed per row.
fn screen(term: &Terminal<TestBackend>) -> String {
    let buf = term.backend().buffer();
    let area = *buf.area();
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        let mut row = String::new();
        for x in area.left()..area.right() {
            row.push_str(buf.cell((x, y)).unwrap().symbol());
        }
        out.push_str(row.trim_end());
        out.push('\n');
    }
    out
}

#[test]
fn transcript_flushes_to_scrollback_and_live_region_stays() {
    let (mut app, etx, _wrx, mut term, caps) = harness();

    etx.send(EngineMsg::Event(ev(EventKind::UserInput {
        text: "fix the flaky test".into(),
    })))
    .unwrap();
    etx.send(EngineMsg::Event(model_response(
        "I'll look at the test file first.\n\n```sh\nls tests/\n```",
    )))
    .unwrap();
    etx.send(EngineMsg::Event(ev(EventKind::ToolCallStart {
        call_id: "c1".into(),
        name: "bash".into(),
        input: serde_json::json!({"command": "cargo test flaky"}),
    })))
    .unwrap();
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("mid_run", screen(&term));

    etx.send(EngineMsg::Event(ev(EventKind::ToolResult {
        call_id: "c1".into(),
        name: "bash".into(),
        content: "test flaky::rerun ... FAILED".into(),
        is_error: false,
        raw_bytes: 28,
        spilled_to: None,
        denied: false,
    })))
    .unwrap();
    etx.send(EngineMsg::Event(ev(EventKind::RunEnd {
        stop_reason: "end_turn".into(),
        steps: 1,
        total_cost_usd: 0.001,
    })))
    .unwrap();
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("turn_done", screen(&term));
}

#[test]
fn permission_dialog_renders_and_denies() {
    let (mut app, etx, _wrx, mut term, caps) = harness();
    let (rtx, rrx) = mpsc::channel();
    etx.send(EngineMsg::Ask(
        AskRequest {
            tool: "bash".into(),
            input: serde_json::json!({"command": "git push origin main"}),
            reason: "outside allowlist".into(),
        },
        rtx,
    ))
    .unwrap();
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("permission_dialog", screen(&term));

    // Armed state after the 200 ms anti-misclick grace.
    std::thread::sleep(std::time::Duration::from_millis(210));
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("permission_dialog_armed", screen(&term));

    // Answer via the app's key path (Esc = deny).
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Esc,
    ));
    assert!(matches!(
        rrx.recv_timeout(std::time::Duration::from_secs(1)),
        Ok(overseer_core::perm::AskDecision::Deny)
    ));
}

#[test]
fn steering_mid_run_queues_at_boundary() {
    let (mut app, _e, wrx, mut term, caps) = harness();
    app.submit_text("do the refactor");
    let cmd = wrx.recv().unwrap();
    let control = match cmd {
        WorkerCmd::Submit { control, .. } => control,
        _ => panic!("expected submit"),
    };
    app.step(&mut term, &caps).unwrap();
    app.submit_text("also rename the helper");
    assert_eq!(control.queued(), vec!["also rename the helper".to_string()]);
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("queued_steer", screen(&term));
}

#[test]
fn composer_multiline_and_history() {
    let (mut app, _e, _w, mut term, caps) = harness();
    for c in "first line".chars() {
        app.key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char(c),
        ));
    }
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("composer_typed", screen(&term));
}

// ── P2 Batch B: sessions, picker, rewind, fork, switch ──────────────

use std::sync::atomic::{AtomicU64, Ordering};

fn temp_root() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "overseer-tui-snap-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A session dir under `root` with a SessionStart + one user prompt.
fn mk_session(root: &std::path::Path, name: &str, cwd: &str, prompt: &str) -> PathBuf {
    use overseer_core::event::EventLog;
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let mut log = EventLog::create(dir.join("events.jsonl")).unwrap();
    log.append(EventKind::SessionStart {
        session_id: name.into(),
        cwd: cwd.into(),
        model: "test-model".into(),
        harness_version: "0".into(),
        parent: None,
    })
    .unwrap();
    log.append(EventKind::UserInput {
        text: prompt.into(),
    })
    .unwrap();
    log.flush().unwrap();
    dir
}

/// Harness whose session_dir lives inside a real session root — the
/// picker enumerates siblings of `session_dir`.
fn session_harness() -> (
    App,
    mpsc::Sender<EngineMsg>,
    mpsc::Receiver<WorkerCmd>,
    Terminal<TestBackend>,
    Caps,
    PathBuf,
) {
    let root = temp_root();
    let s1 = mk_session(&root, "s1", "/repo", "fix the flaky test");
    std::thread::sleep(std::time::Duration::from_millis(5));
    let _s2 = mk_session(&root, "s2", "/other", "add a parser");
    let (etx, erx) = mpsc::channel();
    let (wtx, wrx) = mpsc::channel();
    let app = App::new(
        erx,
        wtx,
        Preset::WorkspaceWrite,
        "/repo".into(),
        "test-model".into(),
        s1,
    );
    let mut backend = TestBackend::new(60, 20);
    backend.set_cursor_position((0, 10)).unwrap();
    let term = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(6),
        },
    )
    .unwrap();
    (app, etx, wrx, term, Caps::default(), root)
}

#[test]
fn session_picker_filters_and_switches() {
    let (mut app, _e, wrx, mut term, caps, root) = session_harness();
    app.submit_text("/sessions");
    app.step(&mut term, &caps).unwrap();
    // Normalize the relative-time column — it races the wall clock.
    let picker = screen(&term)
        .replace("0s ·", "[ago] ·")
        .replace("1s ·", "[ago] ·");
    insta::assert_snapshot!("session_picker", picker);

    // Fuzzy filter: 'par' matches only s2's "add a parser".
    for c in "par".chars() {
        app.key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char(c),
        ));
    }
    app.step(&mut term, &caps).unwrap();
    let filtered = screen(&term)
        .replace("0s ·", "[ago] ·")
        .replace("1s ·", "[ago] ·");
    insta::assert_snapshot!("session_picker_filtered", filtered);

    // Enter switches: the worker gets the command.
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Enter,
    ));
    match wrx.recv_timeout(std::time::Duration::from_secs(1)) {
        Ok(WorkerCmd::SwitchSession { dir }) => {
            assert_eq!(dir, root.join("s2"));
        }
        Ok(_) => panic!("expected SwitchSession, got another command"),
        Err(e) => panic!("expected SwitchSession, got {e}"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn session_switch_reseeds_transcript() {
    let (mut app, etx, _w, mut term, caps, root) = session_harness();
    etx.send(EngineMsg::SessionSwitched {
        dir: root.join("s2"),
    })
    .unwrap();
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("session_switched", screen(&term));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn slash_commands_guarded_during_run() {
    let (mut app, _e, wrx, mut term, caps, root) = session_harness();
    app.submit_text("do the thing");
    let _submit = wrx.recv().unwrap(); // the run's Submit
    app.submit_text("/fork");
    app.submit_text("/sessions");
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("guarded_during_run", screen(&term));
    // No session commands leaked to the worker mid-run.
    assert!(wrx.try_recv().is_err());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rewind_picker_lists_checkpoints() {
    let (mut app, _e, wrx, mut term, caps, root) = session_harness();
    // A checkpoint on e2 (the user input in s1's log); the tracked file
    // lives inside the temp root so the restore is self-contained.
    let cp = root.join("s1/checkpoints/e2/files");
    std::fs::create_dir_all(&cp).unwrap();
    std::fs::write(cp.join("f0"), "snapshot").unwrap();
    std::fs::write(
        root.join("s1/checkpoints/e2/manifest.jsonl"),
        format!(
            "{{\"path\":\"{}\",\"stored\":\"f0\",\"existed\":true}}\n",
            root.join("x.txt").display()
        ),
    )
    .unwrap();
    app.submit_text("/rewind");
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("rewind_picker", screen(&term));

    // Enter performs the rewind: log truncated + worker told to rebuild.
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Enter,
    ));
    match wrx.recv_timeout(std::time::Duration::from_secs(1)) {
        Ok(WorkerCmd::SwitchSession { dir }) => assert_eq!(dir, root.join("s1")),
        Ok(_) => panic!("expected SwitchSession rebuild after rewind"),
        Err(e) => panic!("expected SwitchSession rebuild, got {e}"),
    }
    // The rewind report flushed to the transcript.
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("rewind_done", screen(&term));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn fork_creates_sibling_and_switches() {
    let (mut app, _e, wrx, mut term, caps, root) = session_harness();
    app.submit_text("/fork");
    match wrx.recv_timeout(std::time::Duration::from_secs(1)) {
        Ok(WorkerCmd::SwitchSession { dir }) => {
            assert!(dir.join("events.jsonl").exists(), "fork copied the log");
            assert_eq!(dir.parent().unwrap(), root.as_path());
        }
        Ok(_) => panic!("expected SwitchSession to the fork"),
        Err(e) => panic!("expected SwitchSession to the fork, got {e}"),
    }
    app.step(&mut term, &caps).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

// ── P2 Batch C: transcript search, /diff, typed previews ─────────────

#[test]
fn transcript_overlay_searches_history() {
    let (mut app, etx, _w, mut term, caps, root) = session_harness();
    for t in ["fix the flaky test", "add a parser"] {
        etx.send(EngineMsg::Event(ev(EventKind::UserInput {
            text: t.into(),
        })))
        .unwrap();
    }
    etx.send(EngineMsg::Event(model_response(
        "I'll look at the test file.",
    )))
    .unwrap();
    app.step(&mut term, &caps).unwrap();

    // Ctrl+O opens the pager over flushed history.
    app.key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('o'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("transcript_overlay", screen(&term));

    // Typing filters to matching cells only.
    for c in "parser".chars() {
        app.key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char(c),
        ));
    }
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("transcript_filtered", screen(&term));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn diff_overlay_previews_and_reverts() {
    let (mut app, _e, _w, mut term, caps, root) = session_harness();
    // The agent "changed" this file after checkpoint e2 snapshotted it.
    let target = root.join("app.txt");
    let cp = root.join("s1/checkpoints/e2/files");
    std::fs::create_dir_all(&cp).unwrap();
    std::fs::write(cp.join("f0"), "line one\nline two\n").unwrap();
    std::fs::write(
        root.join("s1/checkpoints/e2/manifest.jsonl"),
        format!(
            "{{\"path\":\"{}\",\"stored\":\"f0\",\"existed\":true}}\n",
            target.display()
        ),
    )
    .unwrap();
    std::fs::write(&target, "line one\nline CHANGED\nline three\n").unwrap();

    app.submit_text("/diff");
    app.step(&mut term, &caps).unwrap();
    // Normalize the temp dir name out of path-bearing rows/toasts (the
    // row shows a truncated tail — the dir name is the varying part).
    let dirname = root.file_name().unwrap().to_str().unwrap().to_string();
    let norm = move |t: &Terminal<TestBackend>| screen(t).replace(&dirname, "[root]");
    insta::assert_snapshot!("diff_overlay", norm(&term));

    // Tab shows the unified diff for the selected file.
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Tab,
    ));
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("diff_preview", norm(&term));

    // Enter reverts to the snapshot; the file content proves it.
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Enter,
    ));
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "line one\nline two\n"
    );
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("diff_reverted_toast", norm(&term));
    let _ = std::fs::remove_dir_all(&root);
}

/// `/` menu + Tab completion: `/s` narrows to sessions/search, Tab
/// completes to the longest common prefix, then a single candidate
/// completes fully.
#[test]
fn slash_menu_and_tab_complete() {
    let (mut app, _etx, _wrx, mut term, caps) = harness();
    for c in "/s".chars() {
        app.key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char(c),
        ));
    }
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("slash_menu", screen(&term));

    // Two candidates → Tab lands on the common prefix "/se".
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Tab,
    ));
    for c in "arch".chars() {
        app.key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char(c),
        ));
    }
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Tab,
    ));
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("slash_completed", screen(&term));
}

// ── P2 gap audit: /tree, fuzzy / menu, hunk review, tool expansion ──

/// `/tree` lists the fork forest parents-first with ↳ children.
#[test]
fn tree_overlay_shows_fork_hierarchy() {
    let (mut app, _e, _w, mut term, caps, root) = session_harness();
    // s2 forks from s1 → /tree must nest it under s1.
    overseer_core::session::fork(&root.join("s1"), None, &root.join("s3")).unwrap();
    app.submit_text("/tree");
    app.step(&mut term, &caps).unwrap();
    let norm = screen(&term)
        .replace("0s ·", "[ago] ·")
        .replace("1s ·", "[ago] ·");
    insta::assert_snapshot!("tree_overlay", norm);
    let _ = std::fs::remove_dir_all(&root);
}

/// Fuzzy matching still drives Tab completion — the visible menu
/// strip was removed in the UI pass. `/t` is a subsequence of
/// tree/transcript → Tab lands on the common `/tr`; `/tre` then
/// completes to `/tree`.
#[test]
fn slash_tab_completes_fuzzy() {
    let (mut app, _etx, _wrx, mut term, caps) = harness();
    for c in "/t".chars() {
        app.key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char(c),
        ));
    }
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Tab,
    ));
    for c in "e".chars() {
        app.key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char(c),
        ));
    }
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Tab,
    ));
    app.step(&mut term, &caps).unwrap();
    let s = screen(&term);
    assert!(s.contains("/tree"), "fuzzy Tab should complete /tree:\n{s}");
}

/// Transcript overlay: Tab expands completed tool blocks (their
/// captured output renders under the summary line).
#[test]
fn transcript_tab_expands_tool_output() {
    let (mut app, etx, _w, mut term, caps, root) = session_harness();
    etx.send(EngineMsg::Event(ev(EventKind::ToolCallStart {
        call_id: "c1".into(),
        name: "bash".into(),
        input: serde_json::json!({"command": "make test"}),
    })))
    .unwrap();
    etx.send(EngineMsg::Event(ev(EventKind::ToolResult {
        call_id: "c1".into(),
        name: "bash".into(),
        content: "42 tests passed\n0 failed".into(),
        is_error: false,
        raw_bytes: 25,
        spilled_to: None,
        denied: false,
    })))
    .unwrap();
    app.step(&mut term, &caps).unwrap();

    app.key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('o'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    app.step(&mut term, &caps).unwrap();
    // Collapsed: output is not visible.
    assert!(!screen(&term).contains("42 tests passed"));

    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Tab,
    ));
    app.step(&mut term, &caps).unwrap();
    let s = screen(&term);
    assert!(s.contains("42 tests passed"), "expanded tool output:\n{s}");
    insta::assert_snapshot!("transcript_expanded", s);
    let _ = std::fs::remove_dir_all(&root);
}

/// Per-hunk reject: mark the second of two hunks, Enter applies only
/// that revert — the first change survives.
#[test]
fn diff_rejects_single_hunk() {
    let (mut app, _e, _w, mut term, caps, root) = session_harness();
    let target = root.join("multi.txt");
    // Snapshot: 10 lines; current changes line 2 AND line 9 (far
    // enough apart to form two hunks at ctx=3... b/c gap > 2*3).
    let snap: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    let cur = snap
        .replace("line 2", "TWO changed")
        .replace("line 9", "NINE changed");
    let cp = root.join("s1/checkpoints/e2/files");
    std::fs::create_dir_all(&cp).unwrap();
    std::fs::write(cp.join("f0"), &snap).unwrap();
    std::fs::write(
        root.join("s1/checkpoints/e2/manifest.jsonl"),
        format!(
            "{{\"path\":\"{}\",\"stored\":\"f0\",\"existed\":true}}\n",
            target.display()
        ),
    )
    .unwrap();
    std::fs::write(&target, &cur).unwrap();

    app.submit_text("/diff");
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Tab,
    )); // preview
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Right,
    )); // hunk 2
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Char(' '),
    )); // reject it
    app.step(&mut term, &caps).unwrap();
    let dirname = root.file_name().unwrap().to_str().unwrap().to_string();
    insta::assert_snapshot!(
        "diff_hunk_marked",
        screen(&term).replace(&dirname, "[root]")
    );

    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Enter,
    ));
    let after = std::fs::read_to_string(&target).unwrap();
    assert!(
        after.contains("TWO changed"),
        "hunk 1 must survive:\n{after}"
    );
    assert!(
        after.contains("line 9"),
        "hunk 2 must be reverted:\n{after}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `!cmd` runs locally — output lands in scrollback and NOTHING is
/// submitted to the worker.
#[test]
fn bang_shell_is_local_only() {
    let (mut app, _etx, wrx, mut term, caps) = harness();
    app.submit_text("!echo hello-from-shell");
    app.step(&mut term, &caps).unwrap();
    assert!(wrx.try_recv().is_err(), "! must never reach the worker");
    insta::assert_snapshot!("bang_shell", screen(&term));
}

/// `@` completion: workspace files fuzzy-match the fragment; Tab
/// completes the path into the composer.
#[test]
fn at_mention_completes_paths() {
    let root = std::env::temp_dir().join(format!("overseer-tui-at-{}", std::process::id()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(root.join("README.md"), "x\n").unwrap();

    let (etx, erx) = mpsc::channel();
    let (wtx, _wrx) = mpsc::channel();
    let mut app = App::new(
        erx,
        wtx,
        Preset::WorkspaceWrite,
        root.display().to_string(),
        "test-model".into(),
        root.join("session"),
    );
    let _ = etx;
    let mut backend = TestBackend::new(60, 20);
    backend.set_cursor_position((0, 10)).unwrap();
    let mut term = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(6),
        },
    )
    .unwrap();
    let caps = Caps::default();

    // The status line truncates the workspace cwd — normalize the
    // temp-dir prefix and the pid-bearing dirname separately.
    let tmp = std::env::temp_dir()
        .display()
        .to_string()
        .trim_end_matches('/')
        .to_string();
    let dirname = root.file_name().unwrap().to_str().unwrap().to_string();
    let norm = move |t: &Terminal<TestBackend>| {
        screen(t).replace(&tmp, "[tmp]").replace(&dirname, "[root]")
    };

    for c in "@mai".chars() {
        app.key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Char(c),
        ));
    }
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("at_menu", norm(&term));

    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Tab,
    ));
    app.step(&mut term, &caps).unwrap();
    insta::assert_snapshot!("at_completed", norm(&term));
    let _ = std::fs::remove_dir_all(&root);
}

// ── full-window surface: transcript + 2-row prompt + 1-row footer ──

fn full_harness() -> (
    App,
    mpsc::Sender<EngineMsg>,
    mpsc::Receiver<WorkerCmd>,
    Terminal<TestBackend>,
    Caps,
) {
    let (etx, erx) = mpsc::channel();
    let (wtx, wrx) = mpsc::channel();
    let mut app = App::new(
        erx,
        wtx,
        Preset::WorkspaceWrite,
        "/repo".into(),
        "test-model".into(),
        PathBuf::from("/tmp/session"),
    );
    app.mode = overseer_tui::app::UiMode::Full;
    let term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    (app, etx, wrx, term, Caps::default())
}

#[test]
fn full_layout_transcript_prompt_footer() {
    let (mut app, etx, _w, mut term, caps) = full_harness();
    etx.send(EngineMsg::Event(ev(EventKind::UserInput {
        text: "fix the flaky test".into(),
    })))
    .unwrap();
    etx.send(EngineMsg::Event(model_response("Looking at the test now.")))
        .unwrap();
    app.step(&mut term, &caps).unwrap();
    let s = screen(&term);
    insta::assert_snapshot!("full_layout", s);
    // The last row is the footer; the prompt's ❯ sits in the 2-row
    // window directly above it.
    let rows: Vec<&str> = s.trim_end_matches('\n').split('\n').collect();
    // Chromeless footer: the empty footer row is trimmed from the
    // dump entirely — the prompt's ❯ is the last visible row.
    let last = rows.last().unwrap();
    assert!(last.contains('❯'), "last row is the prompt: {last}");
}

#[test]
fn full_transcript_pages_up_with_footer_marker() {
    let (mut app, etx, _w, mut term, caps) = full_harness();
    // Overflow the transcript region (17 rows) so paging is real.
    for i in 0..8 {
        etx.send(EngineMsg::Event(ev(EventKind::UserInput {
            text: format!("prompt {i}"),
        })))
        .unwrap();
        etx.send(EngineMsg::Event(model_response(&format!(
            "answer {i} line a\nanswer {i} line b\nanswer {i} line c"
        ))))
        .unwrap();
    }
    app.step(&mut term, &caps).unwrap();
    let s = screen(&term);
    assert!(s.contains("answer 7"), "tail follows by default:\n{s}");

    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::PageUp,
    ));
    app.step(&mut term, &caps).unwrap();
    let s = screen(&term);
    insta::assert_snapshot!("full_paged", s);
    // The `↑N` marker lives on the footer row — the composer
    // placeholder's `↑` mustn't count.
    let footer =
        |t: &Terminal<TestBackend>| screen(t).split('\n').nth(19).unwrap_or("").to_string();
    assert!(footer(&term).contains('↑'), "footer scroll marker:\n{s}");
    assert!(
        !s.contains("answer 7"),
        "paged view should hide the tail:\n{s}"
    );

    // Submitting snaps back to the tail.
    app.submit_text("next task");
    app.step(&mut term, &caps).unwrap();
    assert!(!footer(&term).contains('↑'), "submit resets scroll");
}

#[test]
fn full_dialog_pins_over_transcript() {
    let (mut app, etx, _w, mut term, caps) = full_harness();
    // Scroll up, then open a permission dialog — it pins at the bottom.
    for i in 0..8 {
        etx.send(EngineMsg::Event(ev(EventKind::UserInput {
            text: format!("p{i}"),
        })))
        .unwrap();
        etx.send(EngineMsg::Event(model_response(&format!(
            "r{i}a\nr{i}b\nr{i}c"
        ))))
        .unwrap();
    }
    app.step(&mut term, &caps).unwrap();
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::PageUp,
    ));
    app.step(&mut term, &caps).unwrap();
    assert!(screen(&term).contains('↑'), "scrolled");

    let (rtx, _rrx) = mpsc::channel();
    etx.send(EngineMsg::Ask(
        AskRequest {
            tool: "bash".into(),
            input: serde_json::json!({"command": "rm -rf /tmp/x"}),
            reason: "destructive".into(),
        },
        rtx,
    ))
    .unwrap();
    app.step(&mut term, &caps).unwrap();
    let s = screen(&term);
    assert!(s.contains("permission"), "dialog visible:\n{s}");
    assert!(s.contains("rm -rf /tmp/x"), "typed preview:\n{s}");
    // The `↑N` marker lives on the footer row — the composer
    // placeholder's `↑` mustn't count.
    let footer = s.split('\n').nth(19).unwrap_or("");
    assert!(!footer.contains('↑'), "dialog force-follows the tail:\n{s}");
}

#[test]
fn full_panel_band_sits_below_prompt() {
    let (mut app, _etx, _w, mut term, caps) = full_harness();
    // ↑ on an empty composer opens the control panel.
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Up,
    ));
    app.step(&mut term, &caps).unwrap();
    let s = screen(&term);
    insta::assert_snapshot!("full_panel_band", s);
    // 3-row band sits below the prompt: strip lands two rows under ❯.
    let rows: Vec<&str> = s.trim_end_matches('\n').split('\n').collect();
    let prow = rows.iter().position(|r| r.contains('❯')).unwrap();
    assert!(
        rows[prow + 2].contains("dashboard"),
        "strip two rows below prompt: {:?}",
        rows[prow + 2]
    );
    assert!(s.contains("/repo"), "dashboard rows visible");
}

#[test]
fn full_panel_click_switches_tabs() {
    let (mut app, _etx, _w, mut term, caps) = full_harness();
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::F(1),
    ));
    app.step(&mut term, &caps).unwrap();
    assert!(screen(&term).contains("session"), "dashboard open");

    // Strip row = band top: on a 20-row grid the cap gives 15 rows
    // (min(17, 20-5)) → strip at row 4. " keys " is the fourth tab —
    // x 30..36 after " dashboard " + " agents " + " settings ".
    app.on_ct_event(crossterm::event::Event::Mouse(
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 32,
            row: 4,
            modifiers: crossterm::event::KeyModifiers::empty(),
        },
    ));
    app.step(&mut term, &caps).unwrap();
    let s = screen(&term);
    // §4 keys tab is the grouped list — the "edit" group proves it
    // (the band truncates before the "modes" group at 20 rows).
    assert!(s.contains("ctrl+w"), "keys tab after click:\n{s}");

    // Esc closes; the band gives the rows back to the transcript.
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Esc,
    ));
    app.step(&mut term, &caps).unwrap();
    assert!(!screen(&term).contains("dashboard"), "panel closed");
}

#[test]
fn panel_open_composer_still_types() {
    let (mut app, _etx, _w, mut term, caps) = full_harness();
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::F(1),
    ));
    // The panel is passive chrome — plain keys land in the composer.
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Char('h'),
    ));
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Char('i'),
    ));
    app.step(&mut term, &caps).unwrap();
    let s = screen(&term);
    assert!(s.contains("❯ hi"), "typed through the panel:\n{s}");
    // Backspace edits; arrows only leave the composer once it's empty.
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Backspace,
    ));
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Left,
    ));
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Right,
    ));
    app.step(&mut term, &caps).unwrap();
    let s = screen(&term);
    assert!(s.contains("❯ h"), "backspace lands:\n{s}");
    assert!(
        s.contains("dashboard"),
        "arrows with text stay in the composer, panel stays:\n{s}"
    );
}

// ── L2 graphite redesign ───────────────────────────────────────────

/// §5 empty state + §3 placeholder: a fresh session centres the mark
/// over the wordmark and hints at the composer.
#[test]
fn full_empty_state_and_placeholder() {
    let (mut app, _e, _w, mut term, caps) = full_harness();
    app.step(&mut term, &caps).unwrap();
    let s = screen(&term);
    assert!(s.contains('⋈'), "mark glyph centred:\n{s}");
    assert!(s.contains("overseer"), "wordmark:\n{s}");
    assert!(s.contains("ask anything"), "composer placeholder:\n{s}");
    insta::assert_snapshot!("empty_state", s);
}

/// §2 tool-line indent + glyphs: `●` ok/err, spinner frame running;
/// the end-of-run summary is right-aligned.
#[test]
fn full_tool_glyphs_and_run_summary() {
    let (mut app, etx, _w, mut term, caps) = full_harness();
    app.submit_text("run the checks");
    let _ = _w.try_recv(); // drain the Submit
    etx.send(EngineMsg::Event(ev(EventKind::UserInput {
        text: "run the checks".into(),
    })))
    .unwrap();
    etx.send(EngineMsg::Event(ev(EventKind::ToolCallStart {
        call_id: "c1".into(),
        name: "bash".into(),
        input: serde_json::json!({"command": "cargo test"}),
    })))
    .unwrap();
    etx.send(EngineMsg::Event(ev(EventKind::ToolResult {
        call_id: "c1".into(),
        name: "bash".into(),
        content: "ok".into(),
        is_error: false,
        raw_bytes: 2,
        spilled_to: None,
        denied: false,
    })))
    .unwrap();
    etx.send(EngineMsg::Event(ev(EventKind::ToolCallStart {
        call_id: "c2".into(),
        name: "read".into(),
        input: serde_json::json!({"path": "src/app.rs"}),
    })))
    .unwrap();
    etx.send(EngineMsg::Event(ev(EventKind::ToolResult {
        call_id: "c2".into(),
        name: "read".into(),
        content: "permission denied".into(),
        is_error: true,
        raw_bytes: 17,
        spilled_to: None,
        denied: false,
    })))
    .unwrap();
    etx.send(EngineMsg::Event(ev(EventKind::ToolCallStart {
        call_id: "c3".into(),
        name: "grep".into(),
        input: serde_json::json!({"pattern": "todo", "path": "src"}),
    })))
    .unwrap();
    app.step(&mut term, &caps).unwrap();
    let s = screen(&term);
    assert!(s.contains("● bash"), "ok glyph:\n{s}");
    assert!(s.contains("● read"), "err glyph:\n{s}");
    insta::assert_snapshot!("tool_lines", s);

    etx.send(EngineMsg::Event(ev(EventKind::RunEnd {
        stop_reason: "end_turn".into(),
        steps: 2,
        total_cost_usd: 0.042,
    })))
    .unwrap();
    app.step(&mut term, &caps).unwrap();
    let s = screen(&term);
    assert!(s.contains("2 steps"), "run summary:\n{s}");
    insta::assert_snapshot!("run_summary", s);
}

/// §4 keys tab: grouped two-column key list.
#[test]
fn full_panel_keys_tab() {
    let (mut app, _e, _w, mut term, caps) = full_harness();
    app.key(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Up,
    ));
    for _ in 0..3 {
        app.key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Right,
        ));
    }
    app.step(&mut term, &caps).unwrap();
    let s = screen(&term);
    assert!(s.contains("ctrl+w"), "grouped keys:\n{s}");
    insta::assert_snapshot!("panel_keys", s);
}
