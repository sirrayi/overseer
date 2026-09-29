//! `overseer tui` / bare `overseer` — the interactive frontends.

use overseer_core::provider::Provider;

use crate::args::{self, Arg, Flag};
use crate::flags::{exec_from, ExecFlags, EXEC_FLAGS};
use crate::provider::build_provider;
use crate::session::{agent_config, apply_credentials, resolve_session};

const DEFAULT_WEB_PORT: u16 = 8641;

/// Surface selectors layered on top of the exec flag set.
const TUI_FLAGS: &[Flag] = &[
    // --no-tui: line mode — same session plumbing, plain-text REPL
    // (screen readers, terminals a managed surface can't drive).
    Flag::switch(&["--no-tui"]),
    // --inline: the scrollback-preserving live-strip surface.
    Flag::switch(&["--inline"]),
    // --web: same session, rendered into a browser tab on localhost
    // (the fullscreen surface, drawn to a DOM grid).
    Flag::switch(&["--web"]),
    Flag::value(&["--web-port"]),
];

/// `overseer web` takes the tui set plus `--port`, a synonym for `--web-port`.
const WEB_FLAGS: &[Flag] = &[Flag::value(&["--port"])];

struct TuiFlags {
    line_mode: bool,
    inline: bool,
    web: bool,
    web_port: u16,
    exec: ExecFlags,
}

fn parse_tui(argv: &[String], web_cmd: bool) -> Result<TuiFlags, String> {
    let extra: &[Flag] = if web_cmd { WEB_FLAGS } else { &[] };
    let table: Vec<Flag> = EXEC_FLAGS
        .iter()
        .chain(TUI_FLAGS)
        .chain(extra)
        .copied()
        .collect();
    let mut f = TuiFlags {
        line_mode: false,
        inline: false,
        web: web_cmd,
        web_port: DEFAULT_WEB_PORT,
        exec: exec_from(Vec::new())?,
    };
    let mut rest = Vec::new();
    for arg in args::parse(argv, &table)?.args {
        match arg {
            Arg::Flag {
                name: "--no-tui", ..
            } => f.line_mode = true,
            Arg::Flag {
                name: "--inline", ..
            } => f.inline = true,
            Arg::Flag { name: "--web", .. } => f.web = true,
            Arg::Flag {
                name: "--web-port",
                value: Some(v),
            } => f.web_port = v.parse().map_err(|_| format!("bad --web-port '{v}'"))?,
            Arg::Flag {
                name: "--port",
                value: Some(v),
            } => f.web_port = v.parse().map_err(|_| format!("bad --port '{v}'"))?,
            other => rest.push(other),
        }
    }
    f.exec = exec_from(rest)?;
    Ok(f)
}

/// `overseer tui [exec flags]` / bare `overseer` — the interactive
/// terminal frontend. Same engine, same event stream, same flags as
/// `exec` (minus --json and the positional prompt).
pub(crate) fn cmd_tui(argv: &[String]) -> i32 {
    run(argv, "tui")
}

/// `overseer web [tui flags] [--port <n>]` — exactly `overseer tui --web
/// [--web-port <n>]`: the same session rendered into a localhost browser tab.
pub(crate) fn cmd_web(argv: &[String]) -> i32 {
    run(argv, "web")
}

fn run(argv: &[String], cmd: &str) -> i32 {
    let f = match parse_tui(argv, cmd == "web") {
        Ok(f) => f,
        Err(e) => {
            eprintln!("overseer {cmd}: {e}");
            return 2;
        }
    };
    let flags = &f.exec;
    if flags.prompt.is_some() {
        eprintln!("overseer {cmd}: no positional prompt — type inside the session");
        return 2;
    }
    if !f.line_mode && !f.web && !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        eprintln!("overseer: stdout is not a terminal — use `overseer exec` for pipes/CI");
        return 2;
    }
    let mut config = agent_config(flags);
    apply_credentials(&mut config);
    let provider: std::sync::Arc<dyn Provider> = match build_provider(flags, &config.broker) {
        Ok(p) => p.into(),
        Err(msg) => {
            eprintln!("overseer: {msg}");
            return 2;
        }
    };
    let (session_dir, resume) = resolve_session(flags);
    let cfg = overseer_tui::TuiConfig {
        provider,
        agent: config,
        session_dir,
        resume,
    };
    match if f.line_mode {
        overseer_tui::run_line(cfg)
    } else if f.web {
        overseer_tui::web::run_web(cfg, f.web_port)
    } else if f.inline {
        overseer_tui::run_inline(cfg)
    } else {
        overseer_tui::run(cfg)
    } {
        Ok(code) => code,
        Err(e) => {
            eprintln!("overseer {cmd}: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn tui_only(v: &[String]) -> Result<TuiFlags, String> {
        parse_tui(v, false)
    }

    #[test]
    fn web_is_tui_dash_dash_web() {
        let web = parse_tui(&argv(&["--port", "9001", "--model", "m"]), true).unwrap();
        let tui = tui_only(&argv(&["--web", "--web-port", "9001", "--model", "m"])).unwrap();
        for f in [&web, &tui] {
            assert!(f.web && !f.inline && !f.line_mode);
            assert_eq!(f.web_port, 9001);
            assert_eq!(f.exec.model, "m");
        }
        // Same flags as tui: --web-port and a redundant --web still work.
        let f = parse_tui(&argv(&["--web", "--web-port=9002"]), true).unwrap();
        assert!(f.web);
        assert_eq!(f.web_port, 9002);
        assert_eq!(parse_tui(&[], true).unwrap().web_port, DEFAULT_WEB_PORT);
        // --port belongs to `web` only.
        assert_eq!(
            tui_only(&argv(&["--port", "1"])).err().unwrap(),
            "unknown flag '--port'"
        );
        assert!(parse_tui(&argv(&["--port=x"]), true)
            .err()
            .unwrap()
            .contains("bad --port"));
    }

    #[test]
    fn tui_surface_flags_mix_with_exec_flags() {
        let f = tui_only(&argv(&["--web", "--model", "m", "--web-port=9000"])).unwrap();
        assert!(f.web && !f.inline && !f.line_mode);
        assert_eq!(f.web_port, 9000);
        assert_eq!(f.exec.model, "m");
        let f = tui_only(&argv(&["--no-tui", "--inline"])).unwrap();
        assert!(f.line_mode && f.inline);
        assert_eq!(f.web_port, DEFAULT_WEB_PORT);
    }

    #[test]
    fn tui_rejects_bad_port_and_unknown_flags() {
        assert!(tui_only(&argv(&["--web-port", "nope"]))
            .err()
            .unwrap()
            .contains("bad --web-port"));
        assert!(tui_only(&argv(&["--web-port"])).is_err());
        assert_eq!(
            tui_only(&argv(&["--bogus"])).err().unwrap(),
            "unknown flag '--bogus'"
        );
    }
}
