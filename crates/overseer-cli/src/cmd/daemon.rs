//! `overseer daemon` plus the ctl-socket plumbing `inbox`, `trigger` and
//! `channel` share.

use std::path::PathBuf;

use overseer_gateway::config::DaemonDirs;

use crate::args::{self, Flag, Parsed};
use crate::session::dirs_home;

const DIR_FLAG: Flag = Flag::value(&["--dir"]);

/// Parse a gateway subcommand's argv: `--dir <d>` everywhere, plus `extra`.
pub(crate) fn gateway_args(argv: &[String], extra: &[Flag]) -> Result<Parsed, String> {
    let table: Vec<Flag> = std::iter::once(DIR_FLAG)
        .chain(extra.iter().copied())
        .collect();
    args::parse(argv, &table)
}

/// `--dir <d>`, else `$OVERSEER_HOME/daemon` (default `~/.overseer/daemon`).
pub(crate) fn daemon_dirs(parsed: &Parsed) -> DaemonDirs {
    let dir = parsed.value("--dir").map(PathBuf::from).unwrap_or_else(|| {
        overseer_core::memory::overseer_home()
            .unwrap_or_else(dirs_home)
            .join("daemon")
    });
    DaemonDirs::new(dir)
}

pub(crate) fn ctl_call(
    dirs: &DaemonDirs,
    req: overseer_gateway::ctl::CtlRequest,
) -> Result<overseer_gateway::ctl::CtlResponse, String> {
    overseer_gateway::ctl::call(&dirs.socket(), &req)
}

/// `overseer daemon` — run the always-on gateway in the foreground
/// (launchd/systemd supervision comes with the release packaging).
/// Subcommands status/kill/reload go through the unix socket.
pub(crate) fn cmd_daemon(argv: &[String]) -> i32 {
    let parsed = match gateway_args(argv, &[]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("overseer daemon: {e}");
            return 2;
        }
    };
    let dirs = daemon_dirs(&parsed);
    use overseer_gateway::ctl::CtlRequest;
    match parsed.positionals().first().copied() {
        Some("status") => match ctl_call(&dirs, CtlRequest::Status) {
            Ok(r) => {
                println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
                if r.ok {
                    0
                } else {
                    1
                }
            }
            Err(e) => {
                eprintln!("overseer daemon: {e}");
                1
            }
        },
        Some("kill") => match ctl_call(&dirs, CtlRequest::Kill) {
            Ok(r) if r.ok => {
                println!("daemon stopping");
                0
            }
            Ok(r) => {
                eprintln!("overseer daemon: {}", r.error.unwrap_or_default());
                1
            }
            Err(e) => {
                eprintln!("overseer daemon: {e}");
                1
            }
        },
        Some("reload") => match ctl_call(&dirs, CtlRequest::Reload) {
            Ok(r) if r.ok => {
                println!("config reloaded");
                0
            }
            Ok(r) => {
                eprintln!("overseer daemon: {}", r.error.unwrap_or_default());
                1
            }
            Err(e) => {
                eprintln!("overseer daemon: {e}");
                1
            }
        },
        Some(other) => {
            eprintln!("overseer daemon: unknown subcommand '{other}' (run|status|kill|reload)");
            2
        }
        // Bare `overseer daemon` = run in foreground.
        None => {
            let bin = overseer_gateway::daemon::overseer_binary();
            match overseer_gateway::daemon::Daemon::new(dirs, bin) {
                Ok(mut d) => {
                    eprintln!(
                        "overseer daemon: running (kill: `overseer daemon kill` or touch STOP)"
                    );
                    d.run()
                }
                Err(e) => {
                    eprintln!("overseer daemon: {e}");
                    1
                }
            }
        }
    }
}

#[cfg(test)]
mod daemon_arg_tests {
    use super::*;

    fn positionals(v: &[&str]) -> Vec<String> {
        let argv: Vec<String> = v.iter().map(|s| s.to_string()).collect();
        gateway_args(&argv, &[])
            .unwrap()
            .positionals()
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn daemon_dir_flag_order_independent() {
        // The live-smoke bug: `daemon --dir $DD status` read `$DD` as the
        // subcommand. --dir pairs strip before subcommand detection.
        assert_eq!(positionals(&["--dir", "/tmp/x", "status"]), vec!["status"]);
        assert_eq!(positionals(&["status", "--dir", "/tmp/x"]), vec!["status"]);
        assert!(positionals(&["--dir", "/tmp/x"]).is_empty());
        assert_eq!(
            positionals(&["approve", "--dir", "/tmp/x", "abc"]),
            vec!["approve", "abc"]
        );
        assert_eq!(positionals(&["--dir=/tmp/x", "status"]), vec!["status"]);
    }

    #[test]
    fn daemon_dir_resolves_and_unknown_flags_are_refused() {
        let argv: Vec<String> = ["--dir=/tmp/dd", "status"].map(String::from).to_vec();
        let p = gateway_args(&argv, &[]).unwrap();
        assert_eq!(daemon_dirs(&p).root, PathBuf::from("/tmp/dd"));
        let e = gateway_args(&["--bogus".to_string()], &[]).unwrap_err();
        assert_eq!(e, "unknown flag '--bogus'");
        assert!(gateway_args(&["--dir".to_string()], &[]).is_err());
    }
}
