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
            // §4: active tab = white + underline, inactive = faint —
            // no colour blocks in any palette.
            strip.push(Span::styled(
                format!(" {name} "),
                if i == tab {
                    theme::tab_active()
                } else {
                    theme::tab_idle()
                },
            ));
            used += name.len() + 2;
        }
        let hint = "↑/↓ · ←/→ · esc";
        // 1-col right margin — flush-to-edge reads as clipped at the
        // window border, same reason the divider insets 3px.
        let pad = (width as usize).saturating_sub(used + hint.chars().count() + 1);
        strip.push(Span::raw(" ".repeat(pad)));
        strip.push(Span::styled(hint.to_string(), theme::faint()));
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
    /// §4 two-column: keys in `dim`, values in `text`.
    pub(crate) fn panel_rows(&self, tab: usize) -> Vec<Line<'static>> {
        use crate::theme;
        let kv = |k: &str, v: String| {
            Line::from(vec![
                Span::styled(format!("  {k:<12}"), theme::dim()),
                Span::styled(v, theme::text()),
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
                    kv("cwd", tilde_home(std::path::Path::new(&self.cwd))),
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
                    // DEFERRED(owner): real `87% · 41.2K read` once
                    // Agent::cache_stats()/CacheStats::hit_rate lands.
                    kv("cache", "—".to_string()),
                ]
            }
            1 => {
                if self.agents.is_empty() {
                    return vec![Line::from(Span::styled(
                        "  none yet — task-tool spawns land here",
                        theme::faint(),
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
                        let mode = if a.bg {
                            format!("{}·bg", a.mode)
                        } else {
                            a.mode.to_string()
                        };
                        Line::from(vec![
                            Span::styled(format!("  {:<10}", a.state), st),
                            Span::styled(format!("{mode} — {}", a.prompt), theme::text()),
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
                kv("session dir", tilde_home(&self.session_dir)),
                kv("rules", "~/.overseer/rules".to_string()),
            ],
            _ => keys_rows(),
        }
    }
}

/// Replace the $HOME prefix with `~` — panel paths stay short.
fn tilde_home(p: &std::path::Path) -> String {
    match std::env::var_os("HOME") {
        Some(h) => match p.strip_prefix(std::path::Path::new(&h)) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        },
        None => p.display().to_string(),
    }
}

/// §4 keys tab: grouped, one blank row between groups, key column 14
/// wide in `dim`, action in `text`.
const KEY_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "send",
        &[
            ("enter", "submit"),
            ("ctrl+j", "newline"),
            ("esc", "interrupt / clear"),
        ],
    ),
    (
        "navigate",
        &[
            ("↑", "panel (empty) / history"),
            ("pgup/pgdn", "scroll"),
            ("ctrl+p", "sessions"),
            ("ctrl+o", "search"),
        ],
    ),
    (
        "edit",
        &[
            ("ctrl+w", "word"),
            ("ctrl+_", "undo"),
            ("ctrl+s", "stash"),
            ("alt+e", "editor"),
            ("tab", "complete"),
        ],
    ),
    (
        "modes",
        &[
            ("shift+tab", "cycle mode"),
            ("ctrl+t", "plan"),
            ("ctrl+x", "cancel queued"),
            ("ctrl+y", "copy reply"),
        ],
    ),
    (
        "shell and commands",
        &[
            ("!cmd", "local shell"),
            (
                "/…",
                "sessions · fork · rewind · diff · approve · search · help · quit",
            ),
        ],
    ),
];

fn keys_rows() -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (group, rows) in KEY_GROUPS {
        if !out.is_empty() {
            out.push(Line::default());
        }
        out.push(Line::from(Span::styled(
            format!("  {group}"),
            crate::theme::faint(),
        )));
        for (key, action) in *rows {
            out.push(Line::from(vec![
                Span::styled(format!("    {key:<14}"), crate::theme::dim()),
                Span::styled((*action).to_string(), crate::theme::text()),
            ]));
        }
    }
    out
}
