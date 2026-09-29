//! `overseer trigger fire` — inject an event over the ctl socket.

use super::daemon::{ctl_call, daemon_dirs, gateway_args};
use crate::args::Flag;

const TRIGGER_FLAGS: &[Flag] = &[
    Flag::value(&["--source"]),
    Flag::value(&["--class"]),
    Flag::value(&["--payload"]),
];

/// `overseer trigger fire` — inject an event (testing + webhook shim).
pub(crate) fn cmd_trigger(argv: &[String]) -> i32 {
    let parsed = match gateway_args(argv, TRIGGER_FLAGS) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("overseer trigger: {e}");
            return 2;
        }
    };
    let dirs = daemon_dirs(&parsed);
    if parsed.positionals().first().copied() != Some("fire") {
        eprintln!("overseer trigger: only 'fire' is supported");
        return 2;
    }
    let val = |flag: &str| parsed.value(flag).map(str::to_string);
    let (Some(source), Some(class), Some(payload)) =
        (val("--source"), val("--class"), val("--payload"))
    else {
        eprintln!("overseer trigger fire: needs --source --class --payload");
        return 2;
    };
    let req = overseer_gateway::ctl::CtlRequest::TriggerFire {
        source,
        class,
        payload,
    };
    match ctl_call(&dirs, req) {
        Ok(r) if r.ok => {
            println!("fired");
            0
        }
        Ok(r) => {
            eprintln!("overseer trigger: {}", r.error.unwrap_or_default());
            1
        }
        Err(e) => {
            eprintln!("overseer trigger: {e}");
            1
        }
    }
}
