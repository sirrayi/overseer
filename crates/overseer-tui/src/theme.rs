//! Semantic style tokens (P2: one palette, no raw ANSI scattered through
//! widgets). 4-bit-ANSI-safe core — user-remappable terminal colors are an
//! accessibility feature; richer themes land with the theme system (P2.8).

use ratatui::style::{Color, Modifier, Style};

pub const USER: Style = Style::new().fg(Color::Cyan);
pub const ASSISTANT: Style = Style::new();
pub const DIM: Style = Style::new().fg(Color::DarkGray);
pub const REASONING: Style = Style::new().fg(Color::DarkGray);
pub const TOOL: Style = Style::new().fg(Color::Yellow);
pub const TOOL_OK: Style = Style::new().fg(Color::Green);
pub const TOOL_ERR: Style = Style::new().fg(Color::Red);
pub const META: Style = Style::new().fg(Color::Magenta);
pub const ERROR: Style = Style::new().fg(Color::Red);
pub const BADGE: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Cyan)
    .add_modifier(Modifier::BOLD);
pub const BADGE_PLAN: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Magenta)
    .add_modifier(Modifier::BOLD);
pub const BADGE_RO: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Yellow)
    .add_modifier(Modifier::BOLD);
pub const STATUS: Style = Style::new().fg(Color::DarkGray);
pub const SPINNER: Style = Style::new().fg(Color::Cyan);
pub const QUEUE: Style = Style::new().fg(Color::DarkGray);
pub const DIALOG: Style = Style::new().fg(Color::White);
pub const DIALOG_KEY: Style = Style::new()
    .fg(Color::Yellow)
    .add_modifier(Modifier::BOLD);
pub const DIALOG_SEL: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Yellow)
    .add_modifier(Modifier::BOLD);
pub const CODE: Style = Style::new().fg(Color::Green);
pub const PROMPT: Style = Style::new().fg(Color::Green);
