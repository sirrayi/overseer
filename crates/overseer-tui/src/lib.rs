//! overseer-tui — terminal frontend (Phase 2).
//!
//! Phase 0 policy holds: the TUI is a view over the same event stream the
//! `exec --json` mode emits, never a second engine. The agent loop runs
//! on a worker thread; the UI renders `Event`s, answers `Ask`s, and
//! steers through `Control`.

pub mod app;
pub mod cells;
pub mod composer;
pub mod diff;
pub mod markdown;
pub mod notify;
pub mod probe;
pub mod theme;
pub mod widgets;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
};
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
    pub provider: std::sync::Arc<dyn Provider>,
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
        let _ = execute!(out, DisableBracketedPaste, DisableFocusChange);
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
    theme::set_theme(theme::Theme::detect(&caps));
    {
        let mut out = std::io::stdout();
        execute!(out, EnableBracketedPaste, EnableFocusChange)?;
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
    app.osc = caps.osc;
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

/// `--no-tui`: line mode — the same engine and worker, plain-text REPL
/// instead of the managed viewport. For screen readers, pipes, and
/// terminals the inline region can't drive. Events print as unstyled
/// lines; permission asks read a one-line answer; Ctrl+D or `/quit`
/// exits. File paths print as OSC 8 links (harmless where unsupported).
pub fn run_line(mut cfg: TuiConfig) -> std::io::Result<i32> {
    let (engine_tx, engine_rx) = mpsc::channel::<EngineMsg>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
    let (line_tx, line_rx) = mpsc::channel::<String>();

    let ask_tx = engine_tx.clone();
    cfg.agent.ask_handler = Some(AskHandler(Arc::new(move |req: &AskRequest| {
        let (rtx, rrx) = mpsc::channel();
        if ask_tx.send(EngineMsg::Ask(req.clone(), rtx)).is_err() {
            return AskDecision::Deny;
        }
        rrx.recv().unwrap_or(AskDecision::Deny)
    })));

    let worker = spawn_worker(cfg, engine_tx, cmd_rx);
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            match line {
                Ok(l) => {
                    if line_tx.send(l).is_err() {
                        return; // UI gone
                    }
                }
                Err(_) => return, // EOF / read error
            }
        }
    });

    println!("overseer — line mode (/quit exits, !cmd runs locally)");
    let mut pending_ask: Option<mpsc::Sender<AskDecision>> = None;
    let mut control: Option<overseer_core::control::Control> = None;

    loop {
        while let Ok(msg) = engine_rx.try_recv() {
            match msg {
                EngineMsg::Event(ev) => {
                    if let cells::Feed::NewCells(cs) = cells::feed(&ev) {
                        for c in cs {
                            print_cell(&c);
                        }
                    }
                }
                EngineMsg::Ask(req, tx) => {
                    println!(
                        "! {} {} — allow? [once/session/always/deny]",
                        req.tool,
                        cells::tool_summary(&req.tool, &req.input)
                    );
                    pending_ask = Some(tx);
                }
                EngineMsg::RunDone(out) => {
                    control = None;
                    if let overseer_core::agent::RunOutcome::Provider(m) = &out {
                        println!("provider error: {m}");
                    }
                    print!("> ");
                    let _ = std::io::stdout().flush();
                }
                EngineMsg::RunError(e) => {
                    control = None;
                    println!("engine error: {e}");
                    print!("> ");
                    let _ = std::io::stdout().flush();
                }
                EngineMsg::SessionSwitched { dir } => {
                    println!("── session {} ──", dir.display());
                }
            }
        }
        match line_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(line) => {
                let line = line.trim().to_string();
                if let Some(tx) = pending_ask.take() {
                    let d = match line.as_str() {
                        "once" | "o" | "1" => AskDecision::AllowOnce,
                        "session" | "s" | "2" => AskDecision::AllowSession,
                        "always" | "a" | "3" => AskDecision::AllowAlways,
                        _ => AskDecision::Deny,
                    };
                    let _ = tx.send(d);
                } else if line == "/quit" || line == "/exit" {
                    break;
                } else if let Some(cmd) = line.strip_prefix('!') {
                    let (code, out) = app::line_shell(cmd.trim());
                    println!("{out}(exit {code})");
                } else if !line.is_empty() {
                    if let Some(c) = &control {
                        c.steer(line); // mid-run → steering
                    } else {
                        let c = overseer_core::control::Control::default();
                        control = Some(c.clone());
                        let _ = cmd_tx.send(WorkerCmd::Submit {
                            text: line,
                            control: c,
                        });
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // Ctrl+D / closed stdin
        }
    }
    let _ = cmd_tx.send(WorkerCmd::Shutdown);
    let _ = worker.join();
    Ok(0)
}

/// Print one cell as plain lines (`--no-tui` renderer).
fn print_cell(c: &cells::Cell) {
    match c {
        cells::Cell::User { .. } => {} // the echoed input line suffices
        cells::Cell::Tool { .. } => {
            println!("{}", c.plain());
            if let Some(p) = c.link_path() {
                let url = format!("file://{}", p.display());
                println!("  ⤷ {}", notify::osc8(&url, &p.display().to_string()));
            }
        }
        _ => println!("{}", c.plain()),
    }
}

fn spawn_worker(
    cfg: TuiConfig,
    engine_tx: mpsc::Sender<EngineMsg>,
    cmd_rx: mpsc::Receiver<WorkerCmd>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let provider = cfg.provider;
        let agent_cfg = cfg.agent.clone();
        let agent = if cfg.resume {
            Agent::resume(provider.clone(), cfg.agent, cfg.session_dir.clone())
        } else {
            let id = cfg
                .session_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "session".into());
            Agent::start(provider.clone(), cfg.agent, cfg.session_dir.clone(), id)
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
                WorkerCmd::SwitchSession { dir } => {
                    // Rebuild the agent on another log — session switch,
                    // post-fork, and post-rewind all rebuild context here.
                    match Agent::resume(provider.clone(), agent_cfg.clone(), dir.clone()) {
                        Ok(a) => {
                            agent = a;
                            let _ = engine_tx.send(EngineMsg::SessionSwitched { dir });
                        }
                        Err(e) => {
                            let _ = engine_tx.send(EngineMsg::RunError(format!(
                                "cannot open session {}: {e}",
                                dir.display()
                            )));
                        }
                    }
                }
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
        // `/edit` or Alt+E: hand the draft to $EDITOR with the terminal
        // fully restored, then read it back into the composer.
        if let Some(draft) = app.take_editor_request() {
            edit_in_editor(draft, app)?;
            term.clear()?;
            app.pump(term, caps)?; // repaint before the next poll
        }
    }
    Ok(0)
}

/// Suspend the TUI, run $VISUAL/$EDITOR (fallback `vi`) on a temp file
/// seeded with `draft`, reinstall the result as the composer buffer.
fn edit_in_editor(draft: String, app: &mut App) -> std::io::Result<()> {
    let file = std::env::temp_dir().join(format!("overseer-draft-{}.md", std::process::id()));
    std::fs::write(&file, &draft)?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());

    crossterm::terminal::disable_raw_mode()?;
    {
        let mut out = std::io::stdout();
        let _ = execute!(out, DisableBracketedPaste, DisableFocusChange);
    }
    let status = std::process::Command::new(&editor).arg(&file).status();
    crossterm::terminal::enable_raw_mode()?;
    {
        let mut out = std::io::stdout();
        let _ = execute!(out, EnableBracketedPaste, EnableFocusChange);
    }
    match status {
        Ok(s) if s.success() => {
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            app.set_composer_text(text);
        }
        Ok(_) => app.set_composer_text(draft), // nonzero exit: keep draft
        Err(e) => {
            app.set_composer_text(draft);
            eprintln!("overseer: editor '{editor}' failed: {e}");
        }
    }
    let _ = std::fs::remove_file(&file);
    Ok(())
}
