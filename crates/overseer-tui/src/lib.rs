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
pub mod notice;
pub mod notify;
pub mod probe;
pub mod theme;
pub mod web;
pub mod widgets;

/// The overseer mark as a terminal glyph — §10's single swap point:
/// change this const to re-cut the text-mode mark (the web surface
/// draws `web/mark.svg` instead; the two files must agree).
pub(crate) const MARK_GLYPH: &str = "\u{22C8}"; // ⋈ BOWTIE

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
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
/// terminal in raw mode (or inside the alternate screen).
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = execute!(
            out,
            DisableBracketedPaste,
            DisableFocusChange,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
}

/// Worker + app wiring shared by both surfaces: the human-verdict
/// channel (gate Ask → UI → decision), the agent worker thread, and
/// transcript seeding on --resume.
fn launch(mut cfg: TuiConfig) -> (App, std::thread::JoinHandle<()>) {
    let (engine_tx, engine_rx) = mpsc::channel::<EngineMsg>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();

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
    (app, worker)
}

/// Run the interactive session on the full-window surface: the
/// transcript owns the whole terminal except a 2-row prompt hanging
/// above the 1-row footer. On exit the transcript is handed to native
/// scrollback. Returns the process exit code.
pub fn run(cfg: TuiConfig) -> std::io::Result<i32> {
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
        execute!(
            out,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableFocusChange,
            EnableMouseCapture
        )?;
    }

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut term = Terminal::new(backend)?; // Viewport::Fullscreen
    term.clear()?;

    let (mut app, worker) = launch(cfg);
    app.osc = caps.osc;
    app.mode = app::UiMode::Full;

    let code = drive(&mut term, &caps, &mut app);

    // Leave the managed surface, then hand the transcript to native
    // scrollback — exiting must not erase the session's record.
    {
        let mut out = std::io::stdout();
        let _ = execute!(
            out,
            DisableMouseCapture,
            DisableBracketedPaste,
            DisableFocusChange,
            LeaveAlternateScreen
        );
    }
    print!("{}", app.transcript_plain());
    let _ = std::io::stdout().flush();
    let _ = worker.join();
    code
}

/// `--inline`: the scrollback-preserving live-strip surface. Completed
/// cells flush to native scrollback via `insert_before`; only the
/// bottom strip (dialog/queue/composer/status) is managed.
pub fn run_inline(cfg: TuiConfig) -> std::io::Result<i32> {
    if !std::io::stdout().is_terminal() {
        eprintln!("overseer: stdout is not a terminal — use `overseer exec` for pipes/CI");
        return Ok(2);
    }

    crossterm::terminal::enable_raw_mode()?;
    let _guard = TermGuard;

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

    let (mut app, worker) = launch(cfg);
    app.osc = caps.osc;

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

    let cwd = cfg.agent.cwd.display().to_string();
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
                    if let cells::Feed::NewCells(cs) = cells::feed(&ev, None) {
                        for c in cs {
                            print_cell(&c);
                        }
                    }
                }
                EngineMsg::Ask(req, tx) => {
                    println!(
                        "! {} {} — allow? [once/session/always/deny]",
                        notify::sanitize(&req.tool),
                        notify::sanitize(&cells::tool_summary(&req.tool, &req.input))
                    );
                    pending_ask = Some(tx);
                }
                EngineMsg::RunDone(out) => {
                    control = None;
                    if let overseer_core::agent::RunOutcome::Provider(m) = &out {
                        println!("provider error: {}", notify::sanitize(m));
                    }
                    print!("> ");
                    let _ = std::io::stdout().flush();
                }
                EngineMsg::RunError(e) => {
                    control = None;
                    println!("engine error: {}", notify::sanitize(&e));
                    print!("> ");
                    let _ = std::io::stdout().flush();
                }
                EngineMsg::SessionSwitched { dir } => {
                    println!(
                        "── session {} ──",
                        notify::sanitize(&dir.display().to_string())
                    );
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
                    let (code, out) = app::line_shell(cmd.trim(), &cwd);
                    println!("{}(exit {code})", notify::sanitize(&out));
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
            println!("{}", notify::sanitize(&c.plain()));
            if let Some(p) = c.link_path() {
                let url = format!("file://{}", p.display());
                println!("  ⤷ {}", notify::osc8(&url, &p.display().to_string()));
            }
        }
        _ => println!("{}", notify::sanitize(&c.plain())),
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
    // Unpredictable name + create_new + 0600 — a predictable
    // `overseer-draft-{pid}.md` in the shared temp dir is a symlink/
    // squat target.
    let file = std::env::temp_dir().join(format!("overseer-draft-{}.md", draft_suffix()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&file)?.write_all(draft.as_bytes())?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());

    crossterm::terminal::disable_raw_mode()?;
    {
        let mut out = std::io::stdout();
        if app.full() {
            // The editor needs the main screen back.
            let _ = execute!(
                out,
                DisableBracketedPaste,
                DisableFocusChange,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
        } else {
            let _ = execute!(out, DisableBracketedPaste, DisableFocusChange);
        }
    }
    let status = std::process::Command::new(&editor).arg(&file).status();
    crossterm::terminal::enable_raw_mode()?;
    {
        let mut out = std::io::stdout();
        if app.full() {
            let _ = execute!(
                out,
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableFocusChange,
                EnableMouseCapture
            );
        } else {
            let _ = execute!(out, EnableBracketedPaste, EnableFocusChange);
        }
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

/// `/dev/urandom` when it exists, nanos otherwise — enough entropy to
/// keep the draft filename unguessable.
fn draft_suffix() -> String {
    let mut raw = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut raw))
        .is_ok()
    {
        return raw.iter().map(|b| format!("{b:02x}")).collect();
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}
