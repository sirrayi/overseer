//! Anchored composer (P2.3) — the input box is sacred ground.
//!
//! Grapheme-cluster buffer with atomic chip elements: a large paste
//! collapses to a `[Pasted #N]` atom — instant ingestion, never O(n²)
//! char-by-char processing (the Gemini #18366 failure), and multi-
//! paragraph paste can never auto-submit. History + stash + snapshot
//! undo; Enter submits, Ctrl+J/Alt+Enter inserts a newline.

use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::theme;

/// Pastes longer than this — or containing any newline — become chips.
const CHIP_THRESHOLD: usize = 120;
const UNDO_CAP: usize = 64;

#[derive(Debug, Clone, PartialEq)]
enum Elem {
    /// One grapheme cluster.
    Text(String),
    /// Atomic paste reference into `pastes`.
    Chip(usize),
}

pub struct Composer {
    lines: Vec<Vec<Elem>>,
    row: usize,
    col: usize,
    history: Vec<String>,
    hist_idx: Option<usize>,
    /// Draft preserved when entering history.
    saved: Option<String>,
    /// Stashed buffer (Ctrl+S).
    stash: Option<String>,
    pastes: std::collections::HashMap<usize, String>,
    paste_seq: usize,
    undo: Vec<(Vec<Vec<Elem>>, usize, usize)>,
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

impl Composer {
    pub fn new() -> Self {
        Composer {
            lines: vec![Vec::new()],
            row: 0,
            col: 0,
            history: Vec::new(),
            hist_idx: None,
            saved: None,
            stash: None,
            pastes: std::collections::HashMap::new(),
            paste_seq: 0,
            undo: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.is_empty())
    }

    fn snapshot(&mut self) {
        self.undo
            .push((self.lines.clone(), self.row, self.col));
        if self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
    }

    pub fn undo(&mut self) {
        if let Some((lines, row, col)) = self.undo.pop() {
            let last = lines.len().saturating_sub(1);
            self.lines = lines;
            self.row = row.min(last);
            self.col = col.min(self.lines.get(self.row).map(|l| l.len()).unwrap_or(0));
        }
    }

    /// Typeable text (regular keys + short single-line pastes).
    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.snapshot();
        self.insert_str_raw(s);
        self.hist_idx = None;
    }

    fn insert_str_raw(&mut self, s: &str) {
        for g in s.graphemes(true) {
            if g == "\n" || g == "\r\n" {
                self.split_line();
            } else {
                self.lines[self.row].insert(self.col, Elem::Text(g.to_string()));
                self.col += 1;
            }
        }
    }

    /// Bracketed paste: large or multi-line → one atomic chip.
    pub fn paste(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.snapshot();
        if s.len() > CHIP_THRESHOLD || s.contains('\n') {
            self.paste_seq += 1;
            self.pastes.insert(self.paste_seq, s.to_string());
            self.lines[self.row].insert(self.col, Elem::Chip(self.paste_seq));
            self.col += 1;
        } else {
            self.insert_str_raw(s);
        }
        self.hist_idx = None;
    }

    fn split_line(&mut self) {
        let tail: Vec<Elem> = self.lines[self.row].split_off(self.col);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }

    pub fn newline(&mut self) {
        self.snapshot();
        self.split_line();
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            self.snapshot();
            self.col -= 1;
            // Chips and graphemes both delete atomically.
            let elem = self.lines[self.row].remove(self.col);
            if let Elem::Chip(id) = elem {
                self.pastes.remove(&id);
            }
        } else if self.row > 0 {
            self.snapshot();
            let tail = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].extend(tail);
        }
    }

    pub fn delete(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.snapshot();
            let elem = self.lines[self.row].remove(self.col);
            if let Elem::Chip(id) = elem {
                self.pastes.remove(&id);
            }
        } else if self.row + 1 < self.lines.len() {
            self.snapshot();
            let tail = self.lines.remove(self.row + 1);
            self.lines[self.row].extend(tail);
        }
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
    }

    pub fn right(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].len());
        } else {
            self.history_prev();
        }
    }

    pub fn down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].len());
        } else {
            self.history_next();
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }
    pub fn end(&mut self) {
        self.col = self.lines[self.row].len();
    }

    fn word_is_space(e: &Elem) -> bool {
        matches!(e, Elem::Text(g) if g.trim().is_empty())
    }

    /// Ctrl+W: delete the word behind the cursor (whitespace first, then
    /// the word). One undo entry for the whole operation.
    pub fn delete_word_back(&mut self) {
        let mut did = false;
        self.snapshot();
        loop {
            if self.col > 0 {
                let e = self.lines[self.row].remove(self.col - 1);
                if let Elem::Chip(id) = &e {
                    self.pastes.remove(id);
                }
                self.col -= 1;
                let was_space = Self::word_is_space(&e);
                did = true;
                if !was_space
                    && (self.col == 0
                        || Self::word_is_space(&self.lines[self.row][self.col - 1]))
                {
                    break; // consumed the word back to whitespace/start
                }
            } else if self.row > 0 {
                let tail = self.lines.remove(self.row);
                self.row -= 1;
                self.col = self.lines[self.row].len();
                self.lines[self.row].extend(tail);
                did = true;
            } else {
                break;
            }
        }
        if !did {
            self.undo.pop(); // nothing happened — don't leave an entry
        }
    }

    pub fn word_left(&mut self) {
        if self.col == 0 {
            if self.row == 0 {
                return;
            }
            self.row -= 1;
            self.col = self.lines[self.row].len();
            return;
        }
        let line = &self.lines[self.row];
        let mut c = self.col;
        while c > 0 && Self::word_is_space(&line[c - 1]) {
            c -= 1;
        }
        while c > 0 && !Self::word_is_space(&line[c - 1]) {
            c -= 1;
        }
        self.col = c;
    }

    pub fn word_right(&mut self) {
        let line_len = self.lines[self.row].len();
        if self.col >= line_len {
            if self.row + 1 < self.lines.len() {
                self.row += 1;
                self.col = 0;
            }
            return;
        }
        let line = &self.lines[self.row];
        let mut c = self.col;
        while c < line.len() && !Self::word_is_space(&line[c]) {
            c += 1;
        }
        while c < line.len() && Self::word_is_space(&line[c]) {
            c += 1;
        }
        self.col = c;
    }

    pub fn kill_to_eol(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.snapshot();
            let removed: Vec<Elem> = self.lines[self.row].split_off(self.col);
            for e in removed {
                if let Elem::Chip(id) = e {
                    self.pastes.remove(&id);
                }
            }
        }
    }

    pub fn clear(&mut self) {
        if !self.is_empty() {
            self.snapshot();
            self.lines = vec![Vec::new()];
            self.row = 0;
            self.col = 0;
            self.pastes.clear();
        }
    }

    /// Ctrl+S: park the whole buffer for later recall.
    pub fn stash(&mut self) {
        let t = self.text();
        if t.is_empty() {
            if let Some(s) = self.stash.take() {
                self.snapshot();
                self.insert_str_raw(&s);
            }
        } else {
            self.stash = Some(t);
            self.clear();
        }
    }

    /// Expanded content — chips inlined. What gets submitted.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            for e in line {
                match e {
                    Elem::Text(g) => out.push_str(g),
                    Elem::Chip(id) => {
                        if let Some(p) = self.pastes.get(id) {
                            out.push_str(p);
                        }
                    }
                }
            }
        }
        out
    }

    /// Visible buffer — chips as their chip label (for display/echo).
    fn display_text(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            for e in line {
                match e {
                    Elem::Text(g) => out.push_str(g),
                    Elem::Chip(id) => out.push_str(&format!("[Pasted #{id}]")),
                }
            }
        }
        out
    }

    /// Submit: returns the expanded text and resets the buffer.
    pub fn submit(&mut self) -> String {
        let t = self.text();
        let trimmed = t.trim().to_string();
        if trimmed.is_empty() {
            return String::new();
        }
        if self.history.last().map(|h| h != &trimmed).unwrap_or(true) {
            self.history.push(trimmed.clone());
        }
        self.hist_idx = None;
        self.saved = None;
        self.snapshot();
        self.lines = vec![Vec::new()];
        self.row = 0;
        self.col = 0;
        self.pastes.clear();
        trimmed
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.hist_idx {
            None => {
                self.saved = Some(self.display_text());
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.hist_idx = Some(idx);
        let entry = self.history[idx].clone();
        self.snapshot();
        self.lines = vec![Vec::new()];
        self.row = 0;
        self.col = 0;
        self.insert_str_raw(&entry);
    }

    fn history_next(&mut self) {
        let Some(i) = self.hist_idx else { return };
        self.snapshot();
        self.lines = vec![Vec::new()];
        self.row = 0;
        self.col = 0;
        if i + 1 < self.history.len() {
            self.hist_idx = Some(i + 1);
            let entry = self.history[i + 1].clone();
            self.insert_str_raw(&entry);
        } else {
            self.hist_idx = None;
            let saved = self.saved.take().unwrap_or_default();
            self.insert_str_raw(&saved);
        }
    }

    /// Render into lines at `width`; also returns the cursor's visual
    /// (x, y) so the frame can place the hardware cursor on it.
    pub fn render(&self, width: u16) -> (Vec<Line<'static>>, (u16, u16)) {
        let w = (width as usize).saturating_sub(2).max(4);
        let mut out: Vec<Line<'static>> = Vec::new();
        let (mut cx, mut cy) = (0u16, 0u16);
        let mut cursor_set = false;

        for (i, line) in self.lines.iter().enumerate() {
            let prefix = if i == 0 { "❯ " } else { "  " };
            // Build visual rows for this logical line, tracking cursor.
            let mut spans: Vec<Span<'static>> = vec![Span::styled(prefix, theme::PROMPT)];
            let mut col_px = 2usize; // prefix width
            let mut elem_x_positions: Vec<usize> = Vec::with_capacity(line.len());
            for e in line {
                elem_x_positions.push(col_px);
                match e {
                    Elem::Text(g) => {
                        col_px += UnicodeWidthStr::width(g.as_str());
                        spans.push(Span::raw(g.clone()));
                    }
                    Elem::Chip(id) => {
                        let label = format!("[Pasted #{id}]");
                        col_px += label.len();
                        spans.push(Span::styled(label, theme::META));
                    }
                }
            }
            // Wrap the visual line at w-2; find cursor's visual slot.
            let cur_elem_col = if i == self.row { self.col } else { usize::MAX };
            let cursor_px = if cur_elem_col <= line.len() {
                if cur_elem_col == line.len() {
                    Some(col_px)
                } else {
                    elem_x_positions.get(cur_elem_col).copied()
                }
            } else {
                None
            };
            let wrapped = crate::cells::wrap_styled(spans, w + 2);
            for (j, wl) in wrapped.into_iter().enumerate() {
                if !cursor_set {
                    if let Some(px) = cursor_px {
                        let lo = j * w;
                        let hi = lo + w + 2;
                        if px <= hi {
                            cx = px.saturating_sub(lo) as u16;
                            cy = out.len() as u16;
                            cursor_set = true;
                        }
                    }
                }
                out.push(wl);
            }
        }
        if !cursor_set {
            cy = out.len().saturating_sub(1) as u16;
            cx = 2;
        }
        (out, (cx, cy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_chip_is_atomic_and_expands() {
        let mut c = Composer::new();
        let big = "x".repeat(500);
        c.paste(&big);
        assert!(c.text().contains(&"x".repeat(500)));
        // One elem — a single backspace removes the whole chip.
        c.backspace();
        assert!(c.text().is_empty());
    }

    #[test]
    fn multiline_editing() {
        let mut c = Composer::new();
        c.insert_str("hello");
        c.newline();
        c.insert_str("world");
        assert_eq!(c.text(), "hello\nworld");
        c.up();
        c.end();
        c.backspace();
        assert_eq!(c.text(), "hell\nworld");
    }

    #[test]
    fn history_round_trip() {
        let mut c = Composer::new();
        c.insert_str("first");
        assert_eq!(c.submit(), "first");
        c.insert_str("second");
        c.submit();
        c.insert_str("draft");
        c.up();
        assert_eq!(c.text(), "second");
        c.up();
        assert_eq!(c.text(), "first");
        c.down();
        c.down();
        assert_eq!(c.text(), "draft");
    }

    #[test]
    fn word_navigation() {
        let mut c = Composer::new();
        c.insert_str("foo bar  baz");
        c.word_left();
        assert_eq!(c.col, 9);
        c.word_left();
        assert_eq!(c.col, 4);
        c.word_right();
        assert_eq!(c.col, 9);
    }

    #[test]
    fn undo_restores() {
        let mut c = Composer::new();
        c.insert_str("abc");
        c.backspace();
        c.undo();
        assert_eq!(c.text(), "abc");
    }

    #[test]
    fn stash_parks_buffer() {
        let mut c = Composer::new();
        c.insert_str("work in progress");
        c.stash();
        assert!(c.is_empty());
        c.stash();
        assert_eq!(c.text(), "work in progress");
    }

    #[test]
    fn delete_word_back_eats_word_then_space() {
        let mut c = Composer::new();
        c.insert_str("foo bar");
        c.delete_word_back();
        assert_eq!(c.text(), "foo ");
        c.delete_word_back();
        assert_eq!(c.text(), "");
        // No-op at empty leaves no undo entries.
        c.delete_word_back();
        c.insert_str("x");
        c.undo();
        assert_eq!(c.text(), "");
    }
}
