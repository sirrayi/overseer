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
    assert_eq!(
        control.queued(),
        vec!["also rename the helper".to_string()]
    );
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
