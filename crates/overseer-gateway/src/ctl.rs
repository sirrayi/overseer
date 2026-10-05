//! Control plane — a unix-domain socket carrying NDJSON requests, the
//! same discipline as overseer-proto: one line in, one line out. The
//! socket is how frontends (CLI today; TUI/desktop/messaging later)
//! reach the daemon without sharing process state.
//!
//! Reachable commands: status, kill, inbox.list, inbox.decide,
//! trigger.fire, reload, and (P7) channel.send, digest.get,
//! desktop.signal. There is deliberately **no** config.patch — config
//! mutation is not a socket surface (OpenClaw lesson).
//!
//! Method strings are a wire contract: each variant's `rename` is pinned by
//! the round-trip test below. Adding is allowed; renaming silently breaks
//! every frontend, so it is not.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum CtlRequest {
    /// Daemon liveness + counters.
    Status,
    /// Kill switch — daemon exits at next tick.
    Kill,
    /// List inbox items.
    InboxList,
    /// approve | reject | snooze an item.
    InboxDecide {
        id: String,
        decision: String,
        #[serde(default)]
        snooze_ms: Option<u64>,
    },
    /// Inject an event manually (testing, webhooks-via-CLI).
    TriggerFire {
        source: String,
        class: String,
        payload: String,
    },
    /// Re-read config.json (mtime check also catches edits each tick).
    Reload,
    /// Approve-and-act: mark acted + spawn the item's prompt.
    InboxAct { id: String },
    /// P7-5: queue an outbound channel message. This only ever *drafts* the
    /// message — approval happens through inbox.decide/inbox.act, so the
    /// ladder's external → Ask is never bypassed by a socket call.
    ChannelSend {
        to: String,
        #[serde(default)]
        thread: Option<String>,
        text: String,
        /// Transport ("local" = the daemon's own outbox log).
        #[serde(default)]
        channel: Option<String>,
    },
    /// P7-5: the attention digest as cards (a view over the inbox).
    DigestGet,
    /// P7-6: the desktop's attention state, pushed by the frontend each
    /// tick. Facts only — the daemon never senses anything itself.
    DesktopSignal {
        focused: bool,
        dnd: bool,
        calendar_busy: bool,
        #[serde(default)]
        active_app: Option<String>,
        #[serde(default)]
        idle_s: Option<u64>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CtlResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl CtlResponse {
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            error: None,
            data: Some(data),
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            data: None,
        }
    }
}

/// `sockaddr_un.sun_path` capacity: 104 on macOS/BSD, 108 on Linux.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
pub const SUN_PATH_MAX: usize = 104;
#[cfg(target_os = "linux")]
pub const SUN_PATH_MAX: usize = 108;
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "linux"
)))]
pub const SUN_PATH_MAX: usize = 104;

/// `sun_path` is fixed-size; refuse a path that can never bind with a
/// clear message instead of libc's opaque failure.
pub fn check_socket_path(path: &std::path::Path) -> std::io::Result<()> {
    let n = path.as_os_str().len();
    if n >= SUN_PATH_MAX {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "control socket path too long ({n} bytes > {SUN_PATH_MAX}): {} — set a shorter daemon dir",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Spawn the listener thread. Requests are pushed to `tx` (the daemon
/// loop owns all state — the socket thread is just plumbing).
pub fn listen(
    sock_path: PathBuf,
    tx: std::sync::mpsc::Sender<(CtlRequest, std::sync::mpsc::Sender<CtlResponse>)>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    check_socket_path(&sock_path)?;
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;
    // Socket is user-only by default on macOS/Linux (0700 dir handles it).
    std::thread::Builder::new()
        .name("ctl-listener".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let tx = tx.clone();
                std::thread::spawn(move || handle_conn(&mut s, tx));
            }
        })
}

/// Longest accepted request line. Every legitimate request is tiny; the
/// cap stops one client from growing the daemon's memory without bound.
pub const MAX_REQUEST_LINE: usize = 64 * 1024;

/// Read one newline-terminated request of at most [`MAX_REQUEST_LINE`]
/// bytes. Never buffers more than the cap plus one byte.
fn read_request_line(r: impl Read) -> Result<String, String> {
    let mut buf = Vec::new();
    BufReader::new(r.take(MAX_REQUEST_LINE as u64 + 1))
        .read_until(b'\n', &mut buf)
        .map_err(|e| format!("read: {e}"))?;
    if buf.len() > MAX_REQUEST_LINE {
        return Err("request line exceeds 64 KiB".into());
    }
    String::from_utf8(buf).map_err(|_| "request is not UTF-8".into())
}

fn handle_conn(
    s: &mut UnixStream,
    tx: std::sync::mpsc::Sender<(CtlRequest, std::sync::mpsc::Sender<CtlResponse>)>,
) {
    let resp = match read_request_line(&*s) {
        Err(e) => CtlResponse::err(e),
        Ok(line) => match serde_json::from_str::<CtlRequest>(&line) {
            Ok(req) => {
                let (rtx, rrx) = std::sync::mpsc::channel();
                if tx.send((req, rtx)).is_err() {
                    CtlResponse::err("daemon loop gone")
                } else {
                    rrx.recv().unwrap_or_else(|_| CtlResponse::err("no reply"))
                }
            }
            Err(e) => CtlResponse::err(format!("bad request: {e}")),
        },
    };
    if let Ok(mut out) = serde_json::to_string(&resp) {
        out.push('\n');
        let _ = s.write_all(out.as_bytes());
    }
}

/// Client side: send one request, read one response.
pub fn call(sock_path: &std::path::Path, req: &CtlRequest) -> Result<CtlResponse, String> {
    let mut s = UnixStream::connect(sock_path).map_err(|e| format!("connect: {e}"))?;
    let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    line.push('\n');
    s.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(s)
        .read_line(&mut reply)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&reply).map_err(|e| format!("bad reply: {e}"))
}

#[cfg(test)]
mod ctl_serde_tests {
    use super::*;

    fn serve_one(payload: Vec<u8>, close_write: bool) -> CtlResponse {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        let srv = std::thread::spawn(move || handle_conn(&mut server, tx));
        let writer = std::thread::spawn(move || {
            let _ = client.write_all(&payload);
            if close_write {
                let _ = client.shutdown(std::net::Shutdown::Write);
            }
            let mut reply = String::new();
            let _ = BufReader::new(&client).read_line(&mut reply);
            reply
        });
        srv.join().unwrap();
        let reply = writer.join().unwrap();
        serde_json::from_str(&reply).unwrap_or_else(|e| panic!("reply {reply:?}: {e}"))
    }

    /// A daemon dir under a long TMPDIR (the self-hosted runner) must
    /// fail up front with a nameable cause, not libc's opaque bind error.
    #[test]
    fn socket_path_longer_than_sun_path_is_refused_up_front() {
        let long = std::path::PathBuf::from(format!("/{}", "d".repeat(SUN_PATH_MAX + 10)));
        let (tx, _rx) = std::sync::mpsc::channel();
        let e = listen(long, tx).unwrap_err().to_string();
        assert!(e.contains("control socket path too long"), "{e}");
        assert!(e.contains(&SUN_PATH_MAX.to_string()), "{e}");
        assert!(e.contains("set a shorter daemon dir"), "{e}");
        // A path at the limit still refuses — sun_path needs the NUL.
        let edge = std::path::PathBuf::from(format!("/{}", "d".repeat(SUN_PATH_MAX - 2)));
        assert!(check_socket_path(&edge).is_ok());
        let at = std::path::PathBuf::from(format!("/{}", "d".repeat(SUN_PATH_MAX)));
        assert!(check_socket_path(&at).is_err());
    }

    #[test]
    fn oversized_request_line_is_refused() {
        let resp = serve_one(vec![b'a'; MAX_REQUEST_LINE + 4096], true);
        assert!(!resp.ok);
        let err = resp.error.unwrap_or_default();
        assert!(err.contains("64 KiB"), "{err}");
    }

    #[test]
    fn a_request_that_fits_the_bound_is_still_parsed() {
        let mut line = serde_json::to_vec(&CtlRequest::Status).unwrap();
        line.push(b'\n');
        // The receiver is dropped, so a parsed request reports the loop gone.
        let resp = serve_one(line, false);
        assert_eq!(resp.error.as_deref(), Some("daemon loop gone"));
    }

    #[test]
    fn an_endless_line_stops_at_the_cap() {
        // `repeat` never ends and never sends a newline; the reader must
        // still return after consuming just past the cap.
        let err = read_request_line(std::io::repeat(b'a')).unwrap_err();
        assert_eq!(err, "request line exceeds 64 KiB");
        let exact = [vec![b'a'; MAX_REQUEST_LINE - 1], vec![b'\n']].concat();
        assert_eq!(
            read_request_line(&exact[..]).unwrap().len(),
            MAX_REQUEST_LINE
        );
    }

    /// Every reachable command must survive an NDJSON round-trip (one
    /// line in, one line out — the same discipline as overseer-proto).
    #[test]
    fn requests_round_trip() {
        let reqs = vec![
            CtlRequest::Status,
            CtlRequest::Kill,
            CtlRequest::Reload,
            CtlRequest::InboxList,
            CtlRequest::InboxDecide {
                id: "abc123".into(),
                decision: "approve".into(),
                snooze_ms: None,
            },
            CtlRequest::InboxDecide {
                id: "abc123".into(),
                decision: "snooze".into(),
                snooze_ms: Some(60_000),
            },
            CtlRequest::InboxAct {
                id: "abc123".into(),
            },
            CtlRequest::TriggerFire {
                source: "cli".into(),
                class: "note.low".into(),
                payload: "hello".into(),
            },
        ];
        for req in reqs {
            let mut line = serde_json::to_string(&req).expect("serialize");
            line.push('\n');
            let back: CtlRequest = serde_json::from_str(line.trim()).expect("parse");
            let again = serde_json::to_value(&back).expect("value");
            assert_eq!(again, serde_json::to_value(&req).expect("value"));
        }
    }

    /// Wire contract: every method string, in order, verbatim. Renaming one
    /// breaks every frontend silently — this test is the alarm.
    #[test]
    fn every_method_tag_is_pinned() {
        let cases: Vec<(CtlRequest, &str)> = vec![
            (CtlRequest::Status, "status"),
            (CtlRequest::Kill, "kill"),
            (CtlRequest::InboxList, "inbox_list"),
            (
                CtlRequest::InboxDecide {
                    id: "x".into(),
                    decision: "approve".into(),
                    snooze_ms: None,
                },
                "inbox_decide",
            ),
            (
                CtlRequest::TriggerFire {
                    source: "s".into(),
                    class: "c".into(),
                    payload: "p".into(),
                },
                "trigger_fire",
            ),
            (CtlRequest::Reload, "reload"),
            (CtlRequest::InboxAct { id: "x".into() }, "inbox_act"),
            (
                CtlRequest::ChannelSend {
                    to: "ops".into(),
                    thread: None,
                    text: "hello".into(),
                    channel: None,
                },
                "channel_send",
            ),
            (CtlRequest::DigestGet, "digest_get"),
            (
                CtlRequest::DesktopSignal {
                    focused: true,
                    dnd: false,
                    calendar_busy: false,
                    active_app: Some("iTerm".into()),
                    idle_s: Some(3),
                },
                "desktop_signal",
            ),
        ];
        for (req, tag) in cases {
            let v = serde_json::to_value(&req).unwrap();
            assert_eq!(v["method"], tag, "tag for {req:?}");
            // …and it round-trips from the wire form.
            let back: CtlRequest = serde_json::from_str(&v.to_string()).unwrap();
            assert_eq!(serde_json::to_value(&back).unwrap(), v);
        }
        // The optional fields really are optional on the wire.
        let minimal: CtlRequest =
            serde_json::from_str(r#"{"method":"channel_send","to":"ops","text":"hi"}"#).unwrap();
        let signal: CtlRequest = serde_json::from_str(
            r#"{"method":"desktop_signal","focused":true,"dnd":false,"calendar_busy":false}"#,
        )
        .unwrap();
        assert!(matches!(
            signal,
            CtlRequest::DesktopSignal {
                active_app: None,
                idle_s: None,
                ..
            }
        ));
        assert!(matches!(
            minimal,
            CtlRequest::ChannelSend {
                thread: None,
                channel: None,
                ..
            }
        ));
    }

    #[test]
    fn response_ok_err_shapes() {
        let ok = CtlResponse::ok(serde_json::json!({"fired": true}));
        let v = serde_json::to_value(&ok).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["fired"], true);
        assert!(v.get("error").is_none());

        let err = CtlResponse::err("nope");
        let back: CtlResponse =
            serde_json::from_str(&serde_json::to_string(&err).unwrap()).unwrap();
        assert!(!back.ok);
        assert_eq!(back.error.as_deref(), Some("nope"));
        assert!(back.data.is_none());
    }

    #[test]
    fn malformed_json_is_err() {
        for bad in [
            "",
            "{ not json",
            serde_json::json!({"method": "no_such_command"})
                .to_string()
                .as_str(),
            serde_json::json!({"method": "inbox_decide", "id": "x"})
                .to_string()
                .as_str(),
        ] {
            assert!(
                serde_json::from_str::<CtlRequest>(bad).is_err(),
                "expected Err for {bad:?}"
            );
        }
    }
}
