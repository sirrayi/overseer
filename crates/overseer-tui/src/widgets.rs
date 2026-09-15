//! Live-region widgets (P2.4/2.5/2.7): working indicator, queued-message
//! strip, permission dialog, mode badge + status line, help panel.

use ratatui::text::{Line, Span};

use overseer_core::perm::{AskRequest, Preset};

use crate::theme;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// One-row working indicator: spinner + phase + elapsed + token count +
/// an *accurate* interrupt hint — it's only drawn while the engine is
/// truly interruptible (a run is live on the worker thread).
pub fn indicator(
    phase: &str,
    elapsed_s: u64,
    tokens: u64,
    tick: usize,
    reduce_motion: bool,
) -> Line<'static> {
    // REDUCE_MOTION: a static glyph — the elapsed counter still ticks
    // (information, not decoration).
    let glyph = if reduce_motion {
        "●"
    } else {
        SPINNER[tick % SPINNER.len()]
    };
    Line::from(vec![
        Span::styled(format!("{glyph} "), theme::spinner()),
        Span::styled(phase.to_string(), theme::dim()),
        Span::styled(format!("  {elapsed_s}s  "), theme::dim()),
        Span::styled(format!("{tokens} tok"), theme::dim()),
        Span::styled("  (esc to interrupt)", theme::dim()),
    ])
}

/// Queued ≠ sent: queued steering messages render dimmed, indexed for
/// per-item cancel (Ctrl+Q removes the newest).
pub fn queue_strip(queued: &[String]) -> Vec<Line<'static>> {
    queued
        .iter()
        .enumerate()
        .map(|(i, q)| {
            Line::from(vec![
                Span::styled(format!("  queued[{i}] "), theme::queue()),
                Span::styled(truncate(q, 60), theme::queue()),
            ])
        })
        .collect()
}

/// Permission dialog (P2.5): typed preview + once/session/deny. A ~200 ms
/// grace period after opening swallows dialog keys so a keystroke in
/// flight can't accidentally answer (anti-misclick). Selection is
/// arrow/Enter-driven — ordinary text keeps flowing to the composer, so
/// the prompt never steals a keystroke the user meant to type.
pub struct Dialog {
    pub req: AskRequest,
    /// Instant the dialog opened; dialog keys arm after `grace` elapses.
    pub opened: std::time::Instant,
    /// Highlighted option: 0 = allow once, 1 = allow session, 2 = deny.
    pub selected: usize,
}

impl Dialog {
    pub const GRACE_MS: u128 = 200;
    const N_OPTS: usize = 4;

    pub fn armed(&self) -> bool {
        self.opened.elapsed().as_millis() >= Self::GRACE_MS
    }

    pub fn move_sel(&mut self, dir: i32) {
        self.selected = (self.selected as i32 + dir).rem_euclid(Self::N_OPTS as i32) as usize;
    }

    /// The highlighted option as a decision.
    pub fn confirm(&self) -> overseer_core::perm::AskDecision {
        use overseer_core::perm::AskDecision as D;
        match self.selected {
            0 => D::AllowOnce,
            1 => D::AllowSession,
            2 => D::AllowAlways,
            _ => D::Deny,
        }
    }

    /// Digit shortcut → decision, or None during the grace window.
    pub fn resolve(&self, key: char) -> Option<overseer_core::perm::AskDecision> {
        if !self.armed() {
            return None;
        }
        use overseer_core::perm::AskDecision as D;
        match key {
            '1' => Some(D::AllowOnce),
            '2' => Some(D::AllowSession),
            '3' => Some(D::AllowAlways),
            '4' => Some(D::Deny),
            _ => None,
        }
    }

    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let w = width.max(8) as usize;
        let mut out = vec![crate::cells::wrap_styled(
            vec![
                Span::styled("permission: ", theme::dialog_key()),
                Span::styled(self.req.tool.clone(), theme::dialog()),
                Span::styled(format!(" — {}", self.req.reason), theme::dim()),
            ],
            w,
        )]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        // Typed preview: the deciding evidence differs per tool —
        // bash wants the command line, edit wants the patch, write
        // wants the target + head of content.
        match self.req.tool.as_str() {
            "bash" => {
                if let Some(cmd) = self.req.input.get("command").and_then(|v| v.as_str()) {
                    for l in cmd.lines().take(3) {
                        out.extend(crate::cells::wrap_styled(
                            vec![
                                Span::styled("  $ ", theme::dialog_key()),
                                Span::styled(l.to_string(), theme::dialog()),
                            ],
                            w,
                        ));
                    }
                }
            }
            "edit" => {
                if let Some(p) = self.req.input.get("path").and_then(|v| v.as_str()) {
                    out.push(Line::from(Span::styled(
                        format!("  {p}"),
                        theme::dialog(),
                    )));
                }
                for (mark, key, style) in [
                    ("- ", "old_string", theme::error()),
                    ("+ ", "new_string", theme::meta()),
                ] {
                    if let Some(s) = self.req.input.get(key).and_then(|v| v.as_str()) {
                        for l in s.lines().take(4) {
                            out.extend(crate::cells::wrap_styled(
                                vec![Span::styled(format!("  {mark}{l}"), style)],
                                w,
                            ));
                        }
                    }
                }
            }
            "write" => {
                if let Some(p) = self.req.input.get("path").and_then(|v| v.as_str()) {
                    out.push(Line::from(Span::styled(
                        format!("  {p}"),
                        theme::dialog(),
                    )));
                }
                if let Some(c) = self.req.input.get("content").and_then(|v| v.as_str()) {
                    for l in c.lines().take(4) {
                        out.extend(crate::cells::wrap_styled(
                            vec![
                                Span::styled("  │ ", theme::dialog_key()),
                                Span::styled(l.to_string(), theme::dialog()),
                            ],
                            w,
                        ));
                    }
                }
            }
            _ => {
                if let Some(p) = self.req.input.get("path").and_then(|v| v.as_str()) {
                    out.extend(crate::cells::wrap_styled(
                        vec![
                            Span::styled("  ", theme::dialog()),
                            Span::styled(p.to_string(), theme::dialog()),
                        ],
                        w,
                    ));
                }
            }
        }
        let labels = ["[1] once", "[2] session", "[3] always", "[4] deny"];
        if self.armed() {
            let mut spans = Vec::new();
            for (i, l) in labels.iter().enumerate() {
                if i == self.selected {
                    spans.push(Span::styled(format!(" {l} "), theme::dialog_sel()));
                } else {
                    spans.push(Span::styled(format!(" {l} "), theme::dialog_key()));
                }
                if i + 1 < labels.len() {
                    spans.push(Span::raw("  "));
                }
            }
            spans.push(Span::styled("  (←→ ⏎)".to_string(), theme::dim()));
            out.push(Line::from(spans));
        } else {
            out.push(Line::from(Span::styled("…".to_string(), theme::dialog_key())));
        }
        out
    }
}

/// Bottom status line: mode badge + cwd + model + session cost.
pub fn status_line(
    preset: Preset,
    cwd: &str,
    model: &str,
    cost: f64,
    width: u16,
) -> Line<'static> {
    let (label, badge) = match preset {
        Preset::WorkspaceWrite => (" workspace ", theme::badge()),
        Preset::ReadOnly => (" read-only ", theme::badge_ro()),
        Preset::Plan => (" plan ", theme::badge_plan()),
    };
    let right = format!("{model} · ${:.4}", cost);
    let left_w = 10 + cwd.len();
    let pad = (width as usize).saturating_sub(left_w + right.len()).max(1);
    Line::from(vec![
        Span::styled(label, badge),
        Span::styled(format!(" {cwd}"), theme::status()),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, theme::status()),
    ])
}

/// `?`-on-empty-input help panel (P2.3).
pub fn help_panel() -> Vec<Line<'static>> {
    let rows: &[&str] = &[
        "enter        submit          ctrl+j / alt+enter   newline",
        "esc          interrupt run / clear input",
        "shift+tab    cycle mode (workspace → read-only → plan)",
        "ctrl+t       toggle plan    ctrl+x  cancel queued msg",
        "ctrl+s       stash draft    ctrl+_  undo    ctrl+w  del word",
        "up/down      history        ctrl+c  clear   ctrl+d  quit",
        "ctrl+p       sessions       ctrl+o  search  ctrl+y  copy reply",
        "alt+e / /edit               draft in $EDITOR",
        "tab          complete /cmd or @path    !cmd   run shell locally",
        "/sessions /fork /rewind /diff /approve /search /help /quit",
    ];
    rows.iter()
        .map(|r| Line::from(Span::styled(r.to_string(), theme::dim())))
        .collect()
}

fn truncate(s: &str, n: usize) -> String {
    let mut g = unicode_segmentation::UnicodeSegmentation::graphemes(s, true);
    let taken: String = g.by_ref().take(n).collect();
    if g.next().is_some() {
        format!("{taken}…")
    } else {
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduced_motion_uses_static_glyph() {
        let animated = indicator("working", 1, 10, 0, false);
        let still = indicator("working", 1, 10, 0, true);
        let first = |l: &Line| l.spans[0].content.to_string();
        assert_eq!(first(&still), "● ");
        assert!(SPINNER.contains(&first(&animated).trim()));
    }
}
