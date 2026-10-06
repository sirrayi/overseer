//! Audit (test/audit-surfaces): `overseer web` attacked over raw TCP.
//! Each test boots the real binary on an ephemeral port with a temp
//! HOME and a dead provider. Tests marked `#[ignore = "audit: ..."]`
//! reproduce a defect on b9eae9b; the rest record defenses that held.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_overseer");

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "audit-web-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Web {
    child: Child,
    port: u16,
    token: String,
    home: PathBuf,
    stderr: Arc<Mutex<String>>,
}

impl Drop for Web {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn boot(extra: &[&str]) -> Web {
    let home = tmp("home");
    let mut child = Command::new(BIN)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("OPENAI_API_KEY", "sk-audit-dummy")
        .args([
            "web",
            "--no-open",
            "--port",
            "0",
            "--provider",
            "openai",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--model",
            "m",
        ])
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut r = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    let url = loop {
        line.clear();
        assert!(r.read_line(&mut line).unwrap() > 0, "web exited early");
        if let Some(u) = line.trim().strip_prefix("overseer web: ") {
            break u.to_string();
        }
    };
    let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, token) = rest.split_once("/#t=").unwrap();
    let stderr = Arc::new(Mutex::new(String::new()));
    let sink = stderr.clone();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = r.read_to_string(&mut s);
        sink.lock().unwrap().push_str(&s);
    });
    Web {
        child,
        port: port.parse().unwrap(),
        token: token.to_string(),
        home,
        stderr,
    }
}

impl Web {
    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
    fn raw(&self, req: &[u8], wait: Duration) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(wait)).unwrap();
        let _ = s.write_all(req);
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if out.len() > 64 * 1024 {
                        break;
                    }
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    fn get(&self, path: &str, headers: &str) -> String {
        let req = format!("GET {path} HTTP/1.1\r\n{headers}\r\n");
        self.raw(req.as_bytes(), Duration::from_secs(2))
    }
    fn status(&self, path: &str, headers: &str) -> String {
        status_of(&self.get(path, headers))
    }
    fn input(&self, body: &str) -> String {
        let req = format!(
            "POST /input HTTP/1.1\r\nHost: {}\r\nX-Overseer-Token: {}\r\nContent-Length: {}\r\n\r\n{body}",
            self.host(),
            self.token,
            body.len()
        );
        status_of(&self.raw(req.as_bytes(), Duration::from_secs(3)))
    }
    fn alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
}

fn status_of(resp: &str) -> String {
    resp.lines().next().unwrap_or("").to_string()
}

// ── Host / Origin / Sec-Fetch-Site ──────────────────────────────────────

#[test]
fn host_header_matrix_blocks_rebinding() {
    let w = boot(&["--no-memory"]);
    let p = w.port;
    for bad in [
        format!("Host: 127.0.0.1.nip.io:{p}\r\n"),
        format!("Host: localhost.:{p}\r\n"),
        format!("Host: 127.0.0.1.:{p}\r\n"),
        "Host: 127.0.0.1\r\n".to_string(),
        "Host: 127.0.0.1:1\r\n".to_string(),
        format!("Host: evil.example:{p}\r\n"),
        format!("Host: 127.0.0.1:{p}\r\nHost: evil.example\r\n"),
        String::new(),
    ] {
        assert!(w.status("/", &bad).contains("421"), "accepted {bad:?}");
    }
    for ok in [
        format!("Host: 127.0.0.1:{p}\r\n"),
        format!("Host: localhost:{p}\r\n"),
        format!("Host: LOCALHOST:{p}\r\n"),
    ] {
        assert!(w.status("/", &ok).contains("200"), "refused {ok:?}");
    }
}

#[test]
fn cross_site_origin_and_fetch_metadata_are_refused() {
    let w = boot(&["--no-memory"]);
    let h = format!("Host: {}\r\n", w.host());
    let t = &w.token;
    for extra in [
        "Origin: http://evil.example\r\n",
        "Origin: null\r\n",
        "Sec-Fetch-Site: cross-site\r\n",
        "Sec-Fetch-Site: same-site\r\n",
    ] {
        let s = w.status(&format!("/events?t={t}"), &format!("{h}{extra}"));
        assert!(s.contains("403"), "{extra:?} -> {s}");
        let body = r#"{"type":"focus","gained":true}"#;
        let req = format!(
            "POST /input?t={t} HTTP/1.1\r\n{h}{extra}Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let s = status_of(&w.raw(req.as_bytes(), Duration::from_secs(2)));
        assert!(s.contains("403"), "POST {extra:?} -> {s}");
    }
}

// ── tokens ──────────────────────────────────────────────────────────────

#[test]
fn token_must_match_exactly() {
    let w = boot(&["--no-memory"]);
    let h = format!("Host: {}\r\n", w.host());
    let t = w.token.clone();
    for bad in [
        String::new(),
        t[..10].to_string(),
        t[..t.len() - 1].to_string(),
        format!("{t}0"),
        t.to_uppercase(),
        format!("{t}%00"),
        "x".repeat(10_000),
    ] {
        let s = w.status(&format!("/events?t={bad}"), &h);
        assert!(
            s.contains("401"),
            "token {:?}… -> {s}",
            &bad[..bad.len().min(12)]
        );
    }
    assert!(w.status("/events", &h).contains("401"));
    assert!(w
        .status("/events", &format!("{h}X-Overseer-Token: {t}\r\n"))
        .contains("200"));
    // The header is authoritative: a wrong header is not rescued by ?t=.
    assert!(w
        .status(
            &format!("/events?t={t}"),
            &format!("{h}X-Overseer-Token: nope\r\n")
        )
        .contains("401"));
}

/// A malformed `%` escape followed by a multi-byte char panics in
/// `url_decode` (byte-slicing a &str) before the token check: an
/// unauthenticated local client gets a dropped socket and a panic dump
/// on the user's terminal instead of a 401.
#[test]
fn malformed_percent_escape_gets_401_not_a_panic() {
    let w = boot(&["--no-memory"]);
    let req = format!(
        "GET /events?t=%a\u{e9} HTTP/1.1\r\nHost: {}\r\n\r\n",
        w.host()
    );
    let s = status_of(&w.raw(req.as_bytes(), Duration::from_secs(2)));
    std::thread::sleep(Duration::from_millis(200));
    let err = w.stderr.lock().unwrap().clone();
    assert!(s.contains("401"), "got {s:?}; stderr: {err}");
    assert!(!err.contains("panicked"), "{err}");
}

#[test]
fn token_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let w = boot(&["--no-memory"]);
    let dir = w.home.join(".overseer").join("web");
    let f = std::fs::metadata(dir.join("token")).unwrap();
    let d = std::fs::metadata(&dir).unwrap();
    assert_eq!(f.permissions().mode() & 0o777, 0o600);
    assert_eq!(d.permissions().mode() & 0o777, 0o700);
    assert_eq!(
        std::fs::read_to_string(dir.join("token")).unwrap().trim(),
        w.token
    );
}

/// `--bare` is documented as hermetic, but `web --bare` still writes
/// `~/.overseer/web/token`.
#[test]
fn bare_web_does_not_touch_home_overseer() {
    let w = boot(&["--bare"]);
    assert!(
        !w.home.join(".overseer").exists(),
        "--bare created ~/.overseer"
    );
}

// ── static files / headers ─────────────────────────────────────────────

#[test]
fn static_routes_resist_traversal_and_carry_security_headers() {
    let w = boot(&["--no-memory"]);
    let h = format!("Host: {}\r\n", w.host());
    for p in [
        "/../Cargo.toml",
        "/%2e%2e/Cargo.toml",
        "/app.js/../../Cargo.toml",
        "//etc/passwd",
        "/web/app.js",
        "/src/web.rs",
        "/APP.JS",
        "/app.js%00",
        "/..%2fCargo.toml",
    ] {
        assert!(w.status(p, &h).contains("404"), "{p}");
    }
    for (p, ct) in [
        ("/", "text/html"),
        ("/app.js", "text/javascript"),
        ("/style.css", "text/css"),
        ("/mark.svg", "image/svg+xml"),
    ] {
        let r = w.get(p, &h);
        let head = r.split("\r\n\r\n").next().unwrap().to_ascii_lowercase();
        assert!(head.starts_with("http/1.1 200"), "{p}: {head}");
        assert!(head.contains(&format!("content-type: {ct}")), "{p}: {head}");
        assert!(head.contains("x-content-type-options: nosniff"), "{p}");
        assert!(
            head.contains("content-security-policy: default-src 'self'"),
            "{p}"
        );
        assert!(head.contains("frame-ancestors 'none'"), "{p}");
        assert!(head.contains("referrer-policy: no-referrer"), "{p}");
    }
    assert!(w.status("/", "").contains("421"));
    let head = w.raw(
        format!("HEAD / HTTP/1.1\r\n{h}\r\n").as_bytes(),
        Duration::from_secs(2),
    );
    assert!(status_of(&head).contains("405"));
}

// ── framing / limits ───────────────────────────────────────────────────

#[test]
fn head_and_body_limits_hold() {
    let w = boot(&["--no-memory"]);
    let h = format!("Host: {}\r\n", w.host());
    let big = format!(
        "GET / HTTP/1.1\r\n{h}X-Pad: {}\r\n\r\n",
        "a".repeat(20 * 1024)
    );
    assert!(status_of(&w.raw(big.as_bytes(), Duration::from_secs(2))).contains("431"));
    let lie = format!(
        "POST /input HTTP/1.1\r\n{h}X-Overseer-Token: {}\r\nContent-Length: 999999999\r\n\r\n",
        w.token
    );
    assert!(status_of(&w.raw(lie.as_bytes(), Duration::from_secs(2))).contains("413"));
    let dup = format!(
        "POST /input HTTP/1.1\r\n{h}X-Overseer-Token: {}\r\nContent-Length: 2\r\nContent-Length: 30\r\n\r\n{{}}",
        w.token
    );
    assert!(status_of(&w.raw(dup.as_bytes(), Duration::from_secs(2))).contains("400"));
    let chunked = format!(
        "POST /input HTTP/1.1\r\n{h}X-Overseer-Token: {}\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{{}}\r\n0\r\n\r\n",
        w.token
    );
    assert!(status_of(&w.raw(chunked.as_bytes(), Duration::from_secs(2))).contains("400"));
    // Pipelining: one response per connection, the second request is dropped.
    let two = format!("GET /mark.svg HTTP/1.1\r\n{h}\r\nGET /mark.svg HTTP/1.1\r\n{h}\r\n");
    assert_eq!(
        w.raw(two.as_bytes(), Duration::from_secs(2))
            .matches("HTTP/1.1 ")
            .count(),
        1
    );
}

/// RFC 9112 §6.1: a request with both Transfer-Encoding and
/// Content-Length must be rejected (the classic CL.TE smuggling shape);
/// a signed Content-Length is invalid. Both are accepted today.
#[test]
fn te_plus_cl_and_signed_cl_are_rejected() {
    let w = boot(&["--no-memory"]);
    let h = format!("Host: {}\r\n", w.host());
    let body = r#"{"type":"focus","gained":true}"#;
    let te_cl = format!(
        "POST /input HTTP/1.1\r\n{h}X-Overseer-Token: {}\r\nTransfer-Encoding: chunked\r\nContent-Length: {}\r\n\r\n{body}",
        w.token,
        body.len()
    );
    let s1 = status_of(&w.raw(te_cl.as_bytes(), Duration::from_secs(2)));
    let signed = format!(
        "POST /input HTTP/1.1\r\n{h}X-Overseer-Token: {}\r\nContent-Length: +{}\r\n\r\n{body}",
        w.token,
        body.len()
    );
    let s2 = status_of(&w.raw(signed.as_bytes(), Duration::from_secs(2)));
    assert!(s1.contains("400"), "TE+CL -> {s1}");
    assert!(s2.contains("400"), "CL:+N -> {s2}");
}

/// The 10 s head deadline is checked only between reads and each read
/// may block a full READ_TIMEOUT, so a 1-byte-per-9 s dribbler holds a
/// slot ~18 s.
#[test]
fn head_deadline_is_wall_clock() {
    let w = boot(&["--no-memory"]);
    let mut s = TcpStream::connect(("127.0.0.1", w.port)).unwrap();
    let t = Instant::now();
    let msg = format!("GET / HTTP/1.1\r\nHost: {}\r\nX-Pad: aaaaaaaa", w.host());
    let mut closed_at = None;
    for b in msg.bytes() {
        if s.write_all(&[b]).is_err() {
            closed_at = Some(t.elapsed());
            break;
        }
        s.set_read_timeout(Some(Duration::from_secs(9))).unwrap();
        let mut buf = [0u8; 64];
        match s.read(&mut buf) {
            Ok(_) => {
                closed_at = Some(t.elapsed());
                break;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => {
                closed_at = Some(t.elapsed());
                break;
            }
        }
        if t.elapsed() > Duration::from_secs(30) {
            break;
        }
    }
    let c = closed_at.expect("never closed");
    assert!(c < Duration::from_secs(12), "slot held for {c:?}");
}

#[test]
fn connection_cap_refuses_the_33rd_and_recovers() {
    let w = boot(&["--no-memory"]);
    let h = format!("Host: {}\r\n", w.host());
    let mut held = Vec::new();
    for _ in 0..32 {
        let mut s = TcpStream::connect(("127.0.0.1", w.port)).unwrap();
        s.write_all(b"GET / HTTP/1.1\r\n").unwrap();
        held.push(s);
    }
    std::thread::sleep(Duration::from_millis(300));
    let s = w.status("/", &h);
    assert!(s.contains("503"), "33rd connection got {s}");
    drop(held);
    std::thread::sleep(Duration::from_millis(300));
    assert!(w.status("/", &h).contains("200"));
}

// ── authenticated input robustness ─────────────────────────────────────

/// `{"type":"resize","cols":0,...}` (or 65536, truncated by `as u16`)
/// reaches `frame_json`'s `chunks(0)` and panics the main loop: the whole
/// web session dies.
#[test]
fn zero_resize_does_not_kill_the_session() {
    let mut w = boot(&["--no-memory"]);
    let s = w.input(r#"{"type":"resize","cols":65536,"rows":30}"#);
    assert!(s.contains("204"), "{s}");
    std::thread::sleep(Duration::from_millis(800));
    let err = w.stderr.lock().unwrap().clone();
    assert!(w.alive(), "web process died: {err}");
    assert!(w
        .status("/", &format!("Host: {}\r\n", w.host()))
        .contains("200"));
}

#[cfg(target_os = "linux")]
fn rss_kb(pid: u32) -> u64 {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    s.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

/// Resize dimensions are unbounded: 2000×2000 (4 M cells) costs ~750 MB
/// RSS; 65535×65535 would ask for ~4.3 G cells.
#[test]
#[cfg(target_os = "linux")]
fn resize_dimensions_are_bounded() {
    let mut w = boot(&["--no-memory"]);
    let pid = w.child.id();
    let before = rss_kb(pid);
    assert!(w
        .input(r#"{"type":"resize","cols":2000,"rows":2000}"#)
        .contains("204"));
    std::thread::sleep(Duration::from_secs(4));
    let after = rss_kb(pid);
    assert!(w.alive());
    let grew_mb = after.saturating_sub(before) / 1024;
    assert!(
        grew_mb < 128,
        "resize grew RSS by {grew_mb} MB ({before} -> {after} kB)"
    );
}

#[test]
fn authenticated_garbage_input_is_harmless() {
    let mut w = boot(&["--no-memory"]);
    for b in [
        "",
        "{",
        "null",
        "[]",
        r#"{"type":"key"}"#,
        r#"{"type":"key","code":"char","ch":""}"#,
        r#"{"type":"key","code":"char","ch":"\u0000"}"#,
        r#"{"type":"click","col":-5,"row":99999999999}"#,
        r#"{"type":"scroll","up":"yes"}"#,
        r#"{"type":"paste","text":"\u001b]52;c;eA==\u0007"}"#,
        r#"{"type":"resize","cols":40,"rows":10}"#,
    ] {
        let s = w.input(b);
        assert!(s.contains("204") || s.contains("400"), "{b:?} -> {s}");
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(w.alive(), "{}", w.stderr.lock().unwrap());
}
