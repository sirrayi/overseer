//! App state + event loop (P2.1–2.5, 2.7).
//!
//! Frame model: the engine runs on a worker thread and streams `Event`s
//! over a channel; completed events become immutable cells flushed to
//! native scrollback via `insert_before`. Only the live region — running
//! tools, indicator, queue strip, dialogs, composer, status line — is
//! managed, inside a dynamically-sized `Viewport::Inline`. Every frame is
//! wrapped in BSU/ESU when the terminal probed positive for sync 2026.

use std::io::Write;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyModifiers};
use overseer_core::control::Control;
use overseer_core::event::Event;
use overseer_core::perm::{AskDecision, AskRequest, Preset};
use ratatui::backend::CrosstermBackend;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use crate::cells::{self, Cell, Feed, ToolStatus};
use crate::composer::Composer;
use crate::probe::{self, Caps};
use crate::widgets::{self, Dialog};

/// Engine → UI messages (single channel keeps ordering trivial).
pub enum EngineMsg {
    Event(Event),
    Ask(AskRequest, mpsc::Sender<AskDecision>),
    RunDone(overseer_core::agent::RunOutcome),
    RunError(String),
    /// The worker rebuilt its agent on a different session directory —
    /// the UI reseeds the transcript from that log.
    SessionSwitched { dir: std::path::PathBuf },
}

/// UI → worker commands.
pub enum WorkerCmd {
    Submit { text: String, control: Control },
    SetPreset(Preset),
    /// Drop the current agent and `Agent::resume` on `dir` — session
    /// switch, post-fork, and post-rewind rebuild all share this path.
    SwitchSession { dir: std::path::PathBuf },
    Shutdown,
}

enum RunState {
    Idle,
    Running {
        control: Control,
        started: Instant,
        phase: String,
    },
}

/// One row in the `/rewind` picker: a checkpoint boundary + label.
pub struct RewindRow {
    pub boundary: u64,
    pub files: u32,
    pub label: String,
}

/// Modal overlay pickers (P2.6). Unlike the permission dialog these own
/// the keyboard — the sessions filter IS a text input by design.
pub enum Overlay {
    Sessions {
        rows: Vec<overseer_core::session::SessionInfo>,
        sel: usize,
        filter: String,
        /// false = one line per session; true = adds a preview line.
        wide: bool,
    },
    Rewind {
        rows: Vec<RewindRow>,
        sel: usize,
    },
}

pub struct App {
    engine_rx: mpsc::Receiver<EngineMsg>,
    worker_tx: mpsc::Sender<WorkerCmd>,
    run: RunState,
    /// Completed cells awaiting `insert_before`.
    pending: Vec<Cell>,
    /// In-flight tool cells (call_id order) rendered in the live region.
    live: Vec<Cell>,
    /// Steers that missed delivery (run ended first) — drain on idle.
    pending_queue: Vec<String>,
    composer: Composer,
    dialog: Option<(Dialog, mpsc::Sender<AskDecision>)>,
    overlay: Option<Overlay>,
    preset: Preset,
    cwd: String,
    model: String,
    session_dir: std::path::PathBuf,
    tokens: u64,
    cost: f64,
    show_help: bool,
    show_plan: bool,
    tick: usize,
    dirty: bool,
    pub quit: bool,
}

impl App {
    pub fn new(
        engine_rx: mpsc::Receiver<EngineMsg>,
        worker_tx: mpsc::Sender<WorkerCmd>,
        preset: Preset,
        cwd: String,
        model: String,
        session_dir: std::path::PathBuf,
    ) -> Self {
        App {
            engine_rx,
            worker_tx,
            run: RunState::Idle,
            pending: Vec::new(),
            live: Vec::new(),
            pending_queue: Vec::new(),
            composer: Composer::new(),
            dialog: None,
            overlay: None,
            preset,
            cwd,
            model,
            session_dir,
            tokens: 0,
            cost: 0.0,
            show_help: false,
            show_plan: false,
            tick: 0,
            dirty: true,
            quit: false,
        }
    }

    fn running(&self) -> bool {
        matches!(self.run, RunState::Running { .. })
    }

    /// Feed one replayed event into the transcript (resume seeding).
    pub fn seed(&mut self, ev: &Event) {
        self.on_event(ev);
        // Seeded runs end idle — a replayed ToolCallStart without a
        // result (torn tail) would otherwise linger in `live`.
        for c in self.live.drain(..) {
            self.pending.push(c);
        }
    }

    /// The whole frame: drain channels → input → flush cells → draw.
    pub fn pump(
        &mut self,
        term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
        caps: &Caps,
    ) -> std::io::Result<()> {
        self.poll_input()?;
        self.step(term, caps)?;
        if self.quit {
            let _ = self.worker_tx.send(WorkerCmd::Shutdown);
        }
        Ok(())
    }

    /// Drain engine messages, flush cells, redraw — the half of `pump`
    /// that doesn't touch the keyboard, kept generic over the backend
    /// so `TestBackend` can drive the real pipeline in tests.
    pub fn step<B: ratatui::backend::Backend>(
        &mut self,
        term: &mut Terminal<B>,
        caps: &Caps,
    ) -> std::io::Result<()>
    where
        B::Error: std::fmt::Display,
    {
        self.drain_engine();
        self.tick = self.tick.wrapping_add(1);
        // Dialogs are transient and time-gated — repaint while open so
        // the grace→armed transition appears without further input.
        if self.dialog.is_some() {
            self.dirty = true;
        }
        self.flush_cells(term, caps)?;
        if self.dirty {
            self.draw(term, caps)?;
        }
        Ok(())
    }

    /// Test/debug hook: submit a message as if the user pressed Enter.
    pub fn submit_text(&mut self, text: &str) {
        self.on_submit(text.to_string());
    }

    /// Test/debug hook: synthesize a key press.
    pub fn key(&mut self, key: KeyEvent) {
        self.on_key(key);
    }

    // ── engine messages ──────────────────────────────────────────────

    fn drain_engine(&mut self) {
        while let Ok(msg) = self.engine_rx.try_recv() {
            self.dirty = true;
            match msg {
                EngineMsg::Event(ev) => self.on_event(&ev),
                EngineMsg::Ask(req, tx) => {
                    self.dialog = Some((
                        Dialog {
                            req,
                            opened: Instant::now(),
                            selected: 0,
                        },
                        tx,
                    ));
                }
                EngineMsg::RunDone(out) => self.on_run_done(out),
                EngineMsg::SessionSwitched { dir } => self.on_switched(dir),
                EngineMsg::RunError(e) => {
                    self.restore_queue_to_composer();
                    self.run = RunState::Idle;
                    self.pending.push(Cell::Meta {
                        style: crate::theme::ERROR,
                        text: format!("engine error: {e}"),
                    });
                }
            }
        }
    }

    fn on_event(&mut self, ev: &Event) {
        use overseer_core::event::EventKind;
        if let EventKind::ModelResponse { usage, .. } = ev.kind {
            self.tokens += usage.total_input() + usage.output;
        }
        if let EventKind::RunEnd { total_cost_usd, .. } = ev.kind {
            self.cost = total_cost_usd;
        }
        if let EventKind::ToolCallStart { name, .. } = &ev.kind {
            if let RunState::Running { phase, .. } = &mut self.run {
                *phase = format!("running {name}");
            }
        }
        match cells::feed(ev) {
            Feed::NewCells(new) => {
                for c in new {
                    if let Cell::Tool {
                        status: ToolStatus::Running,
                        ..
                    } = c
                    {
                        self.live.push(c);
                    } else {
                        self.pending.push(c);
                    }
                }
            }
            Feed::ToolDone {
                call_id,
                status,
                output,
            } => {
                if let Some(pos) = self.live.iter().position(|c| match c {
                    Cell::Tool { id, .. } => id == &call_id,
                    _ => false,
                }) {
                    if let Cell::Tool {
                        status: s,
                        output: o,
                        ..
                    } = &mut self.live[pos]
                    {
                        *s = status;
                        *o = output;
                    }
                    let done = self.live.remove(pos);
                    self.pending.push(done);
                }
            }
            Feed::Ignore => {}
        }
    }

    fn on_run_done(&mut self, out: overseer_core::agent::RunOutcome) {
        use overseer_core::agent::RunOutcome as O;
        let interrupted = matches!(out, O::Interrupted { .. });
        let provider_err = matches!(out, O::Provider(_));
        if let O::Provider(msg) = &out {
            self.pending.push(Cell::Meta {
                style: crate::theme::ERROR,
                text: format!("provider error: {msg}"),
            });
        }
        // Undelivered steers: interrupt hands them back to the composer
        // for editing (nothing fires unattended); a normal end auto-
        // drains the queue FIFO, per the queued≠sent contract.
        let leftover = match &self.run {
            RunState::Running { control, .. } => control.queued(),
            RunState::Idle => Vec::new(),
        };
        self.run = RunState::Idle;
        self.live.clear();
        if interrupted || provider_err {
            if !leftover.is_empty() {
                self.composer.insert_str(&leftover.join("\n"));
            }
            self.pending_queue.clear();
        } else {
            self.pending_queue.extend(leftover);
            self.drain_pending_queue();
        }
    }

    /// The worker switched sessions: divider into scrollback, then the
    /// new log replays into the transcript (same seeding as --resume).
    fn on_switched(&mut self, dir: std::path::PathBuf) {
        self.session_dir = dir.clone();
        self.live.clear();
        self.tokens = 0;
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string());
        self.pending.push(Cell::Meta {
            style: crate::theme::META,
            text: format!("── session {name} ──"),
        });
        if let Ok(events) = overseer_core::event::EventLog::replay(dir.join("events.jsonl")) {
            for ev in &events {
                self.seed(ev);
            }
        }
    }

    /// Interrupt/error cleanup: undelivered steers return to the
    /// composer for editing — nothing fires unattended after a stop.
    fn restore_queue_to_composer(&mut self) {
        if let RunState::Running { control, .. } = &self.run {
            let leftover = control.queued();
            if !leftover.is_empty() {
                self.composer.insert_str(&leftover.join("\n"));
            }
        }
        self.pending_queue.clear();
    }

    /// FIFO drain on idle: the oldest queued message becomes the next
    /// submitted turn.
    fn drain_pending_queue(&mut self) {
        if self.running() || self.pending_queue.is_empty() {
            return;
        }
        let text = self.pending_queue.remove(0);
        self.submit(text);
    }

    // ── input ────────────────────────────────────────────────────────

    fn poll_input(&mut self) -> std::io::Result<()> {
        // Spinner cadence ~10 fps while running; near-idle otherwise.
        let wait = if self.running() || self.dialog.is_some() {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(250)
        };
        if !event::poll(wait)? {
            if self.running() || self.dialog.is_some() {
                self.dirty = true; // spinner/elapsed/dialog-arming tick
            }
            return Ok(());
        }
        match event::read()? {
            CtEvent::Key(key) => self.on_key(key),
            CtEvent::Paste(text) => {
                self.composer.paste(&text);
                self.dirty = true;
            }
            CtEvent::Resize(_, _) => self.dirty = true,
            _ => {}
        }
        Ok(())
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Modal pickers own the keyboard entirely (the sessions filter
        // is a text input by design). Esc always closes first.
        if self.overlay.is_some() {
            self.on_overlay_key(key);
            self.dirty = true;
            return;
        }
        // Dialog claims only its own keys — arrows/Enter/digits/Esc.
        // Everything else keeps flowing to the composer (no focus theft;
        // you can keep typing while a permission prompt waits).
        if self.dialog.is_some() {
            let decide = match (key.code, key.modifiers) {
                (KeyCode::Esc, _) => Some(Some(AskDecision::Deny)),
                (KeyCode::Enter, _) => {
                    if self.dialog.as_ref().unwrap().0.armed() {
                        Some(Some(self.dialog.as_ref().unwrap().0.confirm()))
                    } else {
                        Some(None) // grace: swallow, don't submit either
                    }
                }
                (KeyCode::Left, _) | (KeyCode::Right, _) => {
                    let (dlg, _) = self.dialog.as_mut().unwrap();
                    if dlg.armed() {
                        dlg.move_sel(if key.code == KeyCode::Left { -1 } else { 1 });
                    }
                    Some(None)
                }
                (KeyCode::Char(c), m)
                    if m.is_empty() && matches!(c, '1' | '2' | '3') =>
                {
                    let (dlg, _) = self.dialog.as_ref().unwrap();
                    match dlg.resolve(c) {
                        Some(d) => Some(Some(d)),
                        None => Some(None), // grace: swallow the digit
                    }
                }
                _ => None,
            };
            if let Some(decision) = decide {
                if let Some(d) = decision {
                    let (_, tx) = self.dialog.take().unwrap();
                    let _ = tx.send(d);
                }
                self.dirty = true;
                return;
            }
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                if self.composer.is_empty() {
                    self.quit = true;
                }
            }
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.composer.clear();
            }
            (KeyCode::Esc, _) => {
                if self.running() {
                    if let RunState::Running { control, .. } = &self.run {
                        control.interrupt();
                    }
                } else {
                    self.composer.clear();
                    self.show_help = false;
                }
            }
            (KeyCode::Enter, m) if m.contains(KeyModifiers::ALT) => self.composer.newline(),
            (KeyCode::Enter, _) => {
                let text = self.composer.submit();
                if !text.is_empty() {
                    self.on_submit(text);
                } else {
                    self.show_help = false;
                }
            }
            (KeyCode::Char('j'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.composer.newline()
            }
            (KeyCode::BackTab, m) if m.contains(KeyModifiers::SHIFT) => self.cycle_mode(),
            (KeyCode::Char('t'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.show_plan = !self.show_plan;
            }
            (KeyCode::Char('p'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.open_sessions();
            }
            (KeyCode::Char('x'), m) if m.contains(KeyModifiers::CONTROL) => {
                // Cancel the newest undelivered queue entry.
                if let RunState::Running { control, .. } = &self.run {
                    let n = control.queued().len();
                    if n > 0 {
                        let _ = control.cancel_queued(n - 1);
                    }
                } else {
                    self.pending_queue.pop();
                }
            }
            (KeyCode::Char('s'), m) if m.contains(KeyModifiers::CONTROL) => self.composer.stash(),
            (KeyCode::Char('_'), m) if m.contains(KeyModifiers::CONTROL) => self.composer.undo(),
            (KeyCode::Char('k'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.composer.kill_to_eol()
            }
            (KeyCode::Char('a'), m) if m.contains(KeyModifiers::CONTROL) => self.composer.home(),
            (KeyCode::Char('e'), m) if m.contains(KeyModifiers::CONTROL) => self.composer.end(),
            (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.composer.delete_word_back()
            }
            (KeyCode::Left, m) if m.contains(KeyModifiers::ALT) => self.composer.word_left(),
            (KeyCode::Right, m) if m.contains(KeyModifiers::ALT) => self.composer.word_right(),
            (KeyCode::Left, _) => self.composer.left(),
            (KeyCode::Right, _) => self.composer.right(),
            (KeyCode::Up, _) => self.composer.up(),
            (KeyCode::Down, _) => self.composer.down(),
            (KeyCode::Home, _) => self.composer.home(),
            (KeyCode::End, _) => self.composer.end(),
            (KeyCode::Backspace, _) => self.composer.backspace(),
            (KeyCode::Delete, _) => self.composer.delete(),
            (KeyCode::Char('?'), _) if self.composer.is_empty() => {
                self.show_help = !self.show_help;
            }
            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                self.composer.insert_str(&c.to_string());
            }
            _ => return,
        }
        self.dirty = true;
    }

    fn on_overlay_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                self.overlay = None;
            }
            (KeyCode::Up, _) => {
                if let Some(o) = &mut self.overlay {
                    let (sel, len) = overlay_sel(o);
                    *sel = sel.saturating_sub(1).min(len);
                }
            }
            (KeyCode::Down, _) => {
                if let Some(o) = &mut self.overlay {
                    let (sel, len) = overlay_sel(o);
                    *sel = (*sel + 1).min(len);
                }
            }
            (KeyCode::Tab, _) => {
                if let Some(Overlay::Sessions { wide, .. }) = &mut self.overlay {
                    *wide = !*wide;
                }
            }
            (KeyCode::Enter, _) => self.overlay_confirm(),
            (KeyCode::Backspace, _) => {
                if let Some(Overlay::Sessions { filter, sel, .. }) = &mut self.overlay {
                    filter.pop();
                    *sel = 0;
                }
            }
            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                if let Some(Overlay::Sessions { filter, sel, .. }) = &mut self.overlay {
                    filter.push(c);
                    *sel = 0;
                }
            }
            _ => {}
        }
    }

    fn overlay_confirm(&mut self) {
        match self.overlay.take() {
            Some(Overlay::Sessions { rows, sel, filter, .. }) => {
                if let Some(info) = filtered_sessions(&rows, &filter).get(sel).cloned() {
                    if info.dir != self.session_dir {
                        let _ = self.worker_tx.send(WorkerCmd::SwitchSession {
                            dir: info.dir.clone(),
                        });
                    }
                }
            }
            Some(Overlay::Rewind { rows, sel }) => {
                if let Some(row) = rows.get(sel) {
                    self.do_rewind(row.boundary);
                }
            }
            None => {}
        }
    }

    /// `/sessions` (or Ctrl+P): the picker only opens between runs —
    /// switching mid-run would orphan its control/ask state.
    fn open_sessions(&mut self) {
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::ERROR,
                text: "finish or interrupt the run first".into(),
            });
            return;
        }
        let Some(root) = self.session_dir.parent().map(|p| p.to_path_buf()) else {
            return;
        };
        let mut rows = overseer_core::session::list(&root);
        // Current-cwd sessions first (the picker is cwd-scoped by
        // convention, like --continue).
        let cwd = self.cwd.clone();
        rows.sort_by_key(|s| std::cmp::Reverse(s.cwd == cwd));
        self.overlay = Some(Overlay::Sessions {
            rows,
            sel: 0,
            filter: String::new(),
            wide: false,
        });
    }

    /// `/rewind`: checkpoints of the current session with their labels.
    fn open_rewind(&mut self) {
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::ERROR,
                text: "finish or interrupt the run first".into(),
            });
            return;
        }
        let rows: Vec<RewindRow> = overseer_core::session::checkpoints(&self.session_dir)
            .into_iter()
            .rev()
            .map(|b| RewindRow {
                boundary: b,
                files: manifest_files(&self.session_dir, b),
                label: overseer_core::session::checkpoint_label(&self.session_dir, b)
                    .unwrap_or_else(|| "(no prompt text)".into()),
            })
            .collect();
        if rows.is_empty() {
            self.pending.push(Cell::Meta {
                style: crate::theme::DIM,
                text: "no checkpoints yet — checkpoints open on each prompt".into(),
            });
            return;
        }
        self.overlay = Some(Overlay::Rewind { rows, sel: 0 });
    }

    fn do_rewind(&mut self, boundary: u64) {
        match overseer_core::rewind::restore(
            &self.session_dir,
            Some(boundary),
            overseer_core::rewind::Mode::Both,
        ) {
            Ok(rep) => {
                self.pending.push(Cell::Meta {
                    style: crate::theme::META,
                    text: format!(
                        "rewound to e{} — {} file(s) restored, {} removed, {} event(s) dropped",
                        rep.boundary, rep.restored, rep.deleted, rep.truncated
                    ),
                });
                // The log changed — rebuild the agent's context too.
                let _ = self.worker_tx.send(WorkerCmd::SwitchSession {
                    dir: self.session_dir.clone(),
                });
            }
            Err(e) => self.pending.push(Cell::Meta {
                style: crate::theme::ERROR,
                text: format!("rewind failed: {e}"),
            }),
        }
    }

    /// `/fork`: branch the session at head into a sibling dir and switch.
    fn do_fork(&mut self) {
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::ERROR,
                text: "finish or interrupt the run first".into(),
            });
            return;
        }
        let Some(root) = self.session_dir.parent().map(|p| p.to_path_buf()) else {
            return;
        };
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let new_dir = root.join(format!("{ts}"));
        match overseer_core::session::fork(&self.session_dir, None, &new_dir) {
            Ok(()) => {
                self.pending.push(Cell::Meta {
                    style: crate::theme::META,
                    text: format!("forked → {}", new_dir.display()),
                });
                let _ = self
                    .worker_tx
                    .send(WorkerCmd::SwitchSession { dir: new_dir });
            }
            Err(e) => self.pending.push(Cell::Meta {
                style: crate::theme::ERROR,
                text: format!("fork failed: {e}"),
            }),
        }
    }

    fn cycle_mode(&mut self) {
        if self.running() {
            return; // mode changes apply between turns, never mid-flight
        }
        self.preset = match self.preset {
            Preset::WorkspaceWrite => Preset::ReadOnly,
            Preset::ReadOnly => Preset::Plan,
            Preset::Plan => Preset::WorkspaceWrite,
        };
        let _ = self.worker_tx.send(WorkerCmd::SetPreset(self.preset));
    }

    fn on_submit(&mut self, text: String) {
        if let Some(cmd) = text.strip_prefix('/') {
            self.slash(cmd.trim());
            return;
        }
        match &self.run {
            RunState::Running { control, .. } => {
                // Mid-run input steers at the next tool-launch boundary.
                control.steer(text);
            }
            RunState::Idle => self.submit(text),
        }
    }

    fn submit(&mut self, text: String) {
        let control = Control::default();
        self.run = RunState::Running {
            control: control.clone(),
            started: Instant::now(),
            phase: "working".to_string(),
        };
        let _ = self.worker_tx.send(WorkerCmd::Submit { text, control });
    }

    fn slash(&mut self, cmd: &str) {
        match cmd {
            "help" | "?" => self.show_help = !self.show_help,
            "quit" | "exit" | "q" => self.quit = true,
            "clear" => self.composer.clear(),
            "sessions" | "resume" => self.open_sessions(),
            "rewind" => self.open_rewind(),
            "fork" => self.do_fork(),
            other => self.pending.push(Cell::Meta {
                style: crate::theme::ERROR,
                text: format!("unknown command /{other} — try /help"),
            }),
        }
    }

    // ── rendering ────────────────────────────────────────────────────

    /// Flush completed cells into native scrollback.
    fn flush_cells<B: ratatui::backend::Backend>(
        &mut self,
        term: &mut Terminal<B>,
        caps: &Caps,
    ) -> std::io::Result<()>
    where
        B::Error: std::fmt::Display,
    {
        if self.pending.is_empty() {
            return Ok(());
        }
        let width = term
            .size()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .width
            .max(1);
        for cell in self.pending.drain(..) {
            let lines = cell.lines(width);
            let h = lines.len() as u16;
            if h == 0 {
                continue;
            }
            sync_wrap(caps, || {
                term.insert_before(h, |buf| {
                    for (y, line) in lines.iter().enumerate() {
                        buf.set_line(0, y as u16, line, width);
                    }
                })
                .map_err(|e| std::io::Error::other(e.to_string()))
            })?;
        }
        Ok(())
    }

    fn live_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        // Running tools (cap: newest few stay visible).
        for c in self.live.iter().rev().take(4).rev() {
            out.extend(c.lines(width));
        }
        if let Some((dlg, _)) = &self.dialog {
            out.extend(dlg.lines(width));
        }
        let queued: Vec<String> = match &self.run {
            RunState::Running { control, .. } => control.queued(),
            RunState::Idle => Vec::new(),
        };
        out.extend(widgets::queue_strip(&queued));
        out.extend(widgets::queue_strip(&self.pending_queue));
        if let RunState::Running { started, phase, .. } = &self.run {
            out.push(widgets::indicator(
                phase,
                started.elapsed().as_secs(),
                self.tokens,
                self.tick,
            ));
        }
        if let Some(o) = &self.overlay {
            out.extend(overlay_lines(o, width));
        }
        if self.show_plan {
            if let Ok(md) = std::fs::read_to_string(self.session_dir.join("plan.md")) {
                for l in md.lines().take(8) {
                    out.push(Line::from(Span::styled(l.to_string(), crate::theme::META)));
                }
            } else {
                out.push(Line::from(Span::styled(
                    "no plan yet".to_string(),
                    crate::theme::DIM,
                )));
            }
        }
        if self.show_help {
            out.extend(widgets::help_panel());
        }
        out
    }

    fn draw<B: ratatui::backend::Backend>(
        &mut self,
        term: &mut Terminal<B>,
        caps: &Caps,
    ) -> std::io::Result<()>
    where
        B::Error: std::fmt::Display,
    {
        let width = term
            .size()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .width
            .max(1);
        let (composer_lines, (cx, cy)) = self.composer.render(width);
        let live = self.live_lines(width);
        let status = widgets::status_line(self.preset, &self.cwd, &self.model, self.cost, width);

        sync_wrap(caps, || {
            term.draw(|f| {
                // Layout is keyed off f.area() — the actual inline
                // viewport rect, not the terminal size.
                let area = f.area();
                let composer_rows = composer_lines.len().clamp(1, 4);
                let composer_clip = composer_lines.len().saturating_sub(composer_rows);
                let live_shown =
                    (area.height as usize).saturating_sub(composer_rows + 1);
                let live_start = live.len().saturating_sub(live_shown);
                let composer_top = live_shown as u16;
                let cy_screen = composer_top
                    + cy.saturating_sub(composer_clip as u16)
                        .min(composer_rows as u16 - 1);

                let mut lines: Vec<Line<'static>> =
                    Vec::with_capacity(area.height as usize);
                lines.extend(live[live_start..].iter().cloned());
                lines.extend(composer_lines[composer_clip..].iter().cloned());
                lines.push(status.clone());
                f.render_widget(Paragraph::new(lines), area);
                f.set_cursor_position((area.x + cx, area.y + cy_screen));
            })
            .map(|_| ())
            .map_err(|e: B::Error| std::io::Error::other(e.to_string()))
        })?;
        self.dirty = false;
        Ok(())
    }
}

/// (selection index, last valid index) for the open overlay.
fn overlay_sel(o: &mut Overlay) -> (&mut usize, usize) {
    match o {
        Overlay::Sessions {
            rows, sel, filter, ..
        } => (
            sel,
            filtered_sessions(rows, filter).len().saturating_sub(1),
        ),
        Overlay::Rewind { rows, sel } => (sel, rows.len().saturating_sub(1)),
    }
}

/// Fuzzy filter: subsequence match over id + cwd + preview text.
fn filtered_sessions<'a>(
    rows: &'a [overseer_core::session::SessionInfo],
    filter: &str,
) -> Vec<&'a overseer_core::session::SessionInfo> {
    rows.iter()
        .filter(|s| {
            let hay = format!(
                "{} {} {}",
                s.id,
                s.cwd,
                s.first_user.as_deref().unwrap_or("")
            );
            subseq_match(&hay, filter)
        })
        .collect()
}

fn subseq_match(hay: &str, needle: &str) -> bool {
    let mut it = hay.chars().flat_map(char::to_lowercase);
    for nc in needle.chars().flat_map(char::to_lowercase) {
        loop {
            match it.next() {
                Some(hc) if hc == nc => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

/// Render an open overlay picker into live-region lines.
fn overlay_lines(o: &Overlay, _width: u16) -> Vec<Line<'static>> {
    use crate::theme;
    match o {
        Overlay::Sessions {
            rows,
            sel,
            filter,
            wide,
        } => {
            let mut out = vec![
                Line::from(Span::styled(
                    "sessions — type to filter · ↑↓ select · tab preview · enter switch · esc close",
                    theme::DIM,
                )),
                Line::from(vec![
                    Span::styled("> ", theme::DIALOG_KEY),
                    Span::styled(format!("{filter}▌"), theme::DIALOG),
                ]),
            ];
            let shown = filtered_sessions(rows, filter);
            if shown.is_empty() {
                out.push(Line::from(Span::styled(
                    "  no matching sessions".to_string(),
                    theme::DIM,
                )));
            }
            for (i, s) in shown.iter().take(6).enumerate() {
                let cur = i == *sel;
                let mark = if cur { "›" } else { " " };
                let when = rel_time(s.last_ms);
                let head = format!(
                    "{mark} {when} · {} · {}",
                    short_id(&s.id),
                    s.first_user.as_deref().unwrap_or("(no prompt)")
                );
                out.push(Line::from(Span::styled(
                    head,
                    if cur { theme::DIALOG_SEL } else { theme::DIALOG },
                )));
                if *wide {
                    out.push(Line::from(Span::styled(
                        format!(
                            "    {} · {} · {} events · {} checkpoint(s)",
                            s.cwd,
                            s.model,
                            s.events,
                            s.checkpoints.len()
                        ),
                        theme::DIM,
                    )));
                }
            }
            out
        }
        Overlay::Rewind { rows, sel } => {
            let mut out = vec![Line::from(Span::styled(
                "rewind — ↑↓ select · enter restore files+conversation · esc close",
                theme::DIM,
            ))];
            for (i, r) in rows.iter().take(6).enumerate() {
                let cur = i == *sel;
                let mark = if cur { "›" } else { " " };
                out.push(Line::from(Span::styled(
                    format!("{mark} e{} · {} file(s) · {}", r.boundary, r.files, r.label),
                    if cur { theme::DIALOG_SEL } else { theme::DIALOG },
                )));
            }
            out
        }
    }
}

fn short_id(id: &str) -> String {
    if id.len() > 12 {
        format!("…{}", &id[id.len() - 10..])
    } else {
        id.to_string()
    }
}

fn rel_time(ts_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let ago = now.saturating_sub(ts_ms) / 1000;
    if ago < 60 {
        format!("{ago}s")
    } else if ago < 3600 {
        format!("{}m", ago / 60)
    } else if ago < 86400 {
        format!("{}h", ago / 3600)
    } else {
        format!("{}d", ago / 86400)
    }
}

/// Files recorded in checkpoint e<N>'s manifest (the picker's count).
fn manifest_files(session_dir: &std::path::Path, boundary: u64) -> u32 {
    let p = session_dir
        .join("checkpoints")
        .join(format!("e{boundary}"))
        .join("manifest.jsonl");
    std::fs::read_to_string(p)
        .map(|m| m.lines().count() as u32)
        .unwrap_or(0)
}

/// Wrap a draw/insert in synchronized-update markers when probed —
/// the terminal holds the frame until ESU → atomic flip, no flicker.
fn sync_wrap(caps: &Caps, f: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    if caps.sync_output {
        out.write_all(probe::BSU.as_bytes())?;
    }
    let r = f();
    if caps.sync_output {
        out.write_all(probe::ESU.as_bytes())?;
        out.flush()?;
    }
    r
}
