//! Transcript cells (P2.2): the event stream reduces to immutable cells
//! that flush to native scrollback via `insert_before`. Completed content
//! is append-only — never re-measured per frame (the O(n) transcript
//! failure pattern). Live/in-flight state renders in the managed region.
//!
//! Collapse contract: tool calls render one summary line; errors attach
//! a short output tail. Expansion is an overlay concern (P2.9) — cells
//! flushed to scrollback are immutable.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use overseer_core::event::{Event, EventKind};
use overseer_core::ir::Block;

use crate::{markdown, theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Err,
    Denied,
    Skipped,
}

#[derive(Debug, Clone)]
pub enum Cell {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Tool {
        id: String,
        name: String,
        summary: String,
        status: ToolStatus,
        output: Option<String>,
        /// File the call touched — emitted as an OSC 8 link line when
        /// the cell flushes to scrollback.
        link: Option<std::path::PathBuf>,
    },
    /// Plan artifact (P1.7 stays user-visible): markdown checklist.
    Plan {
        markdown: String,
    },
    Meta {
        style: Style,
        text: String,
    },
}

impl Cell {
    /// Unstyled searchable text — the transcript overlay's filter corpus.
    pub fn plain(&self) -> String {
        match self {
            Cell::User { text } | Cell::Assistant { text } | Cell::Reasoning { text } => {
                text.clone()
            }
            Cell::Tool {
                name,
                summary,
                output,
                ..
            } => format!("{name} {summary} {}", output.as_deref().unwrap_or("")),
            Cell::Plan { markdown } => markdown.clone(),
            Cell::Meta { text, .. } => text.clone(),
        }
    }

    /// A file path worth hyperlinking when this cell flushes (OSC 8).
    pub fn link_path(&self) -> Option<&std::path::Path> {
        match self {
            Cell::Tool { link, .. } => link.as_deref(),
            _ => None,
        }
    }

    /// Render at `width` columns. Cells are height-cacheable per width —
    /// the transcript only ever appends.
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let w = width.max(8) as usize;
        match self {
            Cell::User { text } => {
                let mut out = Vec::new();
                for (i, l) in text.lines().enumerate() {
                    let prefix = if i == 0 { "❯ " } else { "  " };
                    out.extend(wrap_styled(
                        vec![
                            Span::styled(prefix, theme::prompt()),
                            Span::styled(l.to_string(), theme::user()),
                        ],
                        w,
                    ));
                }
                out
            }
            Cell::Assistant { text } => markdown::render(text)
                .into_iter()
                .flat_map(|l| wrap_line(l, w))
                .collect(),
            Cell::Reasoning { text } => {
                let mut out = vec![Line::from(Span::styled("thinking", theme::dim()))];
                for l in text.lines().take(6) {
                    out.extend(wrap_styled(
                        vec![Span::styled(format!("  {l}"), theme::reasoning())],
                        w,
                    ));
                }
                out
            }
            Cell::Plan { markdown } => {
                let mut out = Vec::new();
                for l in markdown.lines() {
                    out.extend(wrap_styled(
                        vec![Span::styled(l.to_string(), theme::meta())],
                        w,
                    ));
                }
                out
            }
            Cell::Tool {
                name,
                summary,
                status,
                output,
                ..
            } => {
                let (glyph, st) = match status {
                    ToolStatus::Running => ("◌", theme::tool()),
                    ToolStatus::Ok => ("✓", theme::tool_ok()),
                    ToolStatus::Err | ToolStatus::Skipped => ("✗", theme::tool_err()),
                    ToolStatus::Denied => ("⊘", theme::tool_err()),
                };
                let head = vec![
                    Span::styled(format!("{glyph} "), st),
                    Span::styled(name.clone(), theme::tool()),
                    Span::styled(format!(" {summary}"), theme::dim()),
                ];
                let mut out = wrap_styled(head, w);
                // Errors stay legible in scrollback: a short tail of the
                // output is part of the immutable cell.
                if matches!(status, ToolStatus::Err | ToolStatus::Denied) {
                    if let Some(o) = output {
                        for l in o.lines().take(6) {
                            out.extend(wrap_styled(
                                vec![Span::styled(format!("    {l}"), theme::dim())],
                                w,
                            ));
                        }
                    }
                }
                out
            }
            Cell::Meta { style, text } => text
                .lines()
                .flat_map(|l| wrap_styled(vec![Span::styled(l.to_string(), *style)], w))
                .collect(),
        }
    }

    /// Like `lines` but tool cells render their captured output too —
    /// the transcript overlay's expand toggle (collapsible tool blocks,
    /// P2.9). Capped at 16 output lines per call: a spilled `read` of a
    /// huge file stays browseable without flooding the pager.
    pub fn lines_expanded(&self, width: u16) -> Vec<Line<'static>> {
        let mut out = self.lines(width);
        if let Cell::Tool {
            status,
            output: Some(output),
            ..
        } = self
        {
            // Err/Denied cells already print a 6-line tail in `lines`;
            // expansion is only additive for the other statuses.
            if !matches!(status, ToolStatus::Err | ToolStatus::Denied) {
                let w = (width.max(8)) as usize;
                for l in output.lines().take(16) {
                    out.extend(wrap_styled(
                        vec![Span::styled(format!("    {l}"), theme::dim())],
                        w,
                    ));
                }
                if output.lines().count() > 16 {
                    out.push(Line::from(Span::styled(
                        "    … (truncated)".to_string(),
                        theme::dim(),
                    )));
                }
            }
        }
        out
    }
}

/// One-line summary of a tool call's input — what the collapsed row shows.
pub fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "bash" => truncate(&s("command").replace('\n', " ⏎ "), 80),
        "read" => {
            let mut p = s("path").to_string();
            if let Some(o) = input.get("offset").and_then(|v| v.as_u64()) {
                p.push_str(&format!(":{o}"));
            }
            p
        }
        "write" | "edit" => s("path").to_string(),
        "grep" => format!("{} {}", s("pattern"), s("path")),
        "glob" => s("pattern").to_string(),
        "plan" => format!(
            "{} item(s)",
            input
                .get("items")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0)
        ),
        "task" => truncate(s("prompt"), 80),
        _ => truncate(&serde_json::to_string(input).unwrap_or_default(), 80),
    }
}

fn truncate(s: &str, n: usize) -> String {
    let mut g = s.graphemes(true);
    let taken: String = g.by_ref().take(n).collect();
    if g.next().is_some() {
        format!("{taken}…")
    } else {
        taken
    }
}

/// Reduce one engine event to zero or more cells. ToolCallStart/ToolResult
/// merge into a single Tool cell — `feed` returns the cell to insert or a
/// mutation of an existing one via `ToolUpdate`.
pub enum Feed {
    NewCells(Vec<Cell>),
    /// Finalize a running tool cell in place (matched by call id).
    ToolDone {
        call_id: String,
        status: ToolStatus,
        output: Option<String>,
    },
    Ignore,
}

pub fn feed(ev: &Event) -> Feed {
    match &ev.kind {
        EventKind::SessionStart { .. } => Feed::Ignore,
        EventKind::UserInput { text } => Feed::NewCells(vec![Cell::User { text: text.clone() }]),
        EventKind::ModelResponse { blocks, .. } => {
            let mut cells = Vec::new();
            for b in blocks {
                match b {
                    Block::Text { text } if !text.trim().is_empty() => {
                        cells.push(Cell::Assistant { text: text.clone() })
                    }
                    Block::Reasoning { raw } => {
                        let t = raw
                            .get("thinking")
                            .or_else(|| raw.get("text"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !t.trim().is_empty() {
                            cells.push(Cell::Reasoning { text: t });
                        }
                    }
                    // ToolCall blocks render via ToolCallStart events —
                    // the input arrives there too, dedup by call id.
                    _ => {}
                }
            }
            Feed::NewCells(cells)
        }
        EventKind::ToolCallStart {
            call_id,
            name,
            input,
        } => Feed::NewCells(vec![Cell::Tool {
            id: call_id.clone(),
            name: name.clone(),
            summary: tool_summary(name, input),
            status: ToolStatus::Running,
            output: None,
            link: input
                .get("path")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from),
        }]),
        EventKind::ToolResult {
            call_id,
            name,
            content,
            is_error,
            denied,
            ..
        } => {
            let status = if *denied {
                ToolStatus::Denied
            } else if *is_error {
                ToolStatus::Err
            } else {
                ToolStatus::Ok
            };
            // The plan artifact renders as its own visible cell (P1.7).
            if name == "plan" && !*is_error {
                return Feed::NewCells(vec![
                    plan_marker(),
                    Cell::Plan {
                        markdown: content.clone(),
                    },
                ]);
            }
            Feed::ToolDone {
                call_id: call_id.clone(),
                status,
                output: Some(content.clone()),
            }
        }
        EventKind::Nudge { text } => Feed::NewCells(vec![Cell::Meta {
            style: theme::meta(),
            text: format!("⟲ {text}"),
        }]),
        EventKind::SubagentDone { task_id: id, trace } => Feed::NewCells(vec![Cell::Meta {
            style: theme::meta(),
            text: format!("⤷ subagent {id} finished — trace {}", trace),
        }]),
        EventKind::Tainted { detail } => Feed::NewCells(vec![Cell::Meta {
            style: theme::error(),
            text: format!("⛨ {detail} — side effects now need confirmation"),
        }]),
        EventKind::StuckDetected { pattern } => Feed::NewCells(vec![Cell::Meta {
            style: theme::error(),
            text: format!("⚠ stuck: {pattern}"),
        }]),
        EventKind::Compaction { tail_from, .. } => Feed::NewCells(vec![Cell::Meta {
            style: theme::dim(),
            text: format!("⧉ context compacted — events before e{tail_from} summarized"),
        }]),
        EventKind::Error { message } => Feed::NewCells(vec![Cell::Meta {
            style: theme::error(),
            text: format!("error: {message}"),
        }]),
        EventKind::RunEnd {
            stop_reason,
            steps,
            total_cost_usd,
        } => Feed::NewCells(vec![Cell::Meta {
            style: theme::dim(),
            text: format!("— {stop_reason} · {steps} step(s) · ${total_cost_usd:.4}"),
        }]),
        EventKind::TurnEnd { .. } => Feed::Ignore,
    }
}

fn plan_marker() -> Cell {
    Cell::Meta {
        style: theme::meta(),
        text: "plan".to_string(),
    }
}

/// Wrap a styled line at `width` columns, splitting on grapheme-cluster
/// boundaries (never mid-cluster — ucs-detect/Mode-2027 divergence means
/// only the maintained iterator is trustworthy). Style carries per span.
pub fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    wrap_styled(line.spans, width)
}

/// Wrap a span list to `width` display columns. O(len) — the transcript
/// append cost stays constant per cell.
pub fn wrap_styled(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;

    for span in spans {
        let style = span.style;
        for g in span.content.as_ref().graphemes(true) {
            let gw = UnicodeWidthStr::width(g);
            if g == "\n" || col + gw > width {
                out.push(Line::from(std::mem::take(&mut cur)));
                col = 0;
                // A space at the wrap point is consumed, not drawn.
                if g == "\n" || g == " " {
                    continue;
                }
            }
            // Merge same-style runs so lines stay span-light.
            match cur.last_mut() {
                Some(last) if last.style == style => last.content.to_mut().push_str(g),
                _ => cur.push(Span::styled(g.to_string(), style)),
            }
            col += gw;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(Line::from(cur));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    #[test]
    fn wrap_respects_graphemes() {
        let spans = vec![Span::styled("a👨‍👩‍👧b c", Style::new())];
        let out = wrap_styled(spans, 3);
        let joined: String = out
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert_eq!(joined, "a👨‍👩‍👧b c");
        // Family emoji is 2 wide + never split mid-cluster.
        assert!(out.iter().all(|l| {
            l.spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum::<usize>()
                <= 3
        }));
    }

    #[test]
    fn tool_summary_bash() {
        let s = tool_summary("bash", &serde_json::json!({"command": "cargo test"}));
        assert_eq!(s, "cargo test");
    }

    #[test]
    fn feed_merges_tool_lifecycle() {
        let ev = |kind| overseer_core::event::Event {
            id: 1,
            parent_id: None,
            ts_ms: 0,
            kind,
        };
        match feed(&ev(EventKind::ToolCallStart {
            call_id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        })) {
            Feed::NewCells(c) => assert!(matches!(c[0], Cell::Tool { .. })),
            _ => panic!(),
        }
        match feed(&ev(EventKind::ToolResult {
            call_id: "c1".into(),
            name: "bash".into(),
            content: "ok".into(),
            is_error: false,
            raw_bytes: 2,
            spilled_to: None,
            denied: false,
        })) {
            Feed::ToolDone {
                call_id, status, ..
            } => {
                assert_eq!(call_id, "c1");
                assert_eq!(status, ToolStatus::Ok);
            }
            _ => panic!(),
        }
    }
}
