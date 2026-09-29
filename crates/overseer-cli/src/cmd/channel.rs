//! `overseer channel` — outbound messaging and attention signals.

use super::daemon::{ctl_call, daemon_dirs, gateway_args};
use crate::args::Flag;

/// Every channel flag takes a value; the `signal` booleans are truthy
/// strings (`1|true|yes|on`).
const CHANNEL_FLAGS: &[Flag] = &[
    Flag::value(&["--to"]),
    Flag::value(&["--text"]),
    Flag::value(&["--thread"]),
    Flag::value(&["--via"]),
    Flag::value(&["--focused"]),
    Flag::value(&["--dnd"]),
    Flag::value(&["--calendar-busy"]),
    Flag::value(&["--app"]),
    Flag::value(&["--idle"]),
];

/// `overseer channel` — outbound messaging surface (P7-5): `send` only
/// ever *drafts* (approval still flows through inbox.decide/act, so the
/// ladder is never bypassed by a socket call), `digest` renders the
/// attention view over the inbox, and `signal` pushes a manual desktop-
/// attention fact (P7-6 — the testing path until the native shell lands).
pub(crate) fn cmd_channel(argv: &[String]) -> i32 {
    let parsed = match gateway_args(argv, CHANNEL_FLAGS) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("overseer channel: {e}");
            return 2;
        }
    };
    let dirs = daemon_dirs(&parsed);
    use overseer_gateway::ctl::CtlRequest;
    let val = |flag: &str| parsed.value(flag).map(str::to_string);
    let truthy = |v: Option<String>| {
        v.map(|s| matches!(s.as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false)
    };
    let req = match parsed.positionals().first().copied() {
        Some("send") => {
            let (Some(to), Some(text)) = (val("--to"), val("--text")) else {
                eprintln!("overseer channel send: needs --to <dest> --text <msg>");
                return 2;
            };
            CtlRequest::ChannelSend {
                to,
                thread: val("--thread"),
                text,
                channel: val("--via"),
            }
        }
        Some("digest") => CtlRequest::DigestGet,
        Some("signal") => CtlRequest::DesktopSignal {
            focused: truthy(val("--focused")),
            dnd: truthy(val("--dnd")),
            calendar_busy: truthy(val("--calendar-busy")),
            active_app: val("--app"),
            idle_s: val("--idle").and_then(|s| s.parse().ok()),
        },
        _ => {
            eprintln!(
                "overseer channel: send --to <d> --text <m> [--thread <t>] [--via <c>] \
                 | digest | signal [--focused <b>] [--dnd <b>] [--calendar-busy <b>] \
                 [--app <name>] [--idle <s>]"
            );
            return 2;
        }
    };
    match ctl_call(&dirs, req) {
        Ok(r) if r.ok => {
            if let Some(d) = r.data {
                println!("{}", serde_json::to_string_pretty(&d).unwrap_or_default());
            } else {
                println!("queued");
            }
            0
        }
        Ok(r) => {
            eprintln!("overseer channel: {}", r.error.unwrap_or_default());
            1
        }
        Err(e) => {
            eprintln!("overseer channel: {e}");
            1
        }
    }
}
