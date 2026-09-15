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

/// Modal overlay pickers (P2.6/P2.9). Unlike the permission dialog these
/// own the keyboard — the sessions filter IS a text input by design.
pub enum Overlay {
    Sessions {
        rows: Vec<overseer_core::session::SessionInfo>,
        /// Git branch per row's cwd (computed once at open — spawning
        /// `git` per frame would stall the picker).
        branches: Vec<Option<String>>,
        sel: usize,
        filter: String,
        /// false = one line per session; true = adds a preview line.
        wide: bool,
    },
    /// `/tree`: the fork forest — parents above children, indented.
    Tree {
        rows: Vec<(overseer_core::session::SessionInfo, usize)>,
        sel: usize,
    },
    Rewind {
        rows: Vec<RewindRow>,
        sel: usize,
    },
    /// Ctrl+O: pager over the whole transcript with substring search.
    Transcript {
        query: String,
        /// First visible flattened line.
        scroll: usize,
        /// `x`-key toggle: render tool output under each tool cell.
        expand_tools: bool,
    },
    /// `/diff`: files touched by write/edit since their first checkpoint.
    Diff {
        rows: Vec<DiffRow>,
        sel: usize,
        /// Show the selected row's diff inline.
        preview: bool,
        /// Selected hunk within `rows[sel]` (preview mode only).
        hunk_sel: usize,
    },
}

/// One `/diff` row: a checkpoint-tracked path vs its working-tree state.
pub struct DiffRow {
    pub path: std::path::PathBuf,
    /// "modified" | "created" | "deleted"
    pub status: &'static str,
    /// Earliest checkpoint content (None = the file is agent-created).
    pub snapshot: Option<String>,
    /// Structured diff — drives the preview AND per-hunk reject.
    pub hunks: Vec<crate::diff::Hunk>,
    /// rejected[i] = restore hunk i's old lines on Enter.
    pub rejected: Vec<bool>,
    pub added: usize,
    pub deleted: usize,
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
    /// Every committed cell (post-flush) — the transcript overlay's
    /// search corpus. Cleared on session switch (the reseed repopulates).
    history: Vec<Cell>,
    /// Transient notice row (rule saved, file reverted, …); expires in
    /// `step` after a few seconds.
    toast: Option<(String, std::time::Instant)>,
    /// DECSET-1004 focus state — notifications only fire unfocused.
    focused: bool,
    /// Emit OSC sequences (8/52/133) — set from `caps.osc` by `run`.
    pub osc: bool,
    /// REDUCE_MOTION: static indicator glyph, no spinner animation.
    reduce_motion: bool,
    /// `/edit` / Alt+E: draft handed to `drive` for an $EDITOR round.
    want_editor: Option<String>,
    /// Workspace file list for `@` completion — built lazily, capped.
    file_index: std::cell::OnceCell<Vec<String>>,
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
    /// Engine channel hit the per-frame drain cap — zero-wait polling
    /// until cleared so input echo stays under 50 ms during bursts.
    backlogged: bool,
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
            history: Vec::new(),
            toast: None,
            focused: true,
            osc: false,
            reduce_motion: std::env::var_os("REDUCE_MOTION").is_some()
                || std::env::var_os("OVERSEER_REDUCED_MOTION").is_some(),
            want_editor: None,
            file_index: std::cell::OnceCell::new(),
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
            backlogged: false,
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
        // Toasts expire on their own clock.
        if self
            .toast
            .as_ref()
            .map(|(_, t)| t.elapsed().as_secs() >= 4)
            .unwrap_or(false)
        {
            self.toast = None;
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

    /// Test/debug hook: synthesize a bracketed paste.
    pub fn paste(&mut self, text: &str) {
        self.composer.paste(text);
    }

    // ── engine messages ──────────────────────────────────────────────

    fn drain_engine(&mut self) {
        // Bound per-frame drain: a 500-event burst must not starve the
        // input loop for a whole frame batch (echo <50 ms invariant).
        // Leftovers stay queued — dirty stays set so the next frame
        // continues the drain immediately.
        const DRAIN_CAP: usize = 128;
        let mut n = 0;
        for _ in 0..DRAIN_CAP {
            let Ok(msg) = self.engine_rx.try_recv() else {
                break;
            };
            n += 1;
            self.dirty = true;
            match msg {
                EngineMsg::Event(ev) => self.on_event(&ev),
                EngineMsg::Ask(req, tx) => {
                    self.notify(
                        "overseer: permission",
                        &format!("{} needs approval", req.tool),
                    );
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
                        style: crate::theme::error(),
                        text: format!("engine error: {e}"),
                    });
                }
            }
        }
        // Hit the cap → more is probably queued (one false positive at
        // exactly DRAIN_CAP costs one zero-wait poll — harmless).
        self.backlogged = n == DRAIN_CAP;
    }

    /// True while engine messages remain undrained — the event loop
    /// skips the poll sleep so backpressure drains at full rate.
    pub fn engine_backlog(&self) -> bool {
        self.backlogged
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
        let done_kind = match &out {
            O::Interrupted { .. } => "interrupted",
            O::Provider(_) => "provider error",
            _ => "done",
        };
        self.notify("overseer", &format!("run {done_kind}"));
        if let O::Provider(msg) = &out {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
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
        self.history.clear();
        self.tokens = 0;
        self.file_index = std::cell::OnceCell::new();
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string());
        self.pending.push(Cell::Meta {
            style: crate::theme::meta(),
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
        // Backlogged engine events never wait on the keyboard — drain
        // at full rate (the DRAIN_CAP keeps each frame bounded so input
        // still interleaves). Otherwise spinner cadence ~10 fps while
        // running; near-idle otherwise.
        let wait = if self.engine_backlog() {
            Duration::ZERO
        } else if self.running() || self.dialog.is_some() || self.toast.is_some() {
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
            CtEvent::FocusGained => self.focused = true,
            CtEvent::FocusLost => self.focused = false,
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
                    if m.is_empty() && matches!(c, '1' | '2' | '3' | '4') =>
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
                    if d == AskDecision::AllowAlways {
                        self.set_toast("rule saved to ~/.overseer/rules".into());
                    }
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
            (KeyCode::Char('o'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.open_transcript();
            }
            (KeyCode::Char('y'), m) if m.contains(KeyModifiers::CONTROL) => self.copy_last(),
            (KeyCode::Char('e'), m) if m.contains(KeyModifiers::ALT) => {
                self.want_editor = Some(self.composer.text());
            }
            (KeyCode::Tab, _) => self.complete(),
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
            (KeyCode::Up, _) => match &mut self.overlay {
                Some(Overlay::Transcript { scroll, .. }) => {
                    *scroll = scroll.saturating_sub(1);
                }
                Some(o) => {
                    let (sel, len) = overlay_sel(o);
                    *sel = sel.saturating_sub(1).min(len);
                }
                None => {}
            },
            (KeyCode::Down, _) => match &mut self.overlay {
                Some(Overlay::Transcript { scroll, .. }) => {
                    *scroll = scroll.saturating_add(1);
                }
                Some(o) => {
                    let (sel, len) = overlay_sel(o);
                    *sel = (*sel + 1).min(len);
                }
                None => {}
            },
            (KeyCode::PageUp, _) => {
                if let Some(Overlay::Transcript { scroll, .. }) = &mut self.overlay {
                    *scroll = scroll.saturating_sub(8);
                }
            }
            (KeyCode::PageDown, _) => {
                if let Some(Overlay::Transcript { scroll, .. }) = &mut self.overlay {
                    *scroll = scroll.saturating_add(8);
                }
            }
            (KeyCode::Tab, _) => match &mut self.overlay {
                Some(Overlay::Sessions { wide, .. }) => *wide = !*wide,
                Some(Overlay::Diff {
                    preview, hunk_sel, ..
                }) => {
                    *preview = !*preview;
                    *hunk_sel = 0;
                }
                Some(Overlay::Transcript { expand_tools, .. }) => {
                    *expand_tools = !*expand_tools;
                }
                _ => {}
            },
            (KeyCode::Left, _) | (KeyCode::Right, _) => {
                if let Some(Overlay::Diff {
                    rows,
                    sel,
                    preview,
                    hunk_sel,
                }) = &mut self.overlay
                {
                    if *preview {
                        let n = rows.get(*sel).map(|r| r.hunks.len()).unwrap_or(0);
                        if n > 0 {
                            if key.code == KeyCode::Left {
                                *hunk_sel = hunk_sel.saturating_sub(1);
                            } else {
                                *hunk_sel = (*hunk_sel + 1).min(n - 1);
                            }
                        }
                    }
                }
            }
            (KeyCode::Char(' '), _) => {
                if let Some(Overlay::Diff {
                    rows,
                    sel,
                    preview,
                    hunk_sel,
                }) = &mut self.overlay
                {
                    if *preview {
                        if let Some(r) = rows.get_mut(*sel) {
                            if let Some(flag) = r.rejected.get_mut(*hunk_sel) {
                                *flag = !*flag;
                            }
                        }
                    }
                }
            }
            (KeyCode::Enter, _) => self.overlay_confirm(),
            (KeyCode::Backspace, _) => match &mut self.overlay {
                Some(Overlay::Sessions { filter, sel, .. }) => {
                    filter.pop();
                    *sel = 0;
                }
                Some(Overlay::Transcript { query, scroll, .. }) => {
                    query.pop();
                    *scroll = 0;
                }
                _ => {}
            },
            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => match &mut self.overlay
            {
                Some(Overlay::Sessions { filter, sel, .. }) => {
                    filter.push(c);
                    *sel = 0;
                }
                Some(Overlay::Transcript { query, scroll, .. }) => {
                    query.push(c);
                    *scroll = 0;
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn overlay_confirm(&mut self) {
        match self.overlay.take() {
            Some(Overlay::Sessions { rows, sel, filter, .. }) => {
                if let Some((_, info)) = filtered_sessions(&rows, &filter).get(sel) {
                    let info = (*info).clone();
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
            Some(Overlay::Tree { rows, sel }) => {
                if let Some((info, _)) = rows.get(sel) {
                    if info.dir != self.session_dir {
                        let _ = self.worker_tx.send(WorkerCmd::SwitchSession {
                            dir: info.dir.clone(),
                        });
                    }
                }
            }
            Some(Overlay::Diff { rows, sel, .. }) => {
                if let Some(row) = rows.into_iter().nth(sel) {
                    self.revert_file(&row);
                }
            }
            _ => {}
        }
    }

    /// `/sessions` (or Ctrl+P): the picker only opens between runs —
    /// switching mid-run would orphan its control/ask state.
    fn open_sessions(&mut self) {
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
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
        let branches = rows.iter().map(|s| git_branch(&s.cwd)).collect();
        self.overlay = Some(Overlay::Sessions {
            rows,
            branches,
            sel: 0,
            filter: String::new(),
            wide: false,
        });
    }

    /// `/tree`: the fork forest — sessions ordered parents-first.
    fn open_tree(&mut self) {
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: "finish or interrupt the run first".into(),
            });
            return;
        }
        let Some(root) = self.session_dir.parent().map(|p| p.to_path_buf()) else {
            return;
        };
        let rows = overseer_core::session::tree(&root);
        if rows.is_empty() {
            return;
        }
        // Preselect the current session so the user sees where they are.
        let sel = rows
            .iter()
            .position(|(s, _)| s.dir == self.session_dir)
            .unwrap_or(0);
        self.overlay = Some(Overlay::Tree { rows, sel });
    }

    /// `/rewind`: checkpoints of the current session with their labels.
    fn open_rewind(&mut self) {
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
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
                style: crate::theme::dim(),
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
                    style: crate::theme::meta(),
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
                style: crate::theme::error(),
                text: format!("rewind failed: {e}"),
            }),
        }
    }

    /// Ctrl+O / `/search`: pager over the whole transcript. The search
    /// query is a plain case-insensitive substring over cell text.
    fn open_transcript(&mut self) {
        self.overlay = Some(Overlay::Transcript {
            query: String::new(),
            scroll: usize::MAX, // clamped to the tail on first render
            expand_tools: false,
        });
    }

    /// `/diff`: every path a checkpoint manifest records, earliest
    /// snapshot vs the working tree. Bash side effects stay invisible —
    /// the same blind spot rewind documents.
    fn open_diff(&mut self) {
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: "finish or interrupt the run first".into(),
            });
            return;
        }
        let rows = collect_diff_rows(&self.session_dir);
        if rows.is_empty() {
            self.pending.push(Cell::Meta {
                style: crate::theme::dim(),
                text: "no tracked changes — checkpoints record write/edit only".into(),
            });
            return;
        }
        self.overlay = Some(Overlay::Diff {
            rows,
            sel: 0,
            preview: false,
            hunk_sel: 0,
        });
    }

    /// Revert one `/diff` row: marked hunks (rejects) restore just their
    /// old lines; no marks = whole file back to its earliest snapshot.
    /// Current content is stashed under checkpoints/revert-stash first —
    /// a revert is itself recoverable.
    fn revert_file(&mut self, row: &DiffRow) {
        if let Ok(cur) = std::fs::read_to_string(&row.path) {
            let stash = self
                .session_dir
                .join("checkpoints/revert-stash")
                .join(row.path.file_name().unwrap_or_default());
            if let Some(p) = stash.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            let _ = std::fs::write(&stash, cur);
        }
        let nrej = row.rejected.iter().filter(|x| **x).count();
        // Partial revert needs the file to exist now; a "created" row's
        // reject-all collapses to the full revert (delete) anyway.
        let partial = nrej > 0 && row.snapshot.is_some();
        let ok = if partial {
            match std::fs::read_to_string(&row.path) {
                Ok(cur) => {
                    let out = crate::diff::apply_rejects(&cur, &row.hunks, &row.rejected);
                    std::fs::write(&row.path, out).is_ok()
                }
                Err(_) => false,
            }
        } else {
            match &row.snapshot {
                Some(snap) => {
                    if let Some(p) = row.path.parent() {
                        let _ = std::fs::create_dir_all(p);
                    }
                    std::fs::write(&row.path, snap).is_ok()
                }
                None => std::fs::remove_file(&row.path).is_ok(),
            }
        };
        // Basename only — the diff row above already carries the path.
        let shown = row
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| display_path(&self.cwd, &row.path));
        if ok {
            let what = if partial {
                format!("rejected {nrej} hunk(s) in {shown}")
            } else {
                format!("reverted {shown}")
            };
            self.set_toast(format!("{what} (stash in revert-stash)"));
        } else {
            self.set_toast(format!("revert failed for {shown}"));
        }
    }

    /// `/approve`: leave plan mode and tell the agent to implement.
    fn approve_plan(&mut self) {
        if self.preset != Preset::Plan {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: "not in plan mode (shift+tab to cycle)".into(),
            });
            return;
        }
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: "finish or interrupt the run first".into(),
            });
            return;
        }
        if !self.session_dir.join("plan.md").exists() {
            self.pending.push(Cell::Meta {
                style: crate::theme::dim(),
                text: "no plan yet — ask the agent for one first".into(),
            });
            return;
        }
        self.preset = Preset::WorkspaceWrite;
        let _ = self.worker_tx.send(WorkerCmd::SetPreset(self.preset));
        self.pending.push(Cell::Meta {
            style: crate::theme::meta(),
            text: "plan approved — switching to workspace mode".into(),
        });
        self.submit("The plan is approved — implement it.".to_string());
    }

    fn set_toast(&mut self, text: String) {
        self.toast = Some((text, std::time::Instant::now()));
    }

    /// `/fork`: branch the session at head into a sibling dir and switch.
    fn do_fork(&mut self) {
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
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
                    style: crate::theme::meta(),
                    text: format!("forked → {}", new_dir.display()),
                });
                let _ = self
                    .worker_tx
                    .send(WorkerCmd::SwitchSession { dir: new_dir });
            }
            Err(e) => self.pending.push(Cell::Meta {
                style: crate::theme::error(),
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

    /// OSC notification — focus-gated: only fires while the terminal
    /// doesn't have focus (DECSET 1004). No-op in tests (osc off).
    fn notify(&self, title: &str, body: &str) {
        if self.osc && !self.focused {
            crate::notify::emit(&mut std::io::stdout(), title, body);
        }
    }

    /// Ctrl+Y: copy the most recent assistant text via OSC 52.
    fn copy_last(&mut self) {
        let text = self
            .history
            .iter()
            .rev()
            .find_map(|c| match c {
                Cell::Assistant { text } => Some(text.clone()),
                _ => None,
            });
        match text {
            Some(t) if self.osc => {
                let n = t.chars().count();
                let mut out = std::io::stdout();
                let _ = out.write_all(crate::notify::osc52(&t).as_bytes());
                let _ = out.flush();
                self.set_toast(format!("copied {n} chars"));
            }
            Some(_) => self.set_toast("clipboard needs OSC support".into()),
            None => self.set_toast("nothing to copy yet".into()),
        }
    }

    /// Tab: complete `/command` or `@path` from the current token.
    /// One match completes fully (with trailing space); several complete
    /// to their longest common prefix — the suggestion strip shows the
    /// menu meanwhile.
    fn complete(&mut self) {
        let text = self.composer.text();
        let (stem, frag, candidates) = if let Some(frag) = text.strip_prefix('/') {
            if frag.contains(char::is_whitespace) {
                return;
            }
            (
                "/".to_string(),
                frag.to_string(),
                COMMANDS
                    .iter()
                    .filter(|(n, _)| subseq_match(n, frag))
                    .map(|(n, _)| n.to_string())
                    .collect::<Vec<_>>(),
            )
        } else if let Some(frag) = at_fragment(&text) {
            (
                text[..text.len() - frag.len()].to_string(),
                frag.to_string(),
                self.file_index()
                    .iter()
                    .filter(|p| subseq_match(p, frag))
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        } else {
            return;
        };
        match candidates.len() {
            0 => {}
            1 => self
                .composer
                .set_text(&format!("{stem}{} ", candidates[0])),
            _ => {
                let lcp = candidates
                    .iter()
                    .skip(1)
                    .fold(candidates[0].clone(), |acc, c| common_prefix(&acc, c));
                if lcp.len() > frag.len() {
                    self.composer.set_text(&format!("{stem}{lcp}"));
                }
            }
        }
    }

    /// Workspace files for `@` completion — recursive, skips heavy
    /// dirs, capped. Built lazily on first use (switches reset it).
    fn file_index(&self) -> &[String] {
        self.file_index.get_or_init(|| self.build_index()).as_slice()
    }

    fn build_index(&self) -> Vec<String> {
        const SKIP: &[&str] = &[".git", "target", "node_modules", ".overseer"];
        let mut out = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(&self.cwd)];
        while let Some(d) = stack.pop() {
            if out.len() >= 4000 {
                break;
            }
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let p = e.path();
                if p.is_dir() {
                    if !SKIP.contains(&name.as_str()) && !name.starts_with('.') {
                        stack.push(p);
                    }
                } else if let Ok(rel) = p.strip_prefix(&self.cwd) {
                    out.push(rel.display().to_string());
                }
            }
        }
        out.sort();
        out
    }

    /// `drive` polls this: the draft to round-trip through $EDITOR.
    pub fn take_editor_request(&mut self) -> Option<String> {
        self.want_editor.take()
    }

    /// `drive` installs the edited draft back into the composer.
    pub fn set_composer_text(&mut self, text: String) {
        self.composer.set_text(&text);
        self.dirty = true;
    }

    /// `!cmd`: run in the workspace shell, output lands in the
    /// transcript — never sent to the model. 10 s cap, 8 KiB output.
    fn run_shell(&mut self, cmd: &str) {
        self.pending.push(Cell::Meta {
            style: crate::theme::meta(),
            text: format!("$ {cmd}"),
        });
        match shell_capture(cmd, &self.cwd) {
            Ok((code, out)) => {
                let tail: String = out
                    .lines()
                    .take(24)
                    .collect::<Vec<_>>()
                    .join("\n");
                let suffix = if out.len() > 8192 { "…" } else { "" };
                self.pending.push(Cell::Meta {
                    style: if code == 0 {
                        crate::theme::dim()
                    } else {
                        crate::theme::error()
                    },
                    text: format!("{tail}{suffix}\n(exit {code})"),
                });
            }
            Err(e) => self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: format!("shell failed: {e}"),
            }),
        }
    }

    fn on_submit(&mut self, text: String) {
        if let Some(cmd) = text.strip_prefix('/') {
            self.slash(cmd.trim());
            return;
        }
        if let Some(cmd) = text.strip_prefix('!') {
            self.run_shell(cmd.trim());
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
            "tree" => self.open_tree(),
            "rewind" => self.open_rewind(),
            "fork" => self.do_fork(),
            "diff" => self.open_diff(),
            "approve" => self.approve_plan(),
            "search" | "transcript" => self.open_transcript(),
            "edit" | "editor" => self.want_editor = Some(self.composer.text()),
            other => self.pending.push(Cell::Meta {
                style: crate::theme::error(),
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
        for cell in std::mem::take(&mut self.pending) {
            self.history.push(cell.clone());
            let lines = cell.lines(width);
            let h = lines.len() as u16;
            if h == 0 {
                continue;
            }
            // OSC 133 marks ride the scrollback stream: A/B bracket the
            // user prompt, C/D bracket finished tool output — terminal
            // "jump to prompt" and output-select features work on the
            // transcript. Emitted raw; the live region can't carry OSC.
            let (pre, post) = Self::osc133_marks(self.osc, &cell);
            sync_wrap(caps, || {
                if let Some(m) = pre {
                    let _ = std::io::stdout().write_all(m.as_bytes());
                }
                term.insert_before(h, |buf| {
                    for (y, line) in lines.iter().enumerate() {
                        buf.set_line(0, y as u16, line, width);
                    }
                })
                .map_err(|e| std::io::Error::other(e.to_string()))?;
                if let Some(m) = post {
                    let _ = std::io::stdout().write_all(m.as_bytes());
                }
                Ok(())
            })?;
            // OSC 8: a clickable file:// link line under the cell —
            // styled paths can't ride the ratatui buffer, so the link
            // is its own line.
            if self.osc {
                if let Some(p) = cell.link_path() {
                    let shown = display_path(&self.cwd, p);
                    let url = format!("file://{}", p.display());
                    let mut out = std::io::stdout();
                    let _ = out
                        .write_all(format!("  ⤷ {}\n", crate::notify::osc8(&url, &shown)).as_bytes());
                    let _ = out.flush();
                }
            }
        }
        Ok(())
    }

    /// (pre, post) OSC 133 mark for a cell — None when OSC is off.
    fn osc133_marks(osc: bool, cell: &Cell) -> (Option<String>, Option<String>) {
        if !osc {
            return (None, None);
        }
        use crate::notify::osc133;
        match cell {
            Cell::User { .. } => (Some(osc133("A")), Some(osc133("B"))),
            Cell::Tool { status, .. } => {
                let code = match status {
                    ToolStatus::Ok => 0,
                    _ => 1,
                };
                (Some(osc133("C")), Some(osc133(&format!("D;{code}"))))
            }
            _ => (None, None),
        }
    }

    fn live_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        // A modal overlay owns the whole live region — nothing else
        // competes for its rows. An open permission dialog outranks it
        // (the engine is blocked on that answer).
        if self.dialog.is_none() {
            if let Some(o) = &self.overlay {
                out.extend(self.overlay_lines(o, width));
                if let Some((text, _)) = &self.toast {
                    out.push(Line::from(Span::styled(
                        format!("◆ {text}"),
                        crate::theme::meta(),
                    )));
                }
                return out;
            }
        }
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
        // `/` and `@` completion strip — visible while a fragment is
        // being typed, completed by Tab.
        out.extend(self.suggestion_lines());
        if let RunState::Running { started, phase, .. } = &self.run {
            out.push(widgets::indicator(
                phase,
                started.elapsed().as_secs(),
                self.tokens,
                self.tick,
                self.reduce_motion,
            ));
        }
        if let Some((text, _)) = &self.toast {
            out.push(Line::from(Span::styled(
                format!("◆ {text}"),
                crate::theme::meta(),
            )));
        }
        if self.show_plan {
            if let Ok(md) = std::fs::read_to_string(self.session_dir.join("plan.md")) {
                for l in md.lines().take(8) {
                    out.push(Line::from(Span::styled(l.to_string(), crate::theme::meta())));
                }
            } else {
                out.push(Line::from(Span::styled(
                    "no plan yet".to_string(),
                    crate::theme::dim(),
                )));
            }
        }
        if self.show_help {
            out.extend(widgets::help_panel());
        }
        out
    }

    /// Completion menu above the composer: `/cmd` or `@path` fragment.
    fn suggestion_lines(&self) -> Vec<Line<'static>> {
        let text = self.composer.text();
        let rows: Vec<String> = if let Some(frag) = text.strip_prefix('/') {
            if frag.contains(char::is_whitespace) {
                return Vec::new();
            }
            COMMANDS
                .iter()
                .filter(|(n, _)| subseq_match(n, frag))
                .take(4)
                .map(|(n, d)| format!("/{n} — {d}"))
                .collect()
        } else if let Some(frag) = at_fragment(&text) {
            self.file_index()
                .iter()
                .filter(|p| subseq_match(p, frag))
                .take(4)
                .map(|p| format!("@{p}"))
                .collect()
        } else {
            return Vec::new();
        };
        if rows.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<Line<'static>> = rows
            .into_iter()
            .map(|r| Line::from(Span::styled(r, crate::theme::dim())))
            .collect();
        out.push(Line::from(Span::styled(
            "tab to complete",
            crate::theme::dim(),
        )));
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
        Overlay::Tree { rows, sel } => (sel, rows.len().saturating_sub(1)),
        Overlay::Diff { rows, sel, .. } => (sel, rows.len().saturating_sub(1)),
        // Transcript scrolls lines, not rows — handled by its own arms.
        Overlay::Transcript { scroll, .. } => (scroll, usize::MAX),
    }
}

/// Fuzzy filter: subsequence match over id + cwd + preview text.
/// Returns (row-index, session) pairs so parallel metadata (branches)
/// still lines up after filtering.
fn filtered_sessions<'a>(
    rows: &'a [overseer_core::session::SessionInfo],
    filter: &str,
) -> Vec<(usize, &'a overseer_core::session::SessionInfo)> {
    rows.iter()
        .enumerate()
        .filter(|(_, s)| {
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

/// Current branch of a session's recorded cwd — `git branch
/// --show-current`, None outside a repo or on any failure. Computed
/// once per picker open, never per frame.
fn git_branch(cwd: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", cwd, "branch", "--show-current"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if b.is_empty() {
        None
    } else {
        Some(b)
    }
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

/// `/` menu entries — canonical names + one-line docs (the help panel
/// and the completion strip share this list).
const COMMANDS: &[(&str, &str)] = &[
    ("help", "keys & commands"),
    ("quit", "exit"),
    ("sessions", "pick a session to resume"),
    ("tree", "session fork tree"),
    ("rewind", "restore a checkpoint"),
    ("fork", "branch this session"),
    ("diff", "changed files vs checkpoints"),
    ("approve", "accept the plan, switch to workspace mode"),
    ("search", "transcript search"),
    ("transcript", "transcript search"),
    ("edit", "draft in $EDITOR"),
    ("clear", "clear the composer"),
];

/// The `@`-fragment the cursor sits on: the tail after the last `@`,
/// only when that `@` starts a token and the tail has no whitespace.
fn at_fragment(text: &str) -> Option<&str> {
    let pos = text.rfind('@')?;
    if pos > 0 && !text[..pos].ends_with(char::is_whitespace) {
        return None; // '@' mid-token — an email or literal, not a mention
    }
    let frag = &text[pos + 1..];
    if frag.contains(char::is_whitespace) {
        return None;
    }
    Some(frag)
}

fn common_prefix(a: &str, b: &str) -> String {
    a.chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect()
}

/// `!` in line mode (`run_line`) — `shell_capture` against the cwd.
pub fn line_shell(cmd: &str) -> (i32, String) {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".into());
    shell_capture(cmd, &cwd).unwrap_or((-1, "shell failed\n".to_string()))
}

/// Run `sh -c cmd` in `cwd` with a 10 s cap; returns (exit, capped
/// output). The `!` composer prefix is a local escape hatch — its
/// output is transcript-only, never submitted to the model.
fn shell_capture(cmd: &str, cwd: &str) -> std::io::Result<(i32, String)> {
    // A stale cwd (deleted checkout) must not kill the shell escape —
    // fall back to the process cwd.
    let dir = if std::path::Path::new(cwd).is_dir() {
        cwd
    } else {
        "."
    };
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr).into_owned();
            if !err.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            Ok((
                out.status.code().unwrap_or(-1),
                text.chars().take(8192).collect(),
            ))
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Ok((-1, "(timed out after 10s)".to_string())),
    }
}

/// Filtered transcript lines (history + in-flight live cells), used by
/// both the overlay's height math and its render window.
fn transcript_lines(
    app: &App,
    query: &str,
    width: u16,
    expand_tools: bool,
) -> Vec<Line<'static>> {
    let q = query.to_lowercase();
    app.history
        .iter()
        .chain(app.live.iter())
        .filter(|c| q.is_empty() || c.plain().to_lowercase().contains(&q))
        .flat_map(|c| {
            if expand_tools {
                c.lines_expanded(width)
            } else {
                c.lines(width)
            }
        })
        .collect()
}

/// Files tracked by checkpoint manifests → `/diff` rows (earliest
/// snapshot vs current working-tree content).
fn collect_diff_rows(session_dir: &std::path::Path) -> Vec<DiffRow> {
    // First-seen manifest entry per path: (existed, stored, checkpoint dir).
    let mut seen: std::collections::HashMap<std::path::PathBuf, (bool, String, std::path::PathBuf)> =
        std::collections::HashMap::new();
    for b in overseer_core::session::checkpoints(session_dir) {
        let cp = session_dir.join("checkpoints").join(format!("e{b}"));
        let Ok(m) = std::fs::read_to_string(cp.join("manifest.jsonl")) else {
            continue;
        };
        for line in m.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let (Some(path), Some(stored)) = (
                v.get("path").and_then(|p| p.as_str()),
                v.get("stored").and_then(|s| s.as_str()),
            ) else {
                continue;
            };
            let existed = v.get("existed").and_then(|e| e.as_bool()).unwrap_or(false);
            seen.entry(std::path::PathBuf::from(path))
                .or_insert((existed, stored.to_string(), cp.clone()));
        }
    }
    const CAP: usize = 400; // LCS is O(n·m) — cap the compared prefix
    let mut rows = Vec::new();
    for (path, (existed, stored, cp)) in seen {
        let snapshot: Option<String> = if existed {
            std::fs::read_to_string(cp.join("files").join(&stored)).ok()
        } else {
            None
        };
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        let status = match (existed, std::path::Path::new(&path).exists()) {
            (false, true) => "created",
            (true, false) => "deleted",
            (_, true) => "modified",
            (false, false) => continue, // created then removed — no change
        };
        let old: String = snapshot.clone().unwrap_or_default();
        let hs = crate::diff::hunks(
            &old.lines().take(CAP).collect::<Vec<_>>().join("\n"),
            &current.lines().take(CAP).collect::<Vec<_>>().join("\n"),
            3,
        );
        if hs.is_empty() {
            continue;
        }
        let added = hs.iter().map(|h| h.added).sum();
        let deleted = hs.iter().map(|h| h.deleted).sum();
        let rejected = vec![false; hs.len()];
        rows.push(DiffRow {
            path,
            status,
            snapshot,
            hunks: hs,
            rejected,
            added,
            deleted,
        });
    }
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    rows
}

/// Render an open overlay picker into live-region lines.
impl App {
    fn overlay_lines(&self, o: &Overlay, width: u16) -> Vec<Line<'static>> {
        use crate::theme;
        match o {
        Overlay::Sessions {
            rows,
            branches,
            sel,
            filter,
            wide,
        } => {
            // Hint goes LAST — the region clips from the top, so the
            // key hints survive regardless of viewport height.
            let mut out = vec![Line::from(vec![
                Span::styled("> ", theme::dialog_key()),
                Span::styled(format!("{filter}▌"), theme::dialog()),
            ])];
            let shown = filtered_sessions(rows, filter);
            if shown.is_empty() {
                out.push(Line::from(Span::styled(
                    "  no matching sessions".to_string(),
                    theme::dim(),
                )));
            }
            for (i, (idx, s)) in shown.iter().take(6).enumerate() {
                let cur = i == *sel;
                let mark = if cur { "›" } else { " " };
                let when = rel_time(s.last_ms);
                // ⤶ marks a fork (SessionStart.parent set).
                let fork = if s.parent.is_some() { "⤶ " } else { "" };
                let head = format!(
                    "{mark} {when} · {}{} · {}",
                    fork,
                    short_id(&s.id),
                    s.first_user.as_deref().unwrap_or("(no prompt)")
                );
                out.push(Line::from(Span::styled(
                    head,
                    if cur { theme::dialog_sel() } else { theme::dialog() },
                )));
                if *wide {
                    let branch = branches
                        .get(*idx)
                        .and_then(|b| b.as_deref())
                        .unwrap_or("-");
                    out.push(Line::from(Span::styled(
                        format!(
                            "    {} · {} · ⎇ {} · {} events · {} checkpoint(s)",
                            s.cwd,
                            s.model,
                            branch,
                            s.events,
                            s.checkpoints.len()
                        ),
                        theme::dim(),
                    )));
                }
            }
            out.push(Line::from(Span::styled(
                "type to filter · ↑↓ · tab preview · enter switch · esc",
                theme::dim(),
            )));
            out
        }
        Overlay::Tree { rows, sel } => {
            let mut out = Vec::new();
            for (i, (s, depth)) in rows.iter().take(6).enumerate() {
                let cur = i == *sel;
                let mark = if cur { "›" } else { " " };
                let indent = "  ".repeat((*depth).min(4));
                let fork_mark = if *depth > 0 { "↳ " } else { "" };
                let here = if s.dir == self.session_dir {
                    " · (current)"
                } else {
                    ""
                };
                out.push(Line::from(Span::styled(
                    format!(
                        "{mark} {indent}{fork_mark}{} · {} · {}{}",
                        short_id(&s.id),
                        rel_time(s.last_ms),
                        s.first_user.as_deref().unwrap_or("(no prompt)"),
                        here
                    ),
                    if cur { theme::dialog_sel() } else { theme::dialog() },
                )));
            }
            out.push(Line::from(Span::styled(
                "↑↓ · enter switch · esc",
                theme::dim(),
            )));
            out
        }
        Overlay::Rewind { rows, sel } => {
            let mut out: Vec<Line<'static>> = rows
                .iter()
                .take(6)
                .enumerate()
                .map(|(i, r)| {
                    let cur = i == *sel;
                    Line::from(Span::styled(
                        format!(
                            "{} e{} · {} file(s) · {}",
                            if cur { "›" } else { " " },
                            r.boundary,
                            r.files,
                            r.label
                        ),
                        if cur { theme::dialog_sel() } else { theme::dialog() },
                    ))
                })
                .collect();
            out.push(Line::from(Span::styled(
                "↑↓ · enter restore files+conversation · esc",
                theme::dim(),
            )));
            out
        }
        Overlay::Transcript {
            query,
            scroll,
            expand_tools,
        } => {
            let all = transcript_lines(self, query, width, *expand_tools);
            let n = all.len();
            let mut out = vec![Line::from(vec![
                Span::styled("/ ", theme::dialog_key()),
                Span::styled(format!("{query}▌"), theme::dialog()),
            ])];
            // Scroll is a line offset; usize::MAX (fresh open) means tail.
            let max = n.saturating_sub(4);
            let start = (*scroll).min(max);
            out.extend(all.into_iter().skip(start).take(4));
            out.push(Line::from(Span::styled(
                format!("{n} lines · type to filter · ↑↓/pgdn · tab expand tools · esc"),
                theme::dim(),
            )));
            out
        }
        Overlay::Diff {
            rows,
            sel,
            preview,
            hunk_sel,
        } => {
            let mut out = Vec::new();
            for (i, r) in rows.iter().take(4).enumerate() {
                let cur = i == *sel;
                let mark = if cur { "›" } else { " " };
                let nrej = r.rejected.iter().filter(|x| **x).count();
                let rej = if nrej > 0 {
                    format!(" · {nrej} marked reject")
                } else {
                    String::new()
                };
                out.push(Line::from(Span::styled(
                    format!(
                        "{mark} {} +{} -{} {}{}",
                        r.status,
                        r.added,
                        r.deleted,
                        display_path(&self.cwd, &r.path),
                        rej
                    ),
                    if cur { theme::dialog_sel() } else { theme::dialog() },
                )));
                if *preview && cur {
                    for (hi, h) in r.hunks.iter().take(3).enumerate() {
                        let hcur = hi == *hunk_sel;
                        out.push(Line::from(Span::styled(
                            format!(
                                "  {} {} hunk {} (+{} -{})",
                                if hcur { "›" } else { " " },
                                if r.rejected[hi] { "[x]" } else { "[ ]" },
                                hi + 1,
                                h.added,
                                h.deleted
                            ),
                            if hcur { theme::dialog_sel() } else { theme::dialog() },
                        )));
                        if hcur {
                            for l in h.lines.iter().skip(1).take(4) {
                                let style = if l.starts_with('-') {
                                    theme::error()
                                } else if l.starts_with('+') {
                                    theme::meta()
                                } else {
                                    theme::dim()
                                };
                                out.push(Line::from(Span::styled(format!("    {l}"), style)));
                            }
                        }
                    }
                    if r.hunks.len() > 3 {
                        out.push(Line::from(Span::styled(
                            format!("    … {} more hunk(s)", r.hunks.len() - 3),
                            theme::dim(),
                        )));
                    }
                }
            }
            out.push(Line::from(Span::styled(
                if *preview {
                    "↑↓ file · ←→ hunk · space reject · enter revert · esc"
                } else {
                    "↑↓ file · tab diff · enter revert · esc"
                },
                theme::dim(),
            )));
            out
        }
    }
}
}

/// `/diff` row path: relative to the workspace when possible, else the
/// last two components — full paths rarely fit the picker.
fn display_path(cwd: &str, path: &std::path::Path) -> String {
    let s = path.display().to_string();
    if let Some(rest) = s.strip_prefix(&format!("{}/", cwd.trim_end_matches('/'))) {
        return rest.to_string();
    }
    let comps: Vec<_> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if comps.len() > 2 {
        format!("…/{}/{}", comps[comps.len() - 2], comps[comps.len() - 1])
    } else {
        s
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
