//! App state + event loop (P2.1–2.5, 2.7).
//!
//! Frame model: the engine runs on a worker thread and streams `Event`s
//! over a channel; completed events become immutable cells flushed to
//! native scrollback via `insert_before`. Only the live region — running
//! tools, indicator, queue strip, dialogs, composer, status line — is
//! managed, inside a dynamically-sized `Viewport::Inline`. Every frame is
//! wrapped in BSU/ESU when the terminal probed positive for sync 2026.

use std::io::{Read, Write};
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

// W3 split: same crate::app tree, so child modules still see App's
// private fields; cross-module items are `pub(crate)`.
mod input;
mod overlay;
mod panel;
mod render;
mod shell;

pub use shell::line_shell;

/// Engine → UI messages (single channel keeps ordering trivial).
pub enum EngineMsg {
    Event(Event),
    Ask(AskRequest, mpsc::Sender<AskDecision>),
    RunDone(overseer_core::agent::RunOutcome),
    RunError(String),
    /// The worker rebuilt its agent on a different session directory —
    /// the UI reseeds the transcript from that log.
    SessionSwitched {
        dir: std::path::PathBuf,
    },
}

/// UI → worker commands.
pub enum WorkerCmd {
    Submit {
        text: String,
        control: Control,
    },
    SetPreset(Preset),
    /// Drop the current agent and `Agent::resume` on `dir` — session
    /// switch, post-fork, and post-rewind rebuild all share this path.
    SwitchSession {
        dir: std::path::PathBuf,
    },
    Shutdown,
}

/// Which surface the app renders on. `Inline` is the scrollback-
/// preserving live strip (`Viewport::Inline` + `insert_before`);
/// `Full` owns the entire window on the alternate screen — transcript
/// above, a 2-row prompt, then the footer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Inline,
    Full,
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
        /// Sessions skipped because their log wouldn't parse (S1 rev):
        /// surfaced as one faint footer line, reasons go to /tmp.
        skipped: usize,
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
    /// Control panel — the bottom mark / ↑ on an empty composer.
    /// Tab strip on top, the selected tab's rows listed below.
    Panel {
        /// Index into PANEL_TABS.
        tab: usize,
        /// Scroll offset into the tab's row list.
        scroll: usize,
    },
}

/// One `task`-tool spawn seen in the event stream — drives the panel's
/// agents tab. Foreground spawns close on their `ToolResult`;
/// background ones only acknowledge the spawn there and close on the
/// later `SubagentDone` (matched via the `bg-N` dir in the ack text).
struct AgentEnt {
    /// `ToolCallStart.call_id` — links the spawn to its result.
    call_id: String,
    /// `background: true` in the call input.
    bg: bool,
    /// `bg-N` parsed from the spawn ack's trace path — the id
    /// `SubagentDone` reports under.
    bg_id: Option<String>,
    /// "read" | "write"
    mode: &'static str,
    /// "running" | "done" | "failed"
    state: &'static str,
    /// First prompt line, shortened — the row's at-a-glance label.
    prompt: String,
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
    /// Render surface — `run` sets Full, `run_inline` keeps Inline.
    pub mode: UiMode,
    /// Completed cells awaiting `insert_before` (Inline) or `tbuf`
    /// append (Full).
    pending: Vec<Cell>,
    /// Flattened transcript lines (Full only) — rendered from
    /// `history` cells at `tbuf_w`; rebuilt on resize.
    tbuf: Vec<Line<'static>>,
    /// Width `tbuf` was rendered at — a resize mismatch triggers a
    /// rebuild from `history`.
    tbuf_w: u16,
    /// Full mode: lines scrolled up from the transcript tail (0 =
    /// follow). Applies to `tbuf` only — the live stack stays pinned.
    scroll: usize,
    /// Transcript region height from the last full draw — PageUp/Down
    /// step size.
    view_h: u16,
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
    /// `task` spawns in spawn order — the agents tab renders them
    /// newest-first. Cleared on session switch (the reseed replays
    /// the new log's task calls back in).
    agents: Vec<AgentEnt>,
    tokens: u64,
    cost: f64,
    /// Sum of every RunEnd's per-run cache delta (live + replayed) —
    /// the dashboard `cache` row renders hit rate + read volume.
    cache: overseer_core::ledger::CacheStats,
    show_help: bool,
    show_plan: bool,
    tick: usize,
    dirty: bool,
    /// Engine channel hit the per-frame drain cap — zero-wait polling
    /// until cleared so input echo stays under 50 ms during bursts.
    backlogged: bool,
    /// Full mode: the control panel's bottom band (absolute rows) —
    /// the click hit-test reads it. None when the panel is closed.
    panel_band: Option<ratatui::layout::Rect>,
    /// Full mode: the rendered overlay block's first absolute row +
    /// shown line count + lines clipped off its top — click→row map.
    overlay_block: Option<(u16, usize, usize)>,
    /// Full mode: the prompt band's first grid row — the web frame
    /// carries it (`"p"`) so the client can seat the prompt dip and
    /// divider regardless of what sits below.
    pub prompt_top: u16,
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
            mode: UiMode::Inline,
            pending: Vec::new(),
            tbuf: Vec::new(),
            tbuf_w: 0,
            scroll: 0,
            view_h: 0,
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
            agents: Vec::new(),
            tokens: 0,
            cost: 0.0,
            cache: overseer_core::ledger::CacheStats::default(),
            show_help: false,
            show_plan: false,
            tick: 0,
            dirty: true,
            backlogged: false,
            panel_band: None,
            overlay_block: None,
            prompt_top: 0,
            quit: false,
        }
    }

    fn running(&self) -> bool {
        matches!(self.run, RunState::Running { .. })
    }

    pub fn full(&self) -> bool {
        self.mode == UiMode::Full
    }

    /// `tbuf` lines at `width`, rebuilding from `history` when the
    /// width changed (resize) or on first fill.
    fn tbuf_at(&mut self, width: u16) {
        if self.tbuf_w != width {
            self.tbuf = self.history.iter().flat_map(|c| c.lines(width)).collect();
            self.tbuf_w = width;
        }
    }

    /// Plain-text transcript for the exit handoff — Full mode leaves
    /// the alternate screen, so the session's record is printed into
    /// native scrollback on the way out (styled lines can't leave the
    /// buffer; links/marks are an Inline-stream feature).
    pub fn transcript_plain(&self) -> String {
        let mut out = String::new();
        for c in &self.history {
            let p = c.plain();
            if !p.is_empty() {
                out.push_str(&p);
                out.push('\n');
            }
        }
        out
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
            self.shutdown();
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
                        link: None,
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
        if let EventKind::RunEnd {
            total_cost_usd,
            cache,
            ..
        } = ev.kind
        {
            self.cost = total_cost_usd;
            self.cache.fresh_input += cache.fresh_input;
            self.cache.cache_read += cache.cache_read;
            self.cache.cache_write += cache.cache_write;
            self.cache.output += cache.output;
        }
        if let EventKind::ToolCallStart {
            call_id,
            name,
            input,
        } = &ev.kind
        {
            if let RunState::Running { phase, .. } = &mut self.run {
                *phase = format!("running {name}");
            }
            if name == "task" {
                let v = serde_json::Value::as_str;
                let b = serde_json::Value::as_bool;
                self.agents.push(AgentEnt {
                    call_id: call_id.clone(),
                    bg: input.get("background").and_then(b).unwrap_or(false),
                    bg_id: None,
                    mode: match input.get("mode").and_then(v) {
                        Some("write") => "write",
                        _ => "read",
                    },
                    state: "running",
                    prompt: input
                        .get("prompt")
                        .and_then(v)
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(48)
                        .collect(),
                });
            }
        }
        if let EventKind::ToolResult {
            call_id,
            name,
            is_error,
            content,
            ..
        } = &ev.kind
        {
            if name == "task" {
                if let Some(a) = self.agents.iter_mut().rev().find(|a| a.call_id == *call_id) {
                    if *is_error {
                        a.state = "failed";
                    } else if a.bg {
                        // The ack isn't the finish — record the dir
                        // name so `SubagentDone` can close the row.
                        a.bg_id = bg_id_of(content);
                    } else {
                        a.state = "done";
                    }
                }
            }
        }
        if let EventKind::SubagentDone { task_id, .. } = &ev.kind {
            if let Some(a) = self
                .agents
                .iter_mut()
                .rev()
                .find(|a| a.bg_id.as_deref() == Some(task_id.as_str()))
            {
                a.state = "done";
            }
        }
        // The run-summary cell wants the elapsed wall time — RunEnd
        // lands while `run` is still Running.
        let run_elapsed = match &self.run {
            RunState::Running { started, .. } => Some(started.elapsed()),
            _ => None,
        };
        match cells::feed(ev, run_elapsed) {
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
                } else {
                    // Reseed paths drain `live` into `pending` (or
                    // history) before the ToolResult replays —
                    // resolve the cell wherever it landed or it
                    // renders as `◌` forever.
                    for c in self.pending.iter_mut().chain(self.history.iter_mut()).rev() {
                        if let Cell::Tool {
                            id,
                            status: s,
                            output: o,
                            ..
                        } = c
                        {
                            if *id == call_id {
                                *s = status;
                                *o = output;
                                // `tbuf` may already hold a stale
                                // render of this row — rebuild it.
                                self.tbuf_w = 0;
                                break;
                            }
                        }
                    }
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
        // A provider failure already rendered as one `error:` transcript
        // cell via the engine's own Error event — nothing extra here.
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
        self.agents.clear();
        self.tbuf.clear();
        self.scroll = 0;
        self.tokens = 0;
        self.file_index = std::cell::OnceCell::new();
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string());
        self.pending.push(Cell::Meta {
            style: crate::theme::meta(),
            text: format!("── session {name} ──"),
            link: None,
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
            self.wake_tick();
            return Ok(());
        }
        let ev = event::read()?;
        self.on_ct_event(ev);
        Ok(())
    }

    /// Tell the worker thread to stop — called on quit by `pump` and
    /// by the web loop (which has no `pump`).
    pub fn shutdown(&self) {
        let _ = self.worker_tx.send(WorkerCmd::Shutdown);
    }

    /// Timeout side of `poll_input`: spinner/elapsed/dialog re-arm
    /// cadence. The web surface calls the same hook on input-idle.
    pub fn wake_tick(&mut self) {
        if self.running() || self.dialog.is_some() {
            self.dirty = true;
        }
    }

    /// Input-source-agnostic event dispatch — the terminal reader and
    /// the web server's `/input` handler both land here.
    pub fn on_ct_event(&mut self, ev: CtEvent) {
        match ev {
            CtEvent::Key(key) => self.on_key(key),
            CtEvent::Paste(text) => {
                self.composer.paste(&text);
                self.dirty = true;
            }
            CtEvent::Mouse(m) if self.full() => {
                use crossterm::event::{MouseButton, MouseEventKind};
                let in_band = self
                    .panel_band
                    .map(|r| m.row >= r.y && m.row < r.y + r.height)
                    .unwrap_or(false);
                match m.kind {
                    // Wheel over the panel band scrolls the panel's
                    // rows; anywhere else scrolls the transcript.
                    MouseEventKind::ScrollUp if in_band => {
                        if let Some(Overlay::Panel { scroll, .. }) = &mut self.overlay {
                            *scroll = scroll.saturating_sub(1);
                        }
                    }
                    MouseEventKind::ScrollDown if in_band => {
                        if let Some(Overlay::Panel { scroll, .. }) = &mut self.overlay {
                            *scroll = scroll.saturating_add(1);
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        self.scroll = self.scroll.saturating_add(3);
                    }
                    MouseEventKind::ScrollDown => {
                        self.scroll = self.scroll.saturating_sub(3);
                    }
                    MouseEventKind::Down(MouseButton::Left) => self.on_click(m.column, m.row),
                    _ => {}
                }
                self.dirty = true;
            }
            CtEvent::FocusGained => self.focused = true,
            CtEvent::FocusLost => self.focused = false,
            CtEvent::Resize(_, _) => self.dirty = true,
            _ => {}
        }
    }

    fn set_toast(&mut self, text: String) {
        self.toast = Some((text, std::time::Instant::now()));
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
        let text = self.history.iter().rev().find_map(|c| match c {
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

    /// `drive` polls this: the draft to round-trip through $EDITOR.
    pub fn take_editor_request(&mut self) -> Option<String> {
        self.want_editor.take()
    }

    /// `drive` installs the edited draft back into the composer.
    pub fn set_composer_text(&mut self, text: String) {
        self.composer.set_text(&text);
        self.dirty = true;
    }

    fn on_submit(&mut self, text: String) {
        self.dirty = true; // submits outside on_key (tests, /approve) still repaint
        if let Some(cmd) = text.strip_prefix('/') {
            self.slash(cmd.trim());
            return;
        }
        if let Some(cmd) = text.strip_prefix('!') {
            self.run_shell(cmd.trim());
            return;
        }
        self.scroll = 0; // submitting snaps the transcript to the tail
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
                link: None,
            }),
        }
    }

    // ── rendering ────────────────────────────────────────────────────
}
/// Render an open overlay picker into live-region lines.
impl App {}

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

/// `bg-N` out of a background spawn ack — the trace dir is the id
/// `SubagentDone` reports under ("…subagents/bg-3)").
fn bg_id_of(content: &str) -> Option<String> {
    let i = content.rfind("/bg-")? + 1;
    let id: String = content[i..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    (!id.is_empty()).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::overlay::short_id;
    use super::shell::{shell_capture, shell_capture_timeout};
    use super::*;
    use std::path::PathBuf;

    fn test_app(cwd: &str) -> App {
        let (_etx, erx) = mpsc::channel();
        let (wtx, _wrx) = mpsc::channel();
        App::new(
            erx,
            wtx,
            Preset::WorkspaceWrite,
            cwd.into(),
            "m".into(),
            PathBuf::from("/tmp/ovw-sess"),
        )
    }

    #[test]
    fn short_id_is_char_safe() {
        // Byte-slicing panics mid-char; char-walk must not.
        let id = format!("héad{}", "界".repeat(10));
        let s = short_id(&id);
        assert!(s.starts_with('…'));
        assert_eq!(s.chars().count(), 11);
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn shell_timeout_kills_and_reaps() {
        let t = Instant::now();
        let (code, out) =
            shell_capture_timeout("sleep 30", ".", Duration::from_millis(50)).unwrap();
        assert_eq!(code, -1);
        assert!(out.contains("timed out"));
        // Returns promptly — the child did not run to completion.
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    /// Linux/dash keeps `sleep` as a separate child under `sh -c` (and
    /// pipelines spawn one process per side anywhere); killing only `sh`
    /// orphans grandchildren that hold the pipe write ends and the
    /// reader joins block for their full lifetimes. The timeout must
    /// kill the whole process group.
    #[test]
    fn shell_timeout_kills_the_whole_process_group() {
        let pgid_file = std::env::temp_dir().join(format!("ovw-pgid-{}", std::process::id()));
        // `$$` is the `sh` pid, which is also the process-group id.
        let cmd = format!("echo $$ > {}; sleep 30 | cat", pgid_file.display());
        let t = Instant::now();
        let (code, out) =
            shell_capture_timeout(&cmd, ".", Duration::from_secs(1)).unwrap();
        assert_eq!(code, -1);
        assert!(out.contains("timed out"));
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "grandchildren pinned the reader joins: {:?}",
            t.elapsed()
        );
        let pgid: i32 = std::fs::read_to_string(&pgid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let _ = std::fs::remove_file(&pgid_file);
        // Brief window for init to reap the SIGKILLed group members.
        let mut esrch = false;
        for _ in 0..40 {
            let rc = unsafe { libc::kill(-pgid, 0) };
            if rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                esrch = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(esrch, "process group {pgid} still has live members");
    }

    #[test]
    fn shell_capture_collects_stdout_and_stderr() {
        let (code, out) = shell_capture("echo hi; echo err >&2", "/").unwrap();
        assert_eq!(code, 0);
        assert!(out.contains("hi"));
        assert!(out.contains("err"));
    }

    #[test]
    fn line_shell_uses_agent_cwd() {
        let dir = std::env::temp_dir().join(format!("ovw-lineshell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker.txt"), "marker-ok").unwrap();
        let (code, out) = line_shell("cat marker.txt", &dir.display().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(code, 0);
        assert!(out.contains("marker-ok"));
    }

    #[test]
    fn seed_resolves_tool_status_from_replayed_results() {
        use overseer_core::event::EventKind;
        let ev = |kind| Event {
            id: 0,
            parent_id: None,
            ts_ms: 0,
            prev_hash: 0,
            hash: 0,
            kind,
        };
        let mut app = test_app("/tmp");
        // A replayed ToolCallStart drains live→pending before its
        // ToolResult replays — the done must find the cell wherever
        // it landed or a resumed session renders every tool `◌`.
        app.seed(&ev(EventKind::ToolCallStart {
            call_id: "c1".into(),
            name: "write".into(),
            input: serde_json::json!({"path": "a"}),
        }));
        app.seed(&ev(EventKind::ToolResult {
            call_id: "c1".into(),
            name: "write".into(),
            content: "ok".into(),
            is_error: false,
            raw_bytes: 2,
            spilled_to: None,
            denied: false,
        }));
        app.seed(&ev(EventKind::ToolCallStart {
            call_id: "c2".into(),
            name: "write".into(),
            input: serde_json::json!({"path": "b"}),
        }));
        app.seed(&ev(EventKind::ToolResult {
            call_id: "c2".into(),
            name: "write".into(),
            content: "boom".into(),
            is_error: true,
            raw_bytes: 4,
            spilled_to: None,
            denied: false,
        }));
        let statuses: Vec<ToolStatus> = app
            .pending
            .iter()
            .chain(app.history.iter())
            .filter_map(|c| match c {
                Cell::Tool { status, .. } => Some(*status),
                _ => None,
            })
            .collect();
        assert_eq!(statuses, vec![ToolStatus::Ok, ToolStatus::Err]);
        assert!(app.live.is_empty());
    }

    #[test]
    fn runend_cache_sums_across_live_and_replay() {
        use overseer_core::event::EventKind;
        use overseer_core::ledger::CacheStats;
        let ev = |kind| Event {
            id: 0,
            parent_id: None,
            ts_ms: 0,
            prev_hash: 0,
            hash: 0,
            kind,
        };
        let run_end = |read: u64| EventKind::RunEnd {
            stop_reason: "end_turn".into(),
            steps: 1,
            total_cost_usd: 0.01,
            cache: CacheStats {
                fresh_input: 100,
                cache_read: read,
                cache_write: 10,
                output: 5,
            },
        };
        let mut app = test_app("/tmp");
        app.on_event(&ev(run_end(300))); // live
        app.seed(&ev(run_end(200))); // replayed
        assert_eq!(app.cache.cache_read, 500);
        assert_eq!(app.cache.fresh_input, 200);
        assert_eq!(app.cache.input_total(), 720);
        assert!((app.cache.hit_rate() - 500.0 / 720.0).abs() < 1e-9);
        // A pre-extension RunEnd (zero cache) leaves the row at `—`.
        let mut app2 = test_app("/tmp");
        app2.on_event(&ev(EventKind::RunEnd {
            stop_reason: "end_turn".into(),
            steps: 1,
            total_cost_usd: 0.0,
            cache: Default::default(),
        }));
        assert_eq!(app2.cache.input_total(), 0);
    }

    #[test]
    fn empty_state_guard_respects_overlay() {
        // The web `e` flag shares this guard — the client's mark
        // must not paint over an open panel.
        let mut app = test_app("/tmp");
        assert!(app.empty_state_shown());
        app.overlay = Some(Overlay::Panel { tab: 0, scroll: 0 });
        assert!(!app.empty_state_shown());
        app.overlay = None;
        assert!(app.empty_state_shown());
    }

    #[test]
    fn build_index_skips_heavy_dirs() {
        let dir = std::env::temp_dir().join(format!("ovw-idx-{}", std::process::id()));
        for d in ["src", "target/debug", "node_modules/m", ".git"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.join("target/debug/a.o"), "").unwrap();
        std::fs::write(dir.join("node_modules/m/x.js"), "").unwrap();
        std::fs::write(dir.join(".git/HEAD"), "").unwrap();
        let app = test_app(&dir.display().to_string());
        let idx = app.build_index();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(idx, vec!["src/lib.rs".to_string()]);
    }
}
