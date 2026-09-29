//! Ctrl-tab control panel rows + band rendering — split out of
//! `app.rs` (W3).

use super::*;

/// Panel tab order — ←/→ and Tab cycle, digits are not bound.
pub(crate) const PANEL_TABS: [&str; 4] = ["dashboard", "agents", "settings", "keys"];

impl App {
    /// The panel's bottom band (Full mode): a bare tab strip (key hint
    /// right-aligned), then `cap`-1 content rows at `scroll`.
    pub(crate) fn panel_band_lines(
        &self,
        tab: usize,
        scroll: usize,
        width: u16,
        cap: u16,
    ) -> Vec<Line<'static>> {
        use crate::theme;
        let mut strip = Vec::new();
        let mut used = 0usize;
        for (i, name) in PANEL_TABS.iter().enumerate() {
            strip.push(Span::styled(
                format!(" {name} "),
                if i == tab {
                    theme::dialog_sel()
                } else {
                    theme::dim()
                },
            ));
            used += name.len() + 2;
        }
        let hint = "↑/↓ · ←/→ · esc";
        // 1-col right margin — flush-to-edge reads as clipped at the
        // window border, same reason the divider insets 3px.
        let pad = (width as usize).saturating_sub(used + hint.chars().count() + 1);
        strip.push(Span::raw(" ".repeat(pad)));
        strip.push(Span::styled(hint.to_string(), theme::dim()));
        let mut out = vec![Line::from(strip)];
        let rows = self.panel_rows(tab);
        let body = (cap as usize).saturating_sub(1);
        let start = scroll.min(rows.len().saturating_sub(body));
        out.extend(rows.into_iter().skip(start).take(body));
        while out.len() < cap as usize {
            out.push(Line::default());
        }
        out
    }

    /// One `label  value` row list per panel tab — read-only v1.
    pub(crate) fn panel_rows(&self, tab: usize) -> Vec<Line<'static>> {
        use crate::theme;
        let kv = |k: &str, v: String| {
            Line::from(vec![
                Span::styled(format!("  {k:<12}"), theme::dim()),
                Span::styled(v, theme::dialog()),
            ])
        };
        match tab {
            0 => {
                let state = match &self.run {
                    RunState::Running { started, phase, .. } => format!(
                        "running — {phase} · {}s · {} queued",
                        started.elapsed().as_secs(),
                        self.pending_queue.len()
                    ),
                    RunState::Idle if !self.pending_queue.is_empty() => {
                        format!("idle · {} queued", self.pending_queue.len())
                    }
                    RunState::Idle => "idle".to_string(),
                };
                vec![
                    kv(
                        "session",
                        self.session_dir
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| self.session_dir.display().to_string()),
                    ),
                    kv("cwd", self.cwd.clone()),
                    kv("model", self.model.clone()),
                    kv(
                        "mode",
                        crate::widgets::preset_badge(self.preset)
                            .0
                            .trim()
                            .to_string(),
                    ),
                    kv("state", state),
                    kv("tokens", self.tokens.to_string()),
                    kv("cost", format!("${:.4}", self.cost)),
                    kv("transcript", format!("{} cells", self.history.len())),
                ]
            }
            1 => {
                if self.agents.is_empty() {
                    return vec![Line::from(Span::styled(
                        "  none yet — task-tool spawns land here",
                        theme::dim(),
                    ))];
                }
                self.agents
                    .iter()
                    .rev()
                    .map(|a| {
                        let st = match a.state {
                            "done" => theme::tool_ok(),
                            "failed" => theme::tool_err(),
                            _ => theme::spinner(),
                        };
                        Line::from(vec![
                            Span::styled(format!("  {:<9}", a.state), st),
                            Span::styled(
                                format!(
                                    "{:<9}",
                                    if a.bg {
                                        format!("{}·bg", a.mode)
                                    } else {
                                        a.mode.to_string()
                                    }
                                ),
                                theme::dim(),
                            ),
                            Span::styled(a.prompt.clone(), theme::dialog()),
                        ])
                    })
                    .collect()
            }
            2 => vec![
                kv(
                    "theme",
                    std::env::var("OVERSEER_THEME").unwrap_or_else(|_| "auto".into()),
                ),
                kv(
                    "osc",
                    if self.osc {
                        "on — links · clipboard · marks"
                    } else {
                        "off"
                    }
                    .to_string(),
                ),
                kv(
                    "motion",
                    if self.reduce_motion {
                        "reduced"
                    } else {
                        "animated"
                    }
                    .to_string(),
                ),
                kv(
                    "surface",
                    match self.mode {
                        UiMode::Full => "full-window",
                        UiMode::Inline => "inline",
                    }
                    .to_string(),
                ),
                kv("session dir", self.session_dir.display().to_string()),
                kv("rules", "~/.overseer/rules".to_string()),
            ],
            _ => crate::widgets::help_panel(),
        }
    }
}
