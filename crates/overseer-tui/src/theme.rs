//! Semantic style tokens (P2.8): one palette, no raw ANSI scattered
//! through widgets. The active palette is chosen once at startup —
//! `Theme::detect` maps capabilities + env to a variant — and read via
//! the accessor functions below. 4-bit-ANSI-safe hues in the default
//! palette: user-remappable terminal colors are an a11y feature.
//!
//! Selection: `OVERSEER_THEME=mono|default|high-contrast` wins, then
//! `caps.color == Mono` (NO_COLOR / TERM=dumb) forces `mono`.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub user: Style,
    pub assistant: Style,
    pub dim: Style,
    pub reasoning: Style,
    pub tool: Style,
    pub tool_ok: Style,
    pub tool_err: Style,
    pub meta: Style,
    pub error: Style,
    pub badge: Style,
    pub badge_plan: Style,
    pub badge_ro: Style,
    pub status: Style,
    pub spinner: Style,
    pub queue: Style,
    pub dialog: Style,
    pub dialog_key: Style,
    pub dialog_sel: Style,
    pub code: Style,
    pub prompt: Style,
    pub link: Style,
}

impl Theme {
    /// The shipped palette — ANSI-16 hues only.
    pub const fn default() -> Self {
        Theme {
            user: Style::new().fg(Color::Cyan),
            assistant: Style::new(),
            dim: Style::new().fg(Color::DarkGray),
            reasoning: Style::new().fg(Color::DarkGray),
            tool: Style::new().fg(Color::Yellow),
            tool_ok: Style::new().fg(Color::Green),
            tool_err: Style::new().fg(Color::Red),
            meta: Style::new().fg(Color::Magenta),
            error: Style::new().fg(Color::Red),
            badge: Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            badge_plan: Style::new()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
            badge_ro: Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            status: Style::new().fg(Color::DarkGray),
            spinner: Style::new().fg(Color::Cyan),
            queue: Style::new().fg(Color::DarkGray),
            dialog: Style::new().fg(Color::White),
            dialog_key: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            dialog_sel: Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            code: Style::new().fg(Color::Green),
            prompt: Style::new().fg(Color::Green),
            link: Style::new()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED),
        }
    }

    /// NO_COLOR / screen readers: every token is plain default style.
    pub const fn mono() -> Self {
        Theme {
            user: Style::new(),
            assistant: Style::new(),
            dim: Style::new(),
            reasoning: Style::new(),
            tool: Style::new(),
            tool_ok: Style::new(),
            tool_err: Style::new(),
            meta: Style::new(),
            error: Style::new(),
            badge: Style::new().add_modifier(Modifier::REVERSED),
            badge_plan: Style::new().add_modifier(Modifier::REVERSED),
            badge_ro: Style::new().add_modifier(Modifier::REVERSED),
            status: Style::new(),
            spinner: Style::new(),
            queue: Style::new(),
            dialog: Style::new(),
            dialog_key: Style::new().add_modifier(Modifier::BOLD),
            dialog_sel: Style::new().add_modifier(Modifier::REVERSED),
            code: Style::new(),
            prompt: Style::new(),
            link: Style::new().add_modifier(Modifier::UNDERLINED),
        }
    }

    /// Low-vision palette: bright fg everywhere, selection inverts on
    /// white, accents bold. Still ANSI-16 — no truecolor assumptions.
    pub const fn high_contrast() -> Self {
        let mut t = Theme::default();
        t.dim = Style::new().fg(Color::Gray);
        t.reasoning = Style::new().fg(Color::Gray);
        t.status = Style::new().fg(Color::Gray);
        t.queue = Style::new().fg(Color::Gray);
        t.dialog = Style::new().fg(Color::White).add_modifier(Modifier::BOLD);
        t.dialog_sel = Style::new()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD);
        t.meta = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
        t.error = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
        t
    }

    /// `OVERSEER_THEME` > probe result. Called once by `run` before any
    /// widget renders.
    pub fn detect(caps: &crate::probe::Caps) -> Theme {
        match std::env::var("OVERSEER_THEME").as_deref() {
            Ok("mono") => return Theme::mono(),
            Ok("high-contrast") | Ok("high_contrast") => return Theme::high_contrast(),
            Ok("default") => return Theme::default(),
            _ => {}
        }
        if caps.color == crate::probe::ColorDepth::Mono {
            Theme::mono()
        } else {
            Theme::default()
        }
    }
}

static THEME: std::sync::OnceLock<Theme> = std::sync::OnceLock::new();

/// Install the palette (idempotent — first call wins so tests stay
/// deterministic on `default`).
pub fn set_theme(t: Theme) {
    let _ = THEME.set(t);
}

fn t() -> &'static Theme {
    THEME.get_or_init(Theme::default)
}

macro_rules! accessors {
    ($($name:ident),+ $(,)?) => {
        $(pub fn $name() -> Style { t().$name })+
    };
}

accessors! {
    user, assistant, dim, reasoning, tool, tool_ok, tool_err, meta, error,
    badge, badge_plan, badge_ro, status, spinner, queue, dialog, dialog_key,
    dialog_sel, code, prompt, link,
}
