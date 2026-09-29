//! Semantic style tokens (P2.8): one palette, no raw ANSI scattered
//! through widgets. The active palette is chosen once at startup —
//! `Theme::detect` maps capabilities + env to a variant — and read via
//! the accessor functions below. Most hues are 4-bit-ANSI-safe —
//! user-remappable terminal colors are an a11y feature; the three
//! truecolor accents (`user`/`prompt`/`user_bg`) fall back to ANSI-16
//! via `Theme::ansi` on terminals without truecolor/256 support.
//!
//! `graphite` (L2) is the truecolor minimal palette: colour marks
//! state, never categories — ok/err/warn/meta only. Selection is
//! white + underline, not a colour block.
//!
//! Selection: `OVERSEER_THEME=mono|default|high-contrast|graphite`
//! wins, then `caps.color` — `Mono` (NO_COLOR / TERM=dumb) forces
//! `mono`, `Ansi16` gets `ansi`, Ansi256 gets `default`, TrueColor
//! gets `graphite`.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Primary voice: assistant narration, markdown body.
    pub text: Style,
    pub user: Style,
    pub dim: Style,
    /// Quieter than dim: run summary, hints, footer marker.
    pub faint: Style,
    pub reasoning: Style,
    pub tool: Style,
    pub tool_ok: Style,
    pub tool_err: Style,
    /// Warnings that are not errors: taint, approval-needed, stuck.
    pub warn: Style,
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
    /// Full-row band behind a sent user prompt.
    pub user_bg: Style,
}

impl Theme {
    /// The shipped palette. `user`/`prompt`/`user_bg` are truecolor
    /// accents — detect() only picks this for truecolor/256 terminals;
    /// `ansi` substitutes the nearest ANSI-16 colors below that.
    pub const fn default() -> Self {
        Theme {
            text: Style::new(),
            user: Style::new().fg(Color::Rgb(255, 255, 255)),
            dim: Style::new().fg(Color::DarkGray),
            faint: Style::new().fg(Color::DarkGray),
            reasoning: Style::new().fg(Color::DarkGray),
            tool: Style::new().fg(Color::Yellow),
            tool_ok: Style::new().fg(Color::Green),
            tool_err: Style::new().fg(Color::Red),
            warn: Style::new().fg(Color::Yellow),
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
            prompt: Style::new().fg(Color::Rgb(255, 255, 255)),
            link: Style::new()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED),
            user_bg: Style::new().bg(Color::Rgb(38, 42, 54)),
        }
    }

    /// 16-colour terminals: the default palette with the truecolor
    /// accents swapped for their nearest ANSI-16 colors.
    pub const fn ansi() -> Self {
        let mut t = Theme::default();
        t.user = Style::new().fg(Color::White);
        t.prompt = Style::new().fg(Color::White);
        t.user_bg = Style::new().bg(Color::DarkGray);
        t
    }

    /// NO_COLOR / screen readers: every token is plain default style.
    pub const fn mono() -> Self {
        Theme {
            text: Style::new(),
            user: Style::new(),
            dim: Style::new(),
            faint: Style::new(),
            reasoning: Style::new(),
            tool: Style::new(),
            tool_ok: Style::new(),
            tool_err: Style::new(),
            warn: Style::new(),
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
            user_bg: Style::new().add_modifier(Modifier::REVERSED),
        }
    }

    /// Low-vision palette: bright fg everywhere, selection inverts on
    /// white, accents bold. Still ANSI-16 — no truecolor assumptions.
    pub const fn high_contrast() -> Self {
        let mut t = Theme::default();
        t.text = Style::new().fg(Color::White);
        t.dim = Style::new().fg(Color::Gray);
        t.faint = Style::new().fg(Color::Gray);
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
        t.warn = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        t.user_bg = Style::new().bg(Color::DarkGray);
        t
    }

    /// L2 "graphite": minimal truecolor palette — colour marks state,
    /// never categories. Selection is white + underline, not a colour
    /// block; badges collapse to plain tinted text ("no badges").
    pub const fn graphite() -> Self {
        Theme {
            text: Style::new().fg(Color::Rgb(0xd4, 0xd6, 0xdb)),
            user: Style::new().fg(Color::Rgb(0xff, 0xff, 0xff)),
            dim: Style::new().fg(Color::Rgb(0x85, 0x8a, 0x94)),
            faint: Style::new().fg(Color::Rgb(0x50, 0x54, 0x5c)),
            reasoning: Style::new().fg(Color::Rgb(0x85, 0x8a, 0x94)),
            tool: Style::new().fg(Color::Rgb(0xa9, 0xad, 0xb6)),
            tool_ok: Style::new().fg(Color::Rgb(0x7f, 0xb5, 0x8a)),
            tool_err: Style::new().fg(Color::Rgb(0xd7, 0x7b, 0x7b)),
            warn: Style::new().fg(Color::Rgb(0xd4, 0xb2, 0x6a)),
            meta: Style::new().fg(Color::Rgb(0x8e, 0xa4, 0xc8)),
            error: Style::new().fg(Color::Rgb(0xd7, 0x7b, 0x7b)),
            badge: Style::new().fg(Color::Rgb(0xa9, 0xad, 0xb6)),
            badge_plan: Style::new().fg(Color::Rgb(0x8e, 0xa4, 0xc8)),
            badge_ro: Style::new().fg(Color::Rgb(0xd4, 0xb2, 0x6a)),
            status: Style::new().fg(Color::Rgb(0x50, 0x54, 0x5c)),
            spinner: Style::new().fg(Color::Rgb(0xa9, 0xad, 0xb6)),
            queue: Style::new().fg(Color::Rgb(0x50, 0x54, 0x5c)),
            dialog: Style::new().fg(Color::Rgb(0xd4, 0xd6, 0xdb)),
            dialog_key: Style::new().fg(Color::Rgb(0xa9, 0xad, 0xb6)),
            dialog_sel: Style::new()
                .fg(Color::Rgb(0xff, 0xff, 0xff))
                .add_modifier(Modifier::UNDERLINED),
            code: Style::new()
                .fg(Color::Rgb(0xe6, 0xe8, 0xec))
                .bg(Color::Rgb(0x24, 0x26, 0x2b)),
            prompt: Style::new().fg(Color::Rgb(0xff, 0xff, 0xff)),
            link: Style::new()
                .fg(Color::Rgb(0xa9, 0xc1, 0xe8))
                .add_modifier(Modifier::UNDERLINED),
            user_bg: Style::new().bg(Color::Rgb(0x25, 0x27, 0x2c)),
        }
    }

    /// `OVERSEER_THEME` > probe result. Called once by `run` before any
    /// widget renders.
    pub fn detect(caps: &crate::probe::Caps) -> Theme {
        match std::env::var("OVERSEER_THEME").as_deref() {
            Ok("mono") => return Theme::mono(),
            Ok("high-contrast") | Ok("high_contrast") => return Theme::high_contrast(),
            Ok("default") => return Theme::default(),
            Ok("graphite") => return Theme::graphite(),
            _ => {}
        }
        match caps.color {
            crate::probe::ColorDepth::Mono => Theme::mono(),
            crate::probe::ColorDepth::Ansi16 => Theme::ansi(),
            crate::probe::ColorDepth::Ansi256 => Theme::default(),
            crate::probe::ColorDepth::TrueColor => Theme::graphite(),
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
    text, user, dim, faint, reasoning, tool, tool_ok, tool_err, warn, meta,
    error, badge, badge_plan, badge_ro, status, spinner, queue, dialog,
    dialog_key, dialog_sel, code, prompt, link, user_bg,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Caps, ColorDepth};

    #[test]
    fn ansi_falls_back_from_rgb_accents() {
        let ansi = Theme::ansi();
        assert_eq!(ansi.user.fg, Some(Color::White));
        assert_eq!(ansi.prompt.fg, Some(Color::White));
        assert_eq!(ansi.user_bg.bg, Some(Color::DarkGray));

        let tc = Theme::default();
        assert_eq!(tc.user.fg, Some(Color::Rgb(255, 255, 255)));
        assert_eq!(tc.user_bg.bg, Some(Color::Rgb(38, 42, 54)));

        let mono = Theme::mono();
        assert_eq!(mono.user.fg, None);
        assert_eq!(mono.user_bg.bg, None);
    }

    #[test]
    fn detect_maps_depth_to_palette() {
        let caps = |color| Caps {
            color,
            ..Caps::default()
        };
        // Only when OVERSEER_THEME isn't overriding — env wins above.
        std::env::remove_var("OVERSEER_THEME");
        assert_eq!(Theme::detect(&caps(ColorDepth::Mono)), Theme::mono());
        assert_eq!(Theme::detect(&caps(ColorDepth::Ansi16)), Theme::ansi());
        assert_eq!(Theme::detect(&caps(ColorDepth::Ansi256)), Theme::default());
        assert_eq!(
            Theme::detect(&caps(ColorDepth::TrueColor)),
            Theme::graphite()
        );
    }

    #[test]
    fn graphite_selection_is_underline_not_block() {
        let g = Theme::graphite();
        assert_eq!(g.dialog_sel.fg, Some(Color::Rgb(0xff, 0xff, 0xff)));
        assert_eq!(g.dialog_sel.bg, None);
        assert!(g.dialog_sel.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(g.code.bg, Some(Color::Rgb(0x24, 0x26, 0x2b)));
        // "No badges": mode chips render as tinted text, not bg blocks.
        assert_eq!(g.badge.bg, None);
    }
}
