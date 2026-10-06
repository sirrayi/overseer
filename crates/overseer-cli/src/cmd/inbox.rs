//! `overseer inbox` — the Agent Inbox over the ctl socket.

use super::daemon::{ctl_call, daemon_dirs, gateway_args};

/// `overseer inbox` — the Agent Inbox surface: list/decide/act.
pub(crate) fn cmd_inbox(argv: &[String]) -> i32 {
    let parsed = match gateway_args(argv, &[]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("overseer inbox: {e}");
            return 2;
        }
    };
    let dirs = daemon_dirs(&parsed);
    use overseer_gateway::ctl::CtlRequest;
    let rest = parsed.positionals();
    let sub = rest.first().copied();
    let req = match sub {
        Some("list") | None => CtlRequest::InboxList,
        Some("approve") | Some("reject") => {
            let Some(id) = rest.get(1) else {
                eprintln!("overseer inbox {sub:?}: needs an item id");
                return 2;
            };
            CtlRequest::InboxDecide {
                id: id.to_string(),
                decision: sub.unwrap().to_string(),
                snooze_ms: None,
            }
        }
        Some("snooze") => {
            let Some(id) = rest.get(1) else {
                eprintln!("overseer inbox snooze: needs an item id");
                return 2;
            };
            // Seconds, as the usage says — always ×1000.
            let ms = rest
                .get(2)
                .and_then(|s| s.parse::<u64>().ok())
                .map(|v| v.saturating_mul(1000));
            CtlRequest::InboxDecide {
                id: id.to_string(),
                decision: "snooze".into(),
                snooze_ms: ms,
            }
        }
        Some("act") => {
            let Some(id) = rest.get(1) else {
                eprintln!("overseer inbox act: needs an item id");
                return 2;
            };
            CtlRequest::InboxAct { id: id.to_string() }
        }
        Some(other) => {
            eprintln!(
                "overseer inbox: unknown subcommand '{other}' (list|approve|reject|snooze|act)"
            );
            return 2;
        }
    };
    match ctl_call(&dirs, req) {
        Ok(r) if r.ok => {
            if let Some(d) = r.data {
                println!("{}", serde_json::to_string_pretty(&d).unwrap_or_default());
            }
            0
        }
        Ok(r) => {
            eprintln!("overseer inbox: {}", r.error.unwrap_or_default());
            1
        }
        Err(e) => {
            eprintln!("overseer inbox: {e}");
            1
        }
    }
}
