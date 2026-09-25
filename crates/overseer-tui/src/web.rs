//! `--web`: the same App and fullscreen draw path, rendered to a
//! browser tab over localhost instead of ANSI bytes. The ratatui
//! buffer goes out as JSON on an SSE stream; the page renders it in a
//! monospace grid and POSTs input back as crossterm events — one
//! `App`, one state machine, zero second engine. Dev tool: assets are
//! served from `web/` on disk when present so CSS/JS edits are a
//! browser refresh, not a rebuild.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crossterm::event::{
    Event as CtEvent, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::backend::{Backend, TestBackend};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::probe::{Caps, ColorDepth};
use crate::TuiConfig;

/// Shared input channel — every HTTP connection thread can inject
/// events into the drive loop.
type InputTx = mpsc::Sender<CtEvent>;
/// Connected SSE clients — the drive loop writes each changed frame to
/// all of them and drops the ones whose sockets closed.
type Clients = Arc<Mutex<Vec<TcpStream>>>;
/// Latest frame, replayed to each SSE client on connect — otherwise a
/// tab that attaches between frames stares at a blank screen.
type LastFrame = Arc<Mutex<Option<String>>>;

/// Run the session on the web surface. Blocks until /quit (like `run`).
pub fn run_web(cfg: TuiConfig, port: u16) -> std::io::Result<i32> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let (input_tx, input_rx) = mpsc::channel::<CtEvent>();
    let clients: Clients = Arc::new(Mutex::new(Vec::new()));
    let last: LastFrame = Arc::new(Mutex::new(None));

    let (accept_input, accept_clients, accept_last) =
        (input_tx.clone(), clients.clone(), last.clone());
    std::thread::spawn(move || loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let (itx, cls, lf) = (
                    accept_input.clone(),
                    accept_clients.clone(),
                    accept_last.clone(),
                );
                std::thread::spawn(move || handle_conn(stream, itx, cls, lf));
            }
            Err(_) => return,
        }
    });

    let (mut app, worker) = crate::launch(cfg);
    app.mode = crate::app::UiMode::Full;
    // The browser renders everything — treat it as a truecolor,
    // non-mux peer. OSC stays off: there is no scrollback stream.
    let caps = Caps {
        color: ColorDepth::TrueColor,
        term_version: Some("overseer-web".into()),
        ..Caps::default()
    };

    let backend = TestBackend::new(100, 30);
    let mut term = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )
    .unwrap_or_else(|e| match e {}); // TestBackend::Error = Infallible

    eprintln!("overseer web: http://localhost:{port}");

    let mut last_hash = 0u64;
    while !app.quit {
        match input_rx.recv_timeout(std::time::Duration::from_millis(80)) {
            Ok(CtEvent::Resize(c, r)) => {
                // Resize is a draw-side concern as well as an input
                // event — the backend's own size feeds draw_full.
                term.backend_mut().resize(c, r);
                app.on_ct_event(CtEvent::Resize(c, r));
            }
            Ok(ev) => app.on_ct_event(ev),
            Err(mpsc::RecvTimeoutError::Timeout) => app.wake_tick(),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        app.step(&mut term, &caps)?;
        let frame = frame_json(&mut term);
        let h = fnv(frame.as_bytes());
        if h != last_hash {
            last_hash = h;
            *last.lock().unwrap() = Some(frame.clone());
            broadcast(&clients, &format!("data: {frame}\n\n"));
        }
    }
    app.shutdown();
    broadcast(&clients, "data: {\"bye\":true}\n\n");
    let _ = worker.join();
    Ok(0)
}

fn broadcast(clients: &Clients, msg: &str) {
    let mut list = clients.lock().unwrap();
    list.retain_mut(|s| s.write_all(msg.as_bytes()).is_ok());
}

/// 64-bit FNV-1a — cheap frame-equality check without cloning cells.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in bytes {
        h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
    }
    h
}

/// Serialize the terminal buffer as `{w,h,cur,rows:[ [span] ]}` where a
/// span is `{t,f,b,m}` — text, css fg, css bg, modifier bits. Blank
/// trailing cells are trimmed per row; the browser pads implicitly.
fn frame_json(term: &mut Terminal<TestBackend>) -> String {
    let backend = term.backend_mut();
    let pos = backend
        .get_cursor_position()
        .map(|p| (p.x, p.y))
        .unwrap_or((0, 0));
    let buf = backend.buffer();
    let Rect {
        width: w,
        height: h,
        ..
    } = buf.area;
    let mut out = format!("{{\"w\":{w},\"h\":{h},");
    out.push_str(&format!("\"cur\":[{},{}],\"rows\":[", pos.0, pos.1));
    for (y, row) in buf.content.chunks(w as usize).enumerate() {
        if y > 0 {
            out.push(',');
        }
        out.push('[');
        let mut first = true;
        let mut run: Option<(Color, Color, Modifier)> = None;
        let mut text = String::new();
        for cell in row {
            let sty = (cell.fg, cell.bg, cell.modifier);
            if run != Some(sty) && !text.is_empty() {
                span_json(&mut out, &text, run.unwrap(), &mut first);
                text.clear();
            }
            run = Some(sty);
            text.push_str(cell.symbol());
        }
        if !text.is_empty() {
            span_json(&mut out, &text, run.unwrap(), &mut first);
        }
        out.push(']');
    }
    out.push_str("]}");
    out
}

fn span_json(
    out: &mut String,
    text: &str,
    (fg, bg, m): (Color, Color, Modifier),
    first: &mut bool,
) {
    if !*first {
        out.push(',');
    }
    *first = false;
    out.push_str("{\"t\":");
    json_str(out, text);
    if fg != Color::Reset {
        out.push_str(",\"f\":");
        json_str(out, &color_css(fg));
    }
    if bg != Color::Reset {
        out.push_str(",\"b\":");
        json_str(out, &color_css(bg));
    }
    let bits = m.bits();
    if bits != 0 {
        out.push_str(&format!(",\"m\":{bits}"));
    }
    out.push('}');
}

fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// ratatui Color → CSS. Indexed colors go out as `iNNN` and the client
/// resolves them against the standard xterm palette.
fn color_css(c: Color) -> String {
    match c {
        Color::Reset | Color::Black => "black".into(),
        Color::Red => "#e86671".into(),
        Color::Green => "#98c379".into(),
        Color::Yellow => "#e5c07b".into(),
        Color::Blue => "#61afef".into(),
        Color::Magenta => "#c678dd".into(),
        Color::Cyan => "#56b6c2".into(),
        Color::Gray | Color::White => "#abb2bf".into(),
        Color::DarkGray => "#5c6370".into(),
        Color::LightRed => "#f78c8c".into(),
        Color::LightGreen => "#b5e890".into(),
        Color::LightYellow => "#f2d99c".into(),
        Color::LightBlue => "#82cfff".into(),
        Color::LightMagenta => "#d8a2e8".into(),
        Color::LightCyan => "#7bdde0".into(),
        Color::Indexed(i) => format!("i{i}"),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

// ── HTTP ────────────────────────────────────────────────────────────

fn handle_conn(mut stream: TcpStream, input: InputTx, clients: Clients, last: LastFrame) {
    let _ = stream.set_nodelay(true);
    let mut head = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    // Read the request head; stop at the header/body boundary.
    while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
        if stream.read(&mut byte).unwrap_or(0) == 0 {
            return;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let mut parts = head.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let body_len = head
        .lines()
        .find_map(|l| {
            l.strip_prefix("Content-Length:")
                .map(|v| v.trim().parse::<usize>())
        })
        .and_then(|r| r.ok())
        .unwrap_or(0);
    let mut body = vec![0u8; body_len];
    let _ = stream.read_exact(&mut body);

    match (method, path.split('?').next().unwrap_or("/")) {
        ("GET", "/events") => {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n",
            );
            if let Some(f) = last.lock().unwrap().as_ref() {
                let _ = stream.write_all(format!("data: {f}\n\n").as_bytes());
            }
            if stream.flush().is_ok() {
                clients.lock().unwrap().push(stream);
            }
        }
        ("POST", "/input") => {
            if let Some(ev) = parse_input(&body) {
                let _ = input.send(ev);
            }
            let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\n\r\n");
        }
        ("GET", p) => {
            let (file, ctype) = match p {
                "/" | "/index.html" => ("index.html", "text/html; charset=utf-8"),
                "/app.js" => ("app.js", "text/javascript; charset=utf-8"),
                "/style.css" => ("style.css", "text/css; charset=utf-8"),
                _ => {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length:0\r\n\r\n");
                    return;
                }
            };
            serve_file(&mut stream, file, ctype);
        }
        _ => {
            let _ = stream.write_all(b"HTTP/1.1 405\r\nContent-Length:0\r\n\r\n");
        }
    }
}

/// Dev loop wants disk-fresh assets: serve from `web/` next to the
/// source when it exists, else the copies baked into the binary.
fn serve_file(stream: &mut TcpStream, name: &str, ctype: &str) {
    let disk = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("web")
        .join(name);
    let content = std::fs::read(&disk).unwrap_or_else(|_| match name {
        "index.html" => include_str!("../web/index.html").as_bytes().to_vec(),
        "app.js" => include_str!("../web/app.js").as_bytes().to_vec(),
        "style.css" => include_str!("../web/style.css").as_bytes().to_vec(),
        _ => Vec::new(),
    });
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n",
        content.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&content);
}

// ── input mapping ───────────────────────────────────────────────────

/// Browser → crossterm. The page speaks a tiny JSON vocabulary:
/// `{type:"key",code:"char",ch:"a",ctrl,alt,shift}`,
/// `{type:"key",code:"enter"|"esc"|...}`, `{type:"paste",text}`,
/// `{type:"scroll",up}`, `{type:"resize",cols,rows}`,
/// `{type:"focus",gained}`.
fn parse_input(body: &[u8]) -> Option<CtEvent> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let get = |k: &str| v.get(k);
    match get("type")?.as_str()? {
        "key" => {
            let mut mods = KeyModifiers::empty();
            if get("ctrl").and_then(|b| b.as_bool()).unwrap_or(false) {
                mods |= KeyModifiers::CONTROL;
            }
            if get("alt").and_then(|b| b.as_bool()).unwrap_or(false) {
                mods |= KeyModifiers::ALT;
            }
            if get("shift").and_then(|b| b.as_bool()).unwrap_or(false) {
                mods |= KeyModifiers::SHIFT;
            }
            let code = match get("code")?.as_str()? {
                "char" => KeyCode::Char(get("ch")?.as_str()?.chars().next()?),
                "enter" => KeyCode::Enter,
                "esc" => KeyCode::Esc,
                "backspace" => KeyCode::Backspace,
                "delete" => KeyCode::Delete,
                "tab" => KeyCode::Tab,
                "backtab" => KeyCode::BackTab,
                "up" => KeyCode::Up,
                "down" => KeyCode::Down,
                "left" => KeyCode::Left,
                "right" => KeyCode::Right,
                "home" => KeyCode::Home,
                "end" => KeyCode::End,
                "pageup" => KeyCode::PageUp,
                "pagedown" => KeyCode::PageDown,
                _ => return None,
            };
            Some(CtEvent::Key(KeyEvent::new(code, mods)))
        }
        "paste" => Some(CtEvent::Paste(get("text")?.as_str()?.to_string())),
        "scroll" => Some(CtEvent::Mouse(MouseEvent {
            kind: if get("up").and_then(|b| b.as_bool()).unwrap_or(false) {
                MouseEventKind::ScrollUp
            } else {
                MouseEventKind::ScrollDown
            },
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        })),
        "resize" => Some(CtEvent::Resize(
            get("cols")?.as_u64()? as u16,
            get("rows")?.as_u64()? as u16,
        )),
        "focus" => Some(if get("gained").and_then(|b| b.as_bool()).unwrap_or(true) {
            CtEvent::FocusGained
        } else {
            CtEvent::FocusLost
        }),
        _ => None,
    }
}
