//! `--web`: the same App and fullscreen draw path, rendered to a
//! browser tab over localhost instead of ANSI bytes. The ratatui
//! buffer goes out as JSON on an SSE stream; the page renders it in a
//! monospace grid and POSTs input back as crossterm events — one
//! `App`, one state machine, zero second engine. Dev tool: assets are
//! served from `web/` on disk when present so CSS/JS edits are a
//! browser refresh, not a rebuild.
//!
//! Security model: the server can drive a shell-capable agent, so
//! `GET /input` needs more than the loopback address. A per-install
//! bearer token (`~/.overseer/web/token`, 0600 in a 0700 dir) rides the
//! URL *fragment* — `#t=` never leaves the browser in a request line or
//! a Referer — and `app.js` hands it to `/events` as `?t=` (EventSource
//! can't set headers) and to `POST /input` as `X-Overseer-Token`. Every
//! request must carry a loopback `Host:` (DNS-rebinding defence) and
//! `/events` + `/input` reject cross-site fetch metadata.
//!
//! Limits: ≤32 concurrent connections (long-lived SSE clients count
//! toward the cap — a writer thread drops the guard only when its
//! socket dies, so a closed tab frees the slot), request head ≤16 KiB
//! in 10 s, body ≤64 KiB checked against Content-Length BEFORE the
//! allocation, and every SSE write carries a 5 s timeout so a stalled
//! tab can never wedge the drive loop.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crossterm::event::{
    Event as CtEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::backend::{Backend, TestBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::probe::{Caps, ColorDepth};
use crate::TuiConfig;

/// Shared input channel — every HTTP connection thread can inject
/// events into the drive loop.
type InputTx = mpsc::Sender<CtEvent>;
/// Connected SSE clients — one `sync_channel(1)` sender per client,
/// fed by a per-client writer thread. Broadcast is `try_send`: a full
/// channel drops the stale frame (frames are full snapshots), so a
/// stalled reader never blocks the UI loop.
type Clients = Arc<Mutex<Vec<mpsc::SyncSender<String>>>>;
/// Latest frame, replayed to each SSE client on connect — otherwise a
/// tab that attaches between frames stares at a blank screen.
type LastFrame = Arc<Mutex<Option<String>>>;

/// Auto-pick scans upward from the default; `--web-port` pins instead.
const PORT_RANGE: std::ops::RangeInclusive<u16> = 8641..=8660;
const MAX_CONNS: usize = 32;
const HEAD_CAP: usize = 16 * 1024;
const BODY_CAP: usize = 64 * 1024;
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(15);

/// Security headers on the page + assets. `style-src 'self'` works
/// because `app.js` styles spans via CSSOM (`el.style.*`) — parser-level
/// inline style is the only thing the CSP needs to forbid.
const SEC_HEADERS: &str = concat!(
    "Content-Security-Policy: default-src 'self'; img-src 'self' data:; style-src 'self'; ",
    "script-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; ",
    "form-action 'none'\r\n",
    "X-Content-Type-Options: nosniff\r\n",
    "Referrer-Policy: no-referrer\r\n"
);

/// Web-surface options: the port (None → scan [`PORT_RANGE`]) and
/// whether to auto-open a browser.
pub struct WebOpts {
    pub port: Option<u16>,
    pub open: bool,
}

/// CLI entry point: pinned port, never auto-opens (the caller prints
/// the URL). All the hardening lives in the server below — this is
/// just a narrower front door.
pub fn run_web(cfg: TuiConfig, port: u16) -> std::io::Result<i32> {
    run_web_with(
        cfg,
        WebOpts {
            port: Some(port),
            open: false,
        },
    )
}

/// Run the session on the web surface. Blocks until /quit (like `run`).
/// `opts.port` pins the bind (busy = error); `None` scans 8641..=8660.
/// `opts.open` opens the tokenized URL in the system browser unless the
/// environment says it can't (SSH / headless).
pub fn run_web_with(cfg: TuiConfig, opts: WebOpts) -> std::io::Result<i32> {
    let listener = bind_port(opts.port)?;
    let port = listener.local_addr()?.port();
    let token = web_token()?;
    let url = format!("http://127.0.0.1:{port}/#t={token}");

    let (input_tx, input_rx) = mpsc::channel::<CtEvent>();
    let ctx = Arc::new(Ctx {
        token,
        port,
        input: input_tx,
        clients: Clients::new(Mutex::new(Vec::new())),
        last: LastFrame::new(Mutex::new(None)),
        conns: Arc::new(AtomicUsize::new(0)),
    });

    {
        let ctx = ctx.clone();
        std::thread::spawn(move || loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let ctx = ctx.clone();
                    std::thread::spawn(move || handle_conn(stream, ctx));
                }
                Err(_) => return,
            }
        });
    }

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

    eprintln!("overseer web: {url}");
    if auto_open_ok(opts.open) {
        open_url(&url);
    }

    // Frame diffing happens on the buffer + cursor + prompt_top BEFORE
    // serialization — an idle tick costs a PartialEq, not a JSON write.
    let mut prev: Option<(Buffer, (u16, u16), u16)> = None;
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
        let changed = {
            let backend = term.backend_mut();
            let pos = backend
                .get_cursor_position()
                .map(|p| (p.x, p.y))
                .unwrap_or((0, 0));
            let cur = (backend.buffer().clone(), pos, app.prompt_top);
            match &prev {
                Some(p) if *p == cur => false,
                _ => {
                    prev = Some(cur);
                    true
                }
            }
        };
        if changed {
            let frame = frame_json(&mut term, app.prompt_top, app.transcript_empty());
            *ctx.last.lock().unwrap() = Some(frame.clone());
            broadcast(&ctx.clients, &format!("data: {frame}\n\n"));
        }
    }
    app.shutdown();
    broadcast(&ctx.clients, "data: {\"bye\":true}\n\n");
    let _ = worker.join();
    Ok(0)
}

fn bind_port(port: Option<u16>) -> std::io::Result<TcpListener> {
    match port {
        Some(p) => TcpListener::bind(("127.0.0.1", p)),
        None => {
            let mut last = std::io::Error::new(std::io::ErrorKind::AddrInUse, "no ports tried");
            for p in PORT_RANGE {
                match TcpListener::bind(("127.0.0.1", p)) {
                    Ok(l) => return Ok(l),
                    Err(e) => last = e,
                }
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("no free web port in {PORT_RANGE:?} (last: {last})"),
            ))
        }
    }
}

/// Auto-open policy: `open` opt-in minus SSH sessions and headless
/// Linux. Kept env-pure so tests can drive every arm.
fn auto_open_ok_env(open: bool, ssh: bool, display: bool, os: &str) -> bool {
    if !open || ssh {
        return false;
    }
    match os {
        "macos" => true,
        // A Linux box with no display server has nothing to open into.
        "linux" => display,
        _ => false,
    }
}

fn auto_open_ok(open: bool) -> bool {
    auto_open_ok_env(
        open,
        std::env::var_os("SSH_CONNECTION").is_some(),
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some(),
        std::env::consts::OS,
    )
}

/// `open` (macOS) / `xdg-open` (Linux), detached — the child's stdio is
/// nulled and spawn errors are ignored (a missing opener is a warning's
/// worth of trouble, not a failed launch).
fn open_url(url: &str) {
    let browser = match std::env::consts::OS {
        "macos" => "open",
        "linux" => "xdg-open",
        _ => return,
    };
    let _ = std::process::Command::new(browser)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Per-install bearer token for `/events` and `/input`: 32 bytes of
/// `/dev/urandom`, hex, persisted at `~/.overseer/web/token` (dir 0700,
/// file 0600) and reused across launches so an installed PWA's origin
/// keeps working.
fn web_token() -> std::io::Result<String> {
    // No HOME → no per-install token. Refuse outright: falling back to
    // `./.overseer` would drop a bearer secret in whatever directory
    // the caller happened to sit in.
    let home = std::env::var("HOME")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set"))?;
    let dir = std::path::PathBuf::from(home).join(".overseer").join("web");
    overseer_core::harden::ensure_private_dir(&dir)?;
    let path = dir.join("token");
    // symlink_metadata, not metadata: a planted symlink or a file owned
    // by a different uid is regenerated, never trusted.
    let reusable = match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                !m.file_type().is_symlink() && m.uid() == unsafe { libc::getuid() }
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
        _ => false,
    };
    if reusable {
        if let Ok(t) = std::fs::read_to_string(&path) {
            let t = t.trim();
            if t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
                // Re-assert 0600 — an older build or a manual copy may
                // have left the file lax.
                write_private(&path, t.as_bytes())?;
                return Ok(t.to_string());
            }
        }
    }
    let mut raw = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut raw)?;
    let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    write_private(&path, token.as_bytes())?;
    Ok(token)
}

/// Create-or-replace `path` owner-only (0600). A preexisting symlink
/// is removed first — `OpenOptions` would otherwise follow it and
/// write the secret into the target's file.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        std::fs::remove_file(path)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Length-agnostic byte compare — the token check must not leak a
/// prefix via early exit.
fn token_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let n = a.len().max(b.len()).max(1);
    for i in 0..n {
        let x = a.get(i % a.len().max(1)).copied().unwrap_or(0);
        let y = b.get(i % b.len().max(1)).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

/// Connection-slot lease. Long-lived SSE connections hold their slot
/// in the writer thread (see `handle_conn`) — the slot frees exactly
/// when the socket dies, not when the handler returns.
struct ConnGuard(Arc<AtomicUsize>);

impl ConnGuard {
    fn claim(n: &Arc<AtomicUsize>) -> Option<ConnGuard> {
        n.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| {
            (c < MAX_CONNS).then_some(c + 1)
        })
        .ok()?;
        Some(ConnGuard(n.clone()))
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Per-request context: everything `handle_conn` needs that isn't the
/// socket itself.
struct Ctx {
    token: String,
    port: u16,
    input: InputTx,
    clients: Clients,
    last: LastFrame,
    conns: Arc<AtomicUsize>,
}

fn broadcast(clients: &Clients, msg: &str) {
    let mut list = clients.lock().unwrap();
    list.retain(|tx| match tx.try_send(msg.to_string()) {
        Ok(()) => true,
        // Full = a slow reader drops a stale snapshot; it still gets
        // the next frame. Disconnected = writer is gone, drop the slot.
        Err(mpsc::TrySendError::Full(_)) => true,
        Err(mpsc::TrySendError::Disconnected(_)) => false,
    });
}

/// Serialize the terminal buffer as `{w,h,p,e,cur,rows:[ [span] ]}`
/// where a span is `{t,f,b,m}` — text, css fg, css bg, modifier bits.
/// `p` is the prompt band's first row (the client shifts it for its
/// dip); `e` marks the empty state (client overlays the mark +
/// wordmark). Blank trailing cells are trimmed per row; the browser
/// pads.
fn frame_json(term: &mut Terminal<TestBackend>, prompt_top: u16, transcript_empty: bool) -> String {
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
    let mut out = format!("{{\"w\":{w},\"h\":{h},\"p\":{prompt_top},");
    if transcript_empty {
        out.push_str("\"e\":true,");
    }
    out.push_str(&format!("\"cur\":[{},{}],\"rows\":[", pos.0, pos.1));
    for (y, row) in buf.content.chunks(w as usize).enumerate() {
        if y > 0 {
            out.push(',');
        }
        out.push('[');
        // The client pads rows to full width — trailing blank cells in
        // the default style carry nothing.
        let end = row
            .iter()
            .rposition(|c| {
                !c.symbol().trim().is_empty()
                    || c.fg != Color::Reset
                    || c.bg != Color::Reset
                    || !c.modifier.is_empty()
            })
            .map(|i| i + 1)
            .unwrap_or(0);
        let mut first = true;
        let mut run: Option<(Color, Color, Modifier)> = None;
        let mut text = String::new();
        for cell in &row[..end] {
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

/// ratatui Color → CSS. §1: named ANSI colors map to muted,
/// graphite-adjacent equivalents — nothing saturated reaches the web
/// surface. Indexed colors go out as `iNNN` and the client resolves
/// them against the standard xterm palette.
fn color_css(c: Color) -> String {
    match c {
        Color::Reset | Color::Gray | Color::White => "#d4d6db".into(),
        Color::Black => "#0d0e10".into(),
        Color::Red => "#d77b7b".into(),
        Color::Green => "#7fb58a".into(),
        Color::Yellow => "#d4b26a".into(),
        Color::Blue => "#8ea4c8".into(),
        Color::Magenta => "#b79bd8".into(),
        Color::Cyan => "#7fb8c4".into(),
        Color::DarkGray => "#858a94".into(),
        Color::LightRed => "#e3a1a1".into(),
        Color::LightGreen => "#a0cba9".into(),
        Color::LightYellow => "#e5cf9b".into(),
        Color::LightBlue => "#a9c1e8".into(),
        Color::LightMagenta => "#c9b3e2".into(),
        Color::LightCyan => "#a0d3dc".into(),
        Color::Indexed(i) => format!("i{i}"),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

// ── HTTP ────────────────────────────────────────────────────────────

struct Request {
    method: String,
    /// Raw path+query — routing splits on `?` later.
    path: String,
    /// Header names lowercased; last value wins on duplicates.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// One query value — `?a=x&b=y` order-agnostic.
    fn query(&self, key: &str) -> Option<String> {
        let q = self.path.split_once('?')?.1;
        q.split('&').find_map(|kv| {
            kv.split_once('=').and_then(|(k, v)| {
                (k == key).then(|| String::from_utf8_lossy(&url_decode(v)).into_owned())
            })
        })
    }

    fn route(&self) -> &str {
        self.path.split('?').next().unwrap_or("")
    }
}

/// Head-read failure: `Silent` (EOF/timeout/malformed — hang up) or
/// `Status` (report a code, then hang up).
enum HeadErr {
    Silent,
    Status(u16),
}

/// Request head: chunked raw reads — never a per-line `read_until`,
/// so a newline-less line can't allocate past the 16 KiB cap before
/// the 431 check sees it, and `deadline` is a wall-clock total from
/// accept (not per-read): a 1-byte-per-9 s dribbler can't pin one of
/// the 32 connection slots forever. Content-Length is capped at
/// 64 KiB BEFORE the body allocation — a lying header gets 413, never
/// a giant `vec!`. Head reads never wait on the body beyond the
/// socket's own read timeout.
fn read_request(stream: &TcpStream) -> Result<Request, HeadErr> {
    read_request_within(stream, std::time::Instant::now() + READ_TIMEOUT)
}

fn read_request_within(
    stream: &TcpStream,
    deadline: std::time::Instant,
) -> Result<Request, HeadErr> {
    let mut s = stream.try_clone().map_err(|_| HeadErr::Silent)?;
    let mut head = Vec::with_capacity(1024);
    let mut buf = [0u8; 4096];
    let split = loop {
        if std::time::Instant::now() >= deadline {
            return Err(HeadErr::Silent);
        }
        let n = s.read(&mut buf).map_err(|_| HeadErr::Silent)?;
        if n == 0 {
            return Err(HeadErr::Silent);
        }
        head.extend_from_slice(&buf[..n]);
        if head.len() > HEAD_CAP {
            return Err(HeadErr::Status(431));
        }
        if let Some(i) = head_end(&head) {
            break i;
        }
    };
    let head_text = String::from_utf8_lossy(&head[..split]);
    let mut lines = head_text.lines();
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    if method.is_empty() {
        return Err(HeadErr::Silent);
    }
    let headers = lines
        .clone()
        .filter_map(|l| {
            l.split_once(':')
                .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
        })
        .collect();
    let body_len = lines
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    if body_len > BODY_CAP {
        return Err(HeadErr::Status(413));
    }
    // Chunked reads may have already swallowed part of the body.
    let mut body = head[split..].to_vec();
    body.truncate(body_len);
    if body.len() < body_len {
        let mut rest = vec![0u8; body_len - body.len()];
        // Short body = the sender lied about its length — that's a 400,
        // not a silent trunc.
        if s.read_exact(&mut rest).is_err() {
            return Err(HeadErr::Status(400));
        }
        body.extend_from_slice(&rest);
    }
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

/// Index just past the blank line that ends the head — `\r\n\r\n`, or
/// a bare `\n\n` for clients that skip CR.
fn head_end(b: &[u8]) -> Option<usize> {
    if let Some(i) = b.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some(i + 4);
    }
    b.windows(2).position(|w| w == b"\n\n").map(|i| i + 2)
}

/// One accepted connection. Rejections in order: over-cap 503 → bad
/// Host 421 → bad head → cross-site metadata 403 → bad token 401 →
/// route.
fn handle_conn(mut stream: TcpStream, ctx: Arc<Ctx>) {
    let Some(guard) = ConnGuard::claim(&ctx.conns) else {
        let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length:0\r\n\r\n");
        return;
    };
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let req = match read_request(&stream) {
        Ok(r) => r,
        Err(HeadErr::Silent) => return,
        Err(HeadErr::Status(code)) => {
            let _ = respond(&mut stream, code, "");
            return;
        }
    };
    if !host_ok(req.header("host"), ctx.port) {
        // DNS rebinding: a victim's browser re-pointed at our port
        // still arrives under THEIR Host — refuse to speak to it.
        let _ = respond(&mut stream, 421, "");
        return;
    }
    let route = req.route();
    if matches!(route, "/events" | "/input") {
        if !fetch_metadata_ok(&req, ctx.port) {
            let _ = respond(&mut stream, 403, "");
            return;
        }
        if !token_ok(&req, &ctx.token) {
            let _ = respond(&mut stream, 401, "");
            return;
        }
    }
    match (req.method.as_str(), route) {
        ("GET", "/events") => {
            // The writer thread now owns the socket AND the connection
            // slot — `guard` moves with it, so the cap frees only when
            // the client actually goes away.
            let (tx, rx) = mpsc::sync_channel::<String>(1);
            if stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n",
                )
                .is_err()
            {
                return;
            }
            if let Some(f) = ctx.last.lock().unwrap().as_ref() {
                if stream
                    .write_all(format!("data: {f}\n\n").as_bytes())
                    .is_err()
                {
                    return;
                }
            }
            let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
            ctx.clients.lock().unwrap().push(tx);
            std::thread::spawn(move || sse_writer(stream, rx, guard));
        }
        // Some preview proxies forward POSTs but drop the body — the
        // page also speaks `GET /input?d=<json>`, which survives.
        // NOTE: that fallback puts `?t=` in a URL. Acceptable here:
        // the request is `fetch`, not navigation, and
        // `Referrer-Policy: no-referrer` keeps it out of Referer
        // headers — the token still never crosses a third party.
        ("POST", "/input") | ("GET", "/input") => {
            let decoded = req.query("d");
            let payload: &[u8] = if req.method == "GET" {
                decoded.as_ref().map(|s| s.as_bytes()).unwrap_or(&[])
            } else {
                &req.body
            };
            match parse_input(payload) {
                Some(ev) => {
                    let _ = ctx.input.send(ev);
                    let _ = respond(&mut stream, 204, "");
                }
                None => {
                    let _ = respond(&mut stream, 400, "");
                }
            }
        }
        ("GET", p) => {
            let (file, ctype) = match p {
                "/" | "/index.html" => ("index.html", "text/html; charset=utf-8"),
                "/app.js" => ("app.js", "text/javascript; charset=utf-8"),
                "/style.css" => ("style.css", "text/css; charset=utf-8"),
                // PWA assets — all embedded, all unauthenticated.
                "/mark.svg" => ("mark.svg", "image/svg+xml"),
                "/favicon.svg" => ("favicon.svg", "image/svg+xml"),
                "/manifest.webmanifest" => ("manifest.webmanifest", "application/manifest+json"),
                p if p.starts_with("/icon-") && p.ends_with(".png") && !p.contains("..") => {
                    (&p[1..], "image/png")
                }
                _ => {
                    let _ = respond(&mut stream, 404, "");
                    return;
                }
            };
            serve_file(&mut stream, file, ctype);
        }
        _ => {
            let _ = respond(&mut stream, 405, "");
        }
    }
}

/// SSE writer loop: channel → socket under a write timeout. Exits on
/// send error (client gone → the guard frees its cap slot) or when
/// `clients` drops the sender.
fn sse_writer(mut stream: TcpStream, rx: mpsc::Receiver<String>, guard: ConnGuard) {
    let _guard = guard;
    loop {
        match rx.recv_timeout(KEEPALIVE) {
            Ok(msg) => {
                if stream.write_all(msg.as_bytes()).is_err() {
                    return;
                }
            }
            // ":" is an SSE comment — a no-op heartbeat that flushes
            // dead sockets via the write timeout.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stream.write_all(b":\n\n").is_err() {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn respond(stream: &mut TcpStream, code: u16, reason: &str) -> std::io::Result<()> {
    let reason = if reason.is_empty() {
        match code {
            204 => "No Content",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Content Too Large",
            421 => "Misdirected Request",
            431 => "Request Header Fields Too Large",
            503 => "Service Unavailable",
            _ => "",
        }
    } else {
        reason
    };
    stream.write_all(format!("HTTP/1.1 {code} {reason}\r\nContent-Length:0\r\n\r\n").as_bytes())
}

/// Loopback-only Host check (rebinding defence): the listener binds
/// 127.0.0.1, so a valid request names one of the loopback spellings.
fn host_ok(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else { return false };
    let host = host.trim().to_lowercase();
    host == format!("127.0.0.1:{port}")
        || host == format!("localhost:{port}")
        || host == format!("[::1]:{port}")
}

/// `/events` + `/input` cross-site defence: a present `Sec-Fetch-Site`
/// must be `same-origin`/`none` (never `cross-site`/`same-site` — a
/// same-site evil subdomain still isn't our origin), and a present
/// `Origin` must be our own.
fn fetch_metadata_ok(req: &Request, port: u16) -> bool {
    if let Some(s) = req.header("sec-fetch-site") {
        if !matches!(s.trim().to_lowercase().as_str(), "same-origin" | "none") {
            return false;
        }
    }
    if let Some(o) = req.header("origin") {
        let o = o.trim().to_lowercase();
        if o != format!("http://127.0.0.1:{port}") && o != format!("http://localhost:{port}") {
            return false;
        }
    }
    true
}

/// Bearer check: `X-Overseer-Token` header (POST path) or the `t=`
/// query param (EventSource can't set headers). Constant-time.
fn token_ok(req: &Request, token: &str) -> bool {
    if let Some(h) = req.header("x-overseer-token") {
        return token_eq(h.trim().as_bytes(), token.as_bytes());
    }
    match req.query("t") {
        Some(t) => token_eq(t.as_bytes(), token.as_bytes()),
        None => false,
    }
}

/// Dev loop wants disk-fresh assets: DEBUG builds serve from `web/`
/// next to the source when it exists. Release builds serve only the
/// embedded copies — `CARGO_MANIFEST_DIR` is a build-time path that
/// would let a deployed binary read whatever lives at that source path.
fn serve_file(stream: &mut TcpStream, name: &str, ctype: &str) {
    let disk = cfg!(debug_assertions)
        .then(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("web")
                .join(name)
        })
        .and_then(|p| std::fs::read(p).ok());
    let content = disk.unwrap_or_else(|| embedded_asset(name).to_vec());
    if content.is_empty() {
        let _ = respond(stream, 404, "");
        return;
    }
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n{SEC_HEADERS}\r\n",
        content.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&content);
}

/// Compile-time copies of every served asset — release builds serve
/// these and nothing else (see `serve_file`).
fn embedded_asset(name: &str) -> &'static [u8] {
    match name {
        "index.html" => include_bytes!("../web/index.html"),
        "app.js" => include_bytes!("../web/app.js"),
        "style.css" => include_bytes!("../web/style.css"),
        "mark.svg" => include_bytes!("../web/mark.svg"),
        "favicon.svg" => include_bytes!("../web/favicon.svg"),
        "manifest.webmanifest" => include_bytes!("../web/manifest.webmanifest"),
        "icon-192.png" => include_bytes!("../web/icon-192.png"),
        "icon-512.png" => include_bytes!("../web/icon-512.png"),
        _ => &[],
    }
}

/// Minimal percent-decoder for the `?d=`/`?t=` payloads — `%XX` and
/// `+` are all the page emits via `encodeURIComponent` (which actually
/// uses `%20`, but `+` is harmless to support).
fn url_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        out.push(match b[i] {
            b'%' if i + 2 < b.len() => {
                let hv = u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap_or(0);
                i += 3;
                hv
            }
            b'+' => {
                i += 1;
                b' '
            }
            c => {
                i += 1;
                c
            }
        });
    }
    out
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
                "f1" => KeyCode::F(1),
                _ => return None,
            };
            Some(CtEvent::Key(KeyEvent::new(code, mods)))
        }
        // The bottom mark — opens the control panel (same as F1).
        "panel" => Some(CtEvent::Key(KeyEvent::new(
            KeyCode::F(1),
            KeyModifiers::NONE,
        ))),
        // A left click in the grid — row/col land on the same hit
        // regions the terminal's mouse events use.
        "click" => Some(CtEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: get("col")?.as_u64()? as u16,
            row: get("row")?.as_u64()? as u16,
            modifiers: KeyModifiers::empty(),
        })),
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

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn test_ctx(port: u16) -> (Arc<Ctx>, mpsc::Receiver<CtEvent>) {
        let (input_tx, input_rx) = mpsc::channel();
        (
            Arc::new(Ctx {
                token: TOKEN.into(),
                port,
                input: input_tx,
                clients: Clients::new(Mutex::new(Vec::new())),
                last: LastFrame::new(Mutex::new(None)),
                conns: Arc::new(AtomicUsize::new(0)),
            }),
            input_rx,
        )
    }

    /// A real listener on port 0 feeding `handle_conn` — the same
    /// dispatch shape `run_web_with` uses.
    fn server() -> (u16, mpsc::Receiver<CtEvent>, Arc<Ctx>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (ctx, rx) = test_ctx(port);
        let accept_ctx = ctx.clone();
        std::thread::spawn(move || loop {
            match listener.accept() {
                Ok((s, _)) => {
                    let c = accept_ctx.clone();
                    std::thread::spawn(move || handle_conn(s, c));
                }
                Err(_) => return,
            }
        });
        (port, rx, ctx)
    }

    /// Send one raw request, return the status line.
    fn exchange(port: u16, raw: &str) -> String {
        use std::io::{BufRead, BufReader};
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        s.shutdown(std::net::Shutdown::Write).ok();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        line.trim().to_string()
    }

    fn host(port: u16) -> String {
        format!("Host: 127.0.0.1:{port}\r\n")
    }

    #[test]
    fn token_gates_events_and_input() {
        let (port, _rx, _ctx) = server();
        assert_eq!(
            exchange(port, &format!("GET /events HTTP/1.1\r\n{}\r\n", host(port))),
            "HTTP/1.1 401 Unauthorized"
        );
        assert_eq!(
            exchange(
                port,
                &format!("GET /events?t=wrong HTTP/1.1\r\n{}\r\n", host(port))
            ),
            "HTTP/1.1 401 Unauthorized"
        );
        let body = "{\"type\":\"x\"}";
        assert_eq!(
            exchange(
                port,
                &format!(
                    "POST /input HTTP/1.1\r\n{}Content-Length: {}\r\n\r\n{body}",
                    host(port),
                    body.len()
                )
            ),
            "HTTP/1.1 401 Unauthorized"
        );
        // Static assets need no token.
        assert_eq!(
            exchange(port, &format!("GET / HTTP/1.1\r\n{}\r\n", host(port))),
            "HTTP/1.1 200 OK"
        );
    }

    #[test]
    fn input_accepts_header_and_query_tokens() {
        let (port, rx, _ctx) = server();
        let body = "{\"type\":\"key\",\"code\":\"esc\"}";
        assert_eq!(
            exchange(
                port,
                &format!(
                    "POST /input HTTP/1.1\r\n{}X-Overseer-Token: {TOKEN}\r\nContent-Length: {}\r\n\r\n{body}",
                    host(port),
                    body.len()
                )
            ),
            "HTTP/1.1 204 No Content"
        );
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(CtEvent::Key(_))
        ));
        // The proxy-safe GET fallback carries t= in the URL.
        assert_eq!(
            exchange(
                port,
                &format!(
                    "GET /input?t={TOKEN}&d=%7B%22type%22%3A%22key%22%2C%22code%22%3A%22esc%22%7D HTTP/1.1\r\n{}\r\n",
                    host(port)
                )
            ),
            "HTTP/1.1 204 No Content"
        );
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(CtEvent::Key(_))
        ));
    }

    #[test]
    fn bad_host_is_rejected() {
        let (port, _rx, _ctx) = server();
        assert_eq!(
            exchange(port, "GET / HTTP/1.1\r\nHost: evil.example.com\r\n\r\n"),
            "HTTP/1.1 421 Misdirected Request"
        );
        assert_eq!(
            exchange(port, "GET / HTTP/1.1\r\n\r\n"),
            "HTTP/1.1 421 Misdirected Request"
        );
        assert_eq!(
            exchange(
                port,
                &format!("GET / HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n")
            ),
            "HTTP/1.1 200 OK"
        );
    }

    #[test]
    fn cross_site_metadata_is_rejected() {
        let (port, _rx, _ctx) = server();
        assert_eq!(
            exchange(
                port,
                &format!(
                    "GET /input?t={TOKEN}&d=%7B%22type%22%3A%22esc%22%7D HTTP/1.1\r\n{}Sec-Fetch-Site: cross-site\r\n\r\n",
                    host(port)
                )
            ),
            "HTTP/1.1 403 Forbidden"
        );
        assert_eq!(
            exchange(
                port,
                &format!(
                    "GET /events?t={TOKEN} HTTP/1.1\r\n{}Origin: http://evil.example.com\r\n\r\n",
                    host(port)
                )
            ),
            "HTTP/1.1 403 Forbidden"
        );
        // same-origin metadata passes the metadata gate (token still applies).
        assert_eq!(
            exchange(
                port,
                &format!(
                    "GET /events?t={TOKEN} HTTP/1.1\r\n{}Sec-Fetch-Site: same-origin\r\nOrigin: http://127.0.0.1:{port}\r\n\r\n",
                    host(port)
                )
            ),
            "HTTP/1.1 200 OK"
        );
    }

    #[test]
    fn oversize_body_is_413() {
        let (port, _rx, _ctx) = server();
        assert_eq!(
            exchange(
                port,
                &format!(
                    "POST /input HTTP/1.1\r\n{}X-Overseer-Token: {TOKEN}\r\nContent-Length: {}\r\n\r\n",
                    host(port),
                    BODY_CAP + 1
                )
            ),
            "HTTP/1.1 413 Content Too Large"
        );
    }

    #[test]
    fn connection_cap_returns_503() {
        let (port, _rx, _ctx) = server();
        // Fill every slot with a live authed SSE connection — the
        // writer thread holds the slot while the socket is open.
        let mut held = Vec::new();
        for _ in 0..MAX_CONNS {
            let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            held.push(s);
            // No request needed: the slot is claimed at accept time.
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            exchange(port, &format!("GET / HTTP/1.1\r\n{}\r\n", host(port))),
            "HTTP/1.1 503 Service Unavailable"
        );
        drop(held);
    }

    #[test]
    fn stalled_sse_client_never_stalls_input() {
        let (port, rx, _ctx) = server();
        // A client that never drains its socket: its channel fills,
        // frames drop — and nobody else's request waits on it.
        let _stalled = {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(format!("GET /events?t={TOKEN} HTTP/1.1\r\n{}\r\n", host(port)).as_bytes())
                .unwrap();
            s
        };
        let body = "{\"type\":\"key\",\"code\":\"esc\"}";
        assert_eq!(
            exchange(
                port,
                &format!(
                    "POST /input HTTP/1.1\r\n{}X-Overseer-Token: {TOKEN}\r\nContent-Length: {}\r\n\r\n{body}",
                    host(port),
                    body.len()
                )
            ),
            "HTTP/1.1 204 No Content"
        );
        assert!(rx.recv_timeout(std::time::Duration::from_secs(2)).is_ok());
    }

    #[test]
    fn short_body_is_400() {
        let (port, _rx, _ctx) = server();
        assert_eq!(
            exchange(
                port,
                &format!(
                    "POST /input HTTP/1.1\r\n{}X-Overseer-Token: {TOKEN}\r\nContent-Length: 100\r\n\r\n{{}}",
                    host(port)
                )
            ),
            "HTTP/1.1 400 Bad Request"
        );
    }

    #[test]
    fn newline_less_line_is_bounded_to_431() {
        let (port, _rx, _ctx) = server();
        // One 20 KiB header line with no '\n' — must hit the 431 cap
        // before any over-cap allocation.
        let big = "A".repeat(HEAD_CAP + 4096);
        assert_eq!(
            exchange(port, &format!("GET /{big} HTTP/1.1\r\n{}\r\n", host(port))),
            "HTTP/1.1 431 Request Header Fields Too Large"
        );
        // And the server still answers afterwards (head read released).
        assert_eq!(
            exchange(port, &format!("GET / HTTP/1.1\r\n{}\r\n", host(port))),
            "HTTP/1.1 200 OK"
        );
    }

    #[test]
    fn head_deadline_kills_dribblers() {
        // A dribbler that keeps reads succeeding (a byte every 30 ms)
        // never trips the per-read socket timeout — only the
        // wall-clock head deadline hangs it up. `read_request_within`
        // takes the deadline so the test uses a short clock while the
        // client is still mid-stream.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (srv, _) = listener.accept().unwrap();
            // Per-read timeout far past the deadline: only the
            // deadline can end this.
            let _ = srv.set_read_timeout(Some(std::time::Duration::from_secs(60)));
            let r = read_request_within(
                &srv,
                std::time::Instant::now() + std::time::Duration::from_millis(100),
            );
            done_tx.send(matches!(r, Err(HeadErr::Silent))).unwrap();
        });
        let dribble = std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            for _ in 0..20 {
                // ~600 ms of trickle — well past the 100 ms deadline.
                if c.write_all(b"x").is_err() {
                    break; // server already hung up
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
        });
        assert!(done_rx
            .recv_timeout(std::time::Duration::from_millis(400))
            .unwrap());
        dribble.join().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn web_token_reasserts_perms_and_rejects_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!("ovw-home-{}", std::process::id()));
        std::env::set_var("HOME", &home);
        let dir = home.join(".overseer").join("web");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");

        // Lax perms on an existing token → re-asserted to 0600, reused.
        std::fs::write(&path, TOKEN).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let t = web_token().unwrap();
        assert_eq!(t, TOKEN);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // A planted symlink is never trusted — regenerated in place.
        std::fs::remove_file(&path).unwrap();
        let target = home.join("target.txt");
        std::fs::write(&target, TOKEN).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let t2 = web_token().unwrap();
        assert_ne!(t2, TOKEN);
        assert_eq!(t2.len(), 64);
        assert!(!std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());

        std::env::remove_var("HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn port_fallback_scans_and_pin_fails() {
        // Pin: binding a port someone already holds must fail.
        let held = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let p = held.local_addr().unwrap().port();
        assert!(bind_port(Some(p)).is_err());
        // Auto: lands inside the range; if the default is free to hold
        // here, the pick must skip it.
        let hold_default = TcpListener::bind(("127.0.0.1", *PORT_RANGE.start())).ok();
        let l = bind_port(None).unwrap();
        let p = l.local_addr().unwrap().port();
        assert!(PORT_RANGE.contains(&p));
        if hold_default.is_some() {
            assert!(p > *PORT_RANGE.start());
        }
    }

    #[test]
    fn auto_open_policy() {
        // never opens when asked not to, or over SSH
        assert!(!auto_open_ok_env(false, false, true, "macos"));
        assert!(!auto_open_ok_env(true, true, true, "macos"));
        // headless Linux: no display → no opener
        assert!(!auto_open_ok_env(true, false, false, "linux"));
        assert!(auto_open_ok_env(true, false, true, "linux"));
        assert!(auto_open_ok_env(true, false, false, "macos"));
        // other platforms: no opener defined
        assert!(!auto_open_ok_env(true, false, true, "windows"));
    }

    #[test]
    fn token_compare_is_exact() {
        assert!(token_eq(b"abc", b"abc"));
        assert!(!token_eq(b"abc", b"abd"));
        assert!(!token_eq(b"abc", b"ab"));
        assert!(!token_eq(b"", b"x"));
    }

    #[test]
    fn frame_json_trims_trailing_blanks() {
        use ratatui::widgets::Paragraph;
        let backend = TestBackend::new(10, 3);
        let mut term = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )
        .unwrap();
        term.draw(|f| f.render_widget(Paragraph::new("hi"), f.area()))
            .unwrap();
        let f = frame_json(&mut term, 2, false);
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v["w"], 10);
        assert_eq!(v["h"], 3);
        assert_eq!(v["p"], 2);
        // "hi" then padding trimmed — the row ends at the glyph.
        assert_eq!(v["rows"][0], serde_json::json!([{ "t": "hi" }]));
        assert_eq!(v["rows"][1], serde_json::json!([]));
    }
}
