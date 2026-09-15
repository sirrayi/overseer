//! overseer-tui — terminal frontend (Phase 2).
//!
//! Phase 0 policy holds: the TUI is a view over the same event stream the
//! `exec --json` mode emits, never a second engine. The agent loop runs
//! on a worker thread; the UI renders `Event`s, answers `Ask`s, and
//! steers through `Control`.

pub mod app;
pub mod cells;
pub mod composer;
pub mod markdown;
pub mod probe;
pub mod theme;
pub mod widgets;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use overseer_core::agent::{Agent, AgentConfig};
use overseer_core::event::EventLog;
use overseer_core::perm::{AskDecision, AskHandler, AskRequest};
use overseer_core::provider::Provider;
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};

use app::{App, EngineMsg, WorkerCmd};

/// Everything a TUI session needs — the CLI resolves flags/env into this.
pub struct TuiConfig {
    pub provider: Box<dyn Provider>,
    pub agent: AgentConfig,
    pub session_dir: PathBuf,
    /// Resume an existing session directory (replays history into the
    /// transcript on startup).
    pub resume: bool,
}

/// Restore-on-drop guard: a panic mid-frame must not strand the user's
/// terminal in raw mode.
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = execute!(out, DisableBracketedPaste);
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
}

/// Run the interactive session. Returns the process exit code.
pub fn run(mut cfg: TuiConfig) -> std::io::Result<i32> {
    if !std::io::stdout().is_terminal() {
        eprintln!("overseer: stdout is not a terminal — use `overseer exec` for pipes/CI");
        return Ok(2);
    }

    crossterm::terminal::enable_raw_mode()?;
    let _guard = TermGuard;

    // Capability probe through the pty — before the UI owns stdin.
    let caps = probe::probe(std::time::Duration::from_millis(250));
    {
        let mut out = std::io::stdout();
        execute!(out, EnableBracketedPaste)?;
    }

    let backend = CrosstermBackend::new(std::io::stdout());
    // Fixed-height live region (ratatui's inline viewport height is set
    // at init): tall enough for dialog + composer + status, small enough
    // to keep the transcript dominant. Composer never moves while typing.
    let inline_h = {
        let (_, h) = crossterm::terminal::size().unwrap_or((80, 24));
        (h / 3).clamp(6, 12).min(h.saturating_sub(2)).max(2)
    };
    let mut term = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(inline_h),
        },
    )?;

    let (engine_tx, engine_rx) = mpsc::channel::<EngineMsg>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();

    // The human-verdict channel: gate Ask → UI dialog → decision.
    let ask_tx = engine_tx.clone();
    cfg.agent.ask_handler = Some(AskHandler(Arc::new(move |req: &AskRequest| {
        let (rtx, rrx) = mpsc::channel();
        if ask_tx.send(EngineMsg::Ask(req.clone(), rtx)).is_err() {
            return AskDecision::Deny;
        }
        // Fail closed if the UI is gone.
        rrx.recv().unwrap_or(AskDecision::Deny)
    })));

    let preset = cfg.agent.policy_preset;
    let cwd = cfg.agent.cwd.display().to_string();
    let model = cfg.agent.model.clone();
    let session_dir = cfg.session_dir.clone();
    let resume = cfg.resume;
    let worker = spawn_worker(cfg, engine_tx, cmd_rx);

    let mut app = App::new(engine_rx, cmd_tx, preset, cwd, model, session_dir.clone());
    if resume {
        // Replayed events seed the transcript — resume shows history.
        if let Ok(events) = EventLog::replay(session_dir.join("events.jsonl")) {
            for ev in &events {
                app.seed(ev);
            }
        }
    }

    let code = drive(&mut term, &caps, &mut app);
    let _ = worker.join();
    code
}

fn spawn_worker(
    cfg: TuiConfig,
    engine_tx: mpsc::Sender<EngineMsg>,
    cmd_rx: mpsc::Receiver<WorkerCmd>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let provider = cfg.provider;
        let agent = if cfg.resume {
            Agent::resume(provider.as_ref(), cfg.agent, cfg.session_dir.clone())
        } else {
            let id = cfg
                .session_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "session".into());
            Agent::start(provider.as_ref(), cfg.agent, cfg.session_dir.clone(), id)
        };
        let mut agent = match agent {
            Ok(a) => a,
            Err(e) => {
                let _ = engine_tx.send(EngineMsg::RunError(format!(
                    "cannot open session {}: {e}",
                    cfg.session_dir.display()
                )));
                return;
            }
        };

        while let Ok(cmd) = cmd_rx.recv() {
            match cmd {
                WorkerCmd::Submit { text, control } => {
                    agent.set_control(control);
                    let sink_tx = engine_tx.clone();
                    let mut sink = move |e: &overseer_core::event::Event| {
                        let _ = sink_tx.send(EngineMsg::Event(e.clone()));
                    };
                    match agent.run_turn(&text, &mut sink) {
                        Ok(out) => {
                            let _ = engine_tx.send(EngineMsg::RunDone(out));
                        }
                        Err(e) => {
                            let _ = engine_tx.send(EngineMsg::RunError(e.to_string()));
                        }
                    }
                }
                WorkerCmd::SetPreset(p) => agent.set_preset(p),
                WorkerCmd::Shutdown => break,
            }
        }
    })
}

fn drive(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    caps: &probe::Caps,
    app: &mut App,
) -> std::io::Result<i32> {
    while !app.quit {
        app.pump(term, caps)?;
    }
    Ok(0)
}
