//! Terminal capability probe (P2.1, playbook Ch.9 §3.2).
//!
//! Capabilities are *probed through the pty*, never guessed from env alone:
//! sshd forwards only TERM, so TERM_PROGRAM/KITTY_WINDOW_ID/WT_SESSION all
//! die at the boundary — env-only detection silently disables sync output
//! for every SSH user (the documented flicker case).
//!
//! Probes: DECRQM for DECSET-2026 (sync output), XTVERSION, DA1. A
//! multiplexer is never upgraded to sync — muxes re-chunk the stream and
//! the atomic-frame guarantee is lost regardless of what the pane reports.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// Begin-synchronized-update / end-synchronized-update (DECSET 2026).
pub const BSU: &str = "\x1b[?2026h";
pub const ESU: &str = "\x1b[?2026l";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorDepth {
    Mono,
    #[default]
    Ansi16,
    Ansi256,
    TrueColor,
}

#[derive(Debug, Clone, Default)]
pub struct Caps {
    /// Terminal holds rendering between BSU/ESU → atomic frame flips.
    pub sync_output: bool,
    pub color: ColorDepth,
    /// XTVERSION response when the terminal answered.
    pub term_version: Option<String>,
    /// Inside tmux/screen — sync output stays off (re-chunking).
    pub mux: bool,
}

/// Query the terminal. Call with raw mode already enabled and before the
/// UI owns stdin — responses arrive as escape sequences on stdin, which a
/// reader thread drains for `timeout`.
pub fn probe(timeout: Duration) -> Caps {
    let mux = is_mux();
    let mut caps = Caps {
        sync_output: false,
        color: color_depth(),
        term_version: None,
        mux,
    };

    // DECRQM: report mode 2026. XTVERSION. DA1 (liveness + feature bits).
    let query = "\x1b[?2026$p\x1b[>0q\x1b[c";
    {
        let mut out = std::io::stdout();
        if out.write_all(query.as_bytes()).and_then(|_| out.flush()).is_err() {
            return caps;
        }
    }

    // Drain stdin on a thread with *bounded* reads: the thread must be
    // joined before we return, or it would keep racing ratatui's own
    // CPR read during Terminal init (the zombie-reader bug).
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let reader = {
        let stop = stop.clone();
        std::thread::spawn(move || read_stdin_bounded(stop, tx))
    };

    let deadline = Instant::now() + timeout;
    let mut acc = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(chunk) => {
                acc.extend_from_slice(&chunk);
                // All three probes answered → stop early.
                if sync_reported(&acc).is_some() && xtversion(&acc).is_some() {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = reader.join();

    caps.sync_output = !mux && matches!(sync_reported(&acc), Some(true));
    caps.term_version = xtversion(&acc);
    caps
}

/// Read stdin in bounded slices until `stop` — never leaves a blocked
/// reader behind. Unix uses poll(2); other platforms read once (a stale
/// reader there is accepted — terminals probed are unix in practice).
#[cfg(unix)]
fn read_stdin_bounded(
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tx: std::sync::mpsc::Sender<Vec<u8>>,
) {
    use std::os::unix::io::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let mut buf = [0u8; 512];
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        pfd.revents = 0;
        let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
        if ready <= 0 || pfd.revents & libc::POLLIN == 0 {
            continue;
        }
        match std::io::stdin().read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(not(unix))]
fn read_stdin_bounded(
    _stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tx: std::sync::mpsc::Sender<Vec<u8>>,
) {
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 512];
    if let Ok(n) = stdin.read(&mut buf) {
        let _ = tx.send(buf[..n].to_vec());
    }
}

/// Parse the DECRQM reply `CSI ? 2026 ; Ps $ y`.
/// Ps: 1 = set, 2 = reset, 3 = permanently set, 4 = permanently reset —
/// any of 1–4 means the terminal *implements* 2026. None = no reply yet.
fn sync_reported(buf: &[u8]) -> Option<bool> {
    let needle = b"\x1b[?2026;";
    let pos = buf.windows(needle.len()).position(|w| w == needle)?;
    let rest = &buf[pos + needle.len()..];
    if rest.len() < 3 || rest[1] != b'$' || rest[2] != b'y' {
        return None;
    }
    Some(matches!(rest[0], b'1'..=b'4'))
}

/// XTVERSION reply: `DCS > | version ST` (a DCS string, not CSI).
fn xtversion(buf: &[u8]) -> Option<String> {
    let pos = buf.windows(4).position(|w| w == b"\x1bP>|")?;
    let rest = &buf[pos + 4..];
    // Terminator: ST (\x1b\\ or \x9c) or BEL.
    let end = rest
        .iter()
        .position(|&b| b == 0x9c || b == 0x07)
        .or_else(|| rest.windows(2).position(|w| w == b"\x1b\\"))?;
    String::from_utf8(rest[..end].to_vec()).ok()
}

fn is_mux() -> bool {
    if std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some() {
        return true;
    }
    std::env::var("TERM")
        .map(|t| t.starts_with("screen") || t.starts_with("tmux"))
        .unwrap_or(false)
}

/// Color depth: flags > NO_COLOR > TERM=dumb > COLORTERM > TERM family.
/// (Ch.9 §3.2 precedence order.)
fn color_depth() -> ColorDepth {
    if std::env::var_os("NO_COLOR").is_some() {
        return ColorDepth::Mono;
    }
    if std::env::var_os("CLICOLOR_FORCE").is_some() {
        return ColorDepth::TrueColor;
    }
    let term = std::env::var("TERM").unwrap_or_default();
    if term == "dumb" {
        return ColorDepth::Mono;
    }
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    if matches!(colorterm.as_str(), "truecolor" | "24bit") || term.contains("direct") {
        return ColorDepth::TrueColor;
    }
    if term.contains("256color") {
        return ColorDepth::Ansi256;
    }
    if term.is_empty() {
        return ColorDepth::Mono;
    }
    ColorDepth::Ansi16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrqm_set_and_reset_both_mean_supported() {
        assert_eq!(sync_reported(b"\x1b[?2026;1$y"), Some(true));
        assert_eq!(sync_reported(b"\x1b[?2026;2$y"), Some(true));
        assert_eq!(sync_reported(b"\x1b[?2026;3$y"), Some(true));
        assert_eq!(sync_reported(b"\x1b[?2026;0$y"), Some(false));
        assert_eq!(sync_reported(b"noise"), None);
        // Trailing garbage after a real reply still parses.
        assert_eq!(sync_reported(b"x\x1b[?2026;1$yz"), Some(true));
    }

    #[test]
    fn xtversion_parses_st_and_bel() {
        assert_eq!(
            xtversion(b"\x1bP>|0;389;0\x1b\\"),
            Some("0;389;0".to_string())
        );
        assert_eq!(xtversion(b"\x1bP>|ghostty 1.2\x07"), Some("ghostty 1.2".into()));
        assert_eq!(xtversion(b"none"), None);
    }
}
