//! Key + mouse input dispatch and `@`/`/` completion — split out of
//! `app.rs` (W3). `use super::*` sees the parent module's imports and
//! `App`'s private fields (child modules inherit parent visibility).

use super::overlay::*;
use super::panel::PANEL_TABS;
use super::*;

pub(crate) fn subseq_match(hay: &str, needle: &str) -> bool {
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
pub(crate) fn at_fragment(text: &str) -> Option<&str> {
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

pub(crate) fn common_prefix(a: &str, b: &str) -> String {
    a.chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect()
}

impl App {
    pub(crate) fn on_key(&mut self, key: KeyEvent) {
        // Modal pickers own the keyboard entirely (the sessions filter
        // is a text input by design). Esc always closes first.
        if self.overlay.is_some() {
            // The control panel is passive chrome — it claims only its
            // own nav keys, and those only while the composer is empty,
            // so typing a draft keeps working with the dashboard open:
            // chars/Backspace edit, Tab completes, Enter submits,
            // arrows move the cursor. Esc/F1 always close it.
            let claimed = if matches!(self.overlay, Some(Overlay::Panel { .. })) {
                match key.code {
                    KeyCode::Esc | KeyCode::F(1) => true,
                    KeyCode::Tab | KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
                        self.composer.is_empty()
                    }
                    _ => false,
                }
            } else {
                true
            };
            if claimed {
                self.on_overlay_key(key);
                self.dirty = true;
                return;
            }
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
                (KeyCode::Char(c), m) if m.is_empty() && matches!(c, '1' | '2' | '3' | '4') => {
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
            (KeyCode::Char('j'), m) if m.contains(KeyModifiers::CONTROL) => self.composer.newline(),
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
            // Full mode: the transcript scrolls inside the window —
            // PageUp/Down (and the wheel, in poll_input) walk `tbuf`
            // while the live stack stays pinned to the region bottom.
            (KeyCode::PageUp, _) if self.full() => {
                self.scroll = self.scroll.saturating_add(self.view_h.max(1) as usize);
            }
            (KeyCode::PageDown, _) if self.full() => {
                self.scroll = self.scroll.saturating_sub(self.view_h.max(1) as usize);
            }
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
            // ↑ on an empty composer opens the control panel; history
            // recall still works once anything is typed.
            (KeyCode::Up, _) if self.composer.is_empty() => {
                self.overlay = Some(Overlay::Panel { tab: 0, scroll: 0 });
            }
            (KeyCode::Up, _) => self.composer.up(),
            (KeyCode::Down, _) => self.composer.down(),
            (KeyCode::Home, _) => self.composer.home(),
            (KeyCode::End, _) => self.composer.end(),
            (KeyCode::Backspace, _) => self.composer.backspace(),
            (KeyCode::Delete, _) => self.composer.delete(),
            (KeyCode::Char('?'), _) if self.composer.is_empty() => {
                self.show_help = !self.show_help;
            }
            // F1 = the bottom mark — toggles the control panel.
            (KeyCode::F(1), _) => {
                self.overlay = Some(Overlay::Panel { tab: 0, scroll: 0 });
            }
            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                self.composer.insert_str(&c.to_string());
            }
            _ => return,
        }
        self.dirty = true;
    }

    pub(crate) fn on_overlay_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                self.overlay = None;
            }
            // F1 toggles the panel back off (it's also the mark click).
            (KeyCode::F(1), _) => {
                if matches!(self.overlay, Some(Overlay::Panel { .. })) {
                    self.overlay = None;
                }
            }
            (KeyCode::Up, _) => {
                // Read scroll BEFORE the decrement: ↑ at the panel's
                // top closes it (symmetric with the ↑-opens binding),
                // but scrolling down to 0 must not close under you.
                let was_top = matches!(self.overlay, Some(Overlay::Panel { scroll: 0, .. }));
                match &mut self.overlay {
                    Some(Overlay::Transcript { scroll, .. })
                    | Some(Overlay::Panel { scroll, .. }) => {
                        *scroll = scroll.saturating_sub(1);
                    }
                    Some(o) => {
                        let (sel, len) = overlay_sel(o);
                        *sel = sel.saturating_sub(1).min(len);
                    }
                    None => {}
                }
                if was_top && matches!(self.overlay, Some(Overlay::Panel { .. })) {
                    self.overlay = None;
                }
            }
            (KeyCode::Down, _) => match &mut self.overlay {
                Some(Overlay::Transcript { scroll, .. }) | Some(Overlay::Panel { scroll, .. }) => {
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
                Some(Overlay::Panel { tab, scroll }) => {
                    *tab = (*tab + 1) % PANEL_TABS.len();
                    *scroll = 0;
                }
                _ => {}
            },
            (KeyCode::Left, _) | (KeyCode::Right, _) => {
                if let Some(Overlay::Panel { tab, scroll }) = &mut self.overlay {
                    let d = if key.code == KeyCode::Left {
                        PANEL_TABS.len() - 1
                    } else {
                        1
                    };
                    *tab = (*tab + d) % PANEL_TABS.len();
                    *scroll = 0;
                } else if let Some(Overlay::Diff {
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
            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                match &mut self.overlay {
                    Some(Overlay::Sessions { filter, sel, .. }) => {
                        filter.push(c);
                        *sel = 0;
                    }
                    Some(Overlay::Transcript { query, scroll, .. }) => {
                        query.push(c);
                        *scroll = 0;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Mouse hit-test against the rects `draw_full` recorded. Panel
    /// strip: click a tab to switch. List overlays: click selects the
    /// row, clicking the selected row confirms (double-click shape).
    pub(crate) fn on_click(&mut self, col: u16, row: u16) {
        if let Some(r) = self.panel_band {
            if row >= r.y && row < r.y + r.height {
                if row == r.y {
                    // Strip layout: " name " per tab from column 0.
                    let mut x = 0u16;
                    for (i, name) in PANEL_TABS.iter().enumerate() {
                        let w = name.len() as u16 + 2;
                        if col >= x && col < x + w {
                            if let Some(Overlay::Panel { tab, scroll }) = &mut self.overlay {
                                *tab = i;
                                *scroll = 0;
                            }
                            break;
                        }
                        x += w;
                    }
                }
                return;
            }
        }
        if let Some((top, len, clip)) = self.overlay_block {
            if row >= top && row < top + len as u16 {
                let li = (row - top) as usize + clip;
                // Sessions/Transcript lead with a filter line — clicks
                // there don't select a row.
                let header = match &self.overlay {
                    Some(Overlay::Sessions { .. }) | Some(Overlay::Transcript { .. }) => 1,
                    _ => 0,
                };
                if li < header {
                    return;
                }
                let idx = li - header;
                let mut confirm = false;
                match &mut self.overlay {
                    Some(Overlay::Sessions {
                        rows, sel, filter, ..
                    }) => {
                        let cap = filtered_sessions(rows, filter).len().min(6);
                        if idx < cap {
                            if *sel == idx {
                                confirm = true;
                            } else {
                                *sel = idx;
                            }
                        }
                    }
                    Some(Overlay::Tree { rows, sel }) => {
                        let cap = rows.len().min(6);
                        if idx < cap {
                            if *sel == idx {
                                confirm = true;
                            } else {
                                *sel = idx;
                            }
                        }
                    }
                    Some(Overlay::Rewind { rows, sel }) => {
                        let cap = rows.len().min(6);
                        if idx < cap {
                            if *sel == idx {
                                confirm = true;
                            } else {
                                *sel = idx;
                            }
                        }
                    }
                    Some(Overlay::Diff { rows, sel, .. }) => {
                        let cap = rows.len().min(4);
                        if idx < cap {
                            if *sel == idx {
                                confirm = true;
                            } else {
                                *sel = idx;
                            }
                        }
                    }
                    _ => {}
                }
                if confirm {
                    self.overlay_confirm();
                }
            }
        }
    }

    /// Tab: complete `/command` or `@path` from the current token.
    /// One match completes fully (with trailing space); several complete
    /// to their longest common prefix — the suggestion strip shows the
    /// menu meanwhile.
    pub(crate) fn complete(&mut self) {
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
            1 => self.composer.set_text(&format!("{stem}{} ", candidates[0])),
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
    pub(crate) fn file_index(&self) -> &[String] {
        self.file_index
            .get_or_init(|| self.build_index())
            .as_slice()
    }

    pub(crate) fn build_index(&self) -> Vec<String> {
        const SKIP: &[&str] = &[".git", "target", "node_modules", ".overseer"];
        // A synchronous read_dir on the UI thread must stay bounded:
        // 20k entries or ~150 ms, whichever trips first — a huge
        // checkout degrades to no completions, never a frozen frame.
        const MAX_ENTRIES: usize = 20_000;
        let deadline = Instant::now() + Duration::from_millis(150);
        let mut out = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(&self.cwd)];
        let mut visited = 0usize;
        'walk: while let Some(d) = stack.pop() {
            if out.len() >= MAX_ENTRIES {
                break;
            }
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                visited += 1;
                if visited.is_multiple_of(512) && Instant::now() >= deadline {
                    break 'walk;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                let p = e.path();
                if p.is_dir() {
                    if !SKIP.contains(&name.as_str()) && !name.starts_with('.') {
                        stack.push(p);
                    }
                } else if let Ok(rel) = p.strip_prefix(&self.cwd) {
                    if out.len() >= MAX_ENTRIES {
                        break 'walk;
                    }
                    out.push(rel.display().to_string());
                }
            }
        }
        out.sort();
        out
    }
}
