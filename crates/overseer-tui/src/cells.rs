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
        /// Target emitted as an OSC 8 link line on flush (subagent
        /// traces) — the glyph row keeps just the word "trace".
        link: Option<std::path::PathBuf>,
    },
    /// End-of-run summary: right-aligned faint `steps · elapsed · cost`;
    /// `warn` prefixes the reason in warn colour on abnormal ends.
    End {
        text: String,
        warn: Option<String>,
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
            Cell::End { text, warn } => match warn {
                Some(w) => format!("{w} · {text}"),
                None => text.clone(),
            },
        }
    }

    /// A file path worth hyperlinking when this cell flushes (OSC 8).
    pub fn link_path(&self) -> Option<&std::path::Path> {
        match self {
            Cell::Tool { link, .. } => link.as_deref(),
            Cell::Meta { link, .. } => link.as_deref(),
            _ => None,
        }
    }

    /// Render at `width` columns. Cells are height-cacheable per width —
    /// the transcript only ever appends.
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        self.lines_at(width, None)
    }

    /// `tick` = Some((tick, reduce_motion)) only for the live region:
    /// a running tool's glyph cycles the spinner (frozen ◌ under
    /// REDUCE_MOTION). Scrollback cells always render the static arm.
    pub(crate) fn lines_at(&self, width: u16, tick: Option<(usize, bool)>) -> Vec<Line<'static>> {
        let w = width.max(8) as usize;
        match self {
            Cell::User { text } => {
                let bg = theme::user_bg();
                // One unstyled row of turn separation above the band —
                // renderers strip it when the cell tops the transcript.
                let mut out = vec![Line::default()];
                for (i, l) in text.lines().enumerate() {
                    let prefix = if i == 0 { "❯ " } else { "  " };
                    for line in wrap_styled(
                        vec![
                            Span::styled(prefix, theme::prompt()),
                            Span::styled(l.to_string(), theme::user()),
                        ],
                        w,
                    ) {
                        let pad = w.saturating_sub(line.width());
                        let mut line = line.patch_style(bg);
                        if pad > 0 {
                            line.spans.push(Span::styled(" ".repeat(pad), bg));
                        }
                        out.push(line);
                    }
                }
                out
            }
            Cell::Assistant { text } => {
                let mut out: Vec<Line> = markdown::render(text)
                    .into_iter()
                    .flat_map(|l| wrap_line(l, w))
                    .collect();
                // Fence lines carry the code bg across the block width —
                // only when the palette gives `code` a bg (graphite).
                if let Some(bg) = theme::code().bg {
                    for l in out.iter_mut() {
                        if !l.spans.is_empty() && l.spans.iter().all(|s| s.style.bg == Some(bg)) {
                            let pad = w.saturating_sub(l.width());
                            if pad > 0 {
                                l.spans
                                    .push(Span::styled(" ".repeat(pad), Style::new().bg(bg)));
                            }
                        }
                    }
                }
                out
            }
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
                let (glyph, gst) = match status {
                    // The live region cycles the spinner here; flushed
                    // cells freeze on ◌ (and always under REDUCE_MOTION).
                    ToolStatus::Running => match tick {
                        Some((t, false)) => (
                            crate::widgets::SPINNER[t % crate::widgets::SPINNER.len()],
                            theme::spinner(),
                        ),
                        _ => ("◌", theme::tool()),
                    },
                    ToolStatus::Ok => ("●", theme::tool_ok()),
                    ToolStatus::Err => ("●", theme::tool_err()),
                    ToolStatus::Denied => ("⊘", theme::tool_err()),
                    ToolStatus::Skipped => ("○", theme::faint()),
                };
                // `  ● bash  ls -la` — the glyph marks state, the name
                // is the tool colour, args dim, truncated at width-1.
                let head_w = 2 + 1 + 1 + UnicodeWidthStr::width(name.as_str()) + 2;
                let args = truncate(summary, w.saturating_sub(1 + head_w));
                let head = vec![
                    Span::styled(format!("  {glyph} "), gst),
                    Span::styled(name.clone(), theme::tool()),
                    Span::styled(format!("  {args}"), theme::dim()),
                ];
                let mut out = vec![Line::from(head)];
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
            Cell::Meta { style, text, .. } => text
                .lines()
                .flat_map(|l| wrap_styled(vec![Span::styled(l.to_string(), *style)], w))
                .collect(),
            Cell::End { text, warn } => {
                // Right-aligned run summary, one row, faint; an
                // abnormal stop reason prefixes it in warn.
                let warn_txt = warn.as_deref().unwrap_or("");
                let warn_w = if warn_txt.is_empty() {
                    0
                } else {
                    UnicodeWidthStr::width(warn_txt) + 3 // " · "
                };
                let pad = w.saturating_sub(warn_w + UnicodeWidthStr::width(text.as_str()) + 1);
                let mut spans = vec![Span::styled(" ".repeat(pad), Style::new())];
                if warn_w > 0 {
                    spans.push(Span::styled(format!("{warn_txt} · "), theme::warn()));
                }
                spans.push(Span::styled(text.clone(), theme::faint()));
                vec![Line::from(spans)]
            }
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
        "bash" => s("command").replace('\n', " ⏎ "),
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
        "task" => s("prompt").to_string(),
        _ => serde_json::to_string(input).unwrap_or_default(),
    }
}

/// §2: the sent-prompt band leads with one blank row of turn
/// separation — renderers call this on the transcript's first cell so
/// the session doesn't open on a gap.
pub(crate) fn strip_top_gap(cell: &Cell, lines: &mut Vec<Line<'static>>) {
    if matches!(cell, Cell::User { .. })
        && lines.first().map(|l| l.spans.is_empty()).unwrap_or(false)
    {
        lines.remove(0);
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
/// mutation of an existing one via `Feed::ToolDone`.
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

/// `run_elapsed` supplies the live run's wall-clock duration for the
/// RunEnd summary; replays and the line-mode surface pass None.
pub fn feed(ev: &Event, run_elapsed: Option<std::time::Duration>) -> Feed {
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
            style: theme::faint(),
            text: format!("  ⟲ {text}"),
            link: None,
        }]),
        EventKind::SubagentDone { task_id: id, trace } => Feed::NewCells(vec![Cell::Meta {
            style: theme::meta(),
            // The trace dir goes into the OSC 8 link, not the text.
            text: format!("  ↳ subagent {id} finished · trace"),
            link: Some(std::path::PathBuf::from(trace)),
        }]),
        EventKind::Tainted { detail } => Feed::NewCells(vec![Cell::Meta {
            style: theme::warn(),
            text: format!("  ! {detail} — side effects now ask first"),
            link: None,
        }]),
        // P7-3 audit-only computer-use record — no transcript cell.
        EventKind::ComputerAct { .. } => Feed::Ignore,
        EventKind::StuckDetected { pattern } => Feed::NewCells(vec![Cell::Meta {
            style: theme::warn(),
            text: format!("  ! stuck: {pattern}"),
            link: None,
        }]),
        EventKind::Compaction { tail_from, .. } => Feed::NewCells(vec![Cell::Meta {
            style: theme::faint(),
            text: format!("  ⋯ context compacted — events before e{tail_from} summarized"),
            link: None,
        }]),
        EventKind::Error { message } => Feed::NewCells(vec![Cell::Meta {
            style: theme::error(),
            text: format!("  ● {message}"),
            link: None,
        }]),
        // Audit-only engine events (P6-2 memory commits, P6-4 consent
        // grants, P8-B model switches) are provenance, not conversation —
        // never rendered. The SecurityCore audit kinds (PermissionDecision /
        // PolicyLoad / SandboxDenial) are log-only too — transcript stays clean.
        EventKind::MemoryUpdated { .. }
        | EventKind::ConsentGranted { .. }
        | EventKind::ModelSwitch { .. }
        | EventKind::PermissionDecision { .. }
        | EventKind::PolicyLoad { .. }
        | EventKind::SandboxDenial { .. } => Feed::Ignore,
        // A provider_error end follows an Error cell carrying the same
        // message — skip the summary so the failure lands as one line.
        EventKind::RunEnd { stop_reason, .. } if stop_reason == "provider_error" => Feed::Ignore,
        EventKind::RunEnd {
            stop_reason,
            steps,
            total_cost_usd,
        } => Feed::NewCells(vec![Cell::End {
            text: run_summary(*steps, run_elapsed, *total_cost_usd),
            warn: abnormal_stop(stop_reason),
        }]),
        EventKind::TurnEnd { .. } => Feed::Ignore,
    }
}

/// `19 steps · 1m 12s · $0.042` — segments append only when known.
/// DEFERRED(owner): `· NN% cached` once Agent::cache_stats() /
/// CacheStats::hit_rate lands on the run path (per AGENTS.md).
fn run_summary(steps: u32, elapsed: Option<std::time::Duration>, cost: f64) -> String {
    let mut s = format!("{steps} step{}", if steps == 1 { "" } else { "s" });
    if let Some(d) = elapsed {
        let secs = d.as_secs();
        let t = if secs >= 3600 {
            format!("{}h {:02}m", secs / 3600, secs % 3600 / 60)
        } else if secs >= 60 {
            format!("{}m {:02}s", secs / 60, secs % 60)
        } else {
            format!("{secs}s")
        };
        s.push_str(" · ");
        s.push_str(&t);
    }
    if cost >= 0.0005 {
        s.push_str(&format!(" · ${cost:.3}"));
    }
    s
}

/// Normal ends are silent about the reason; anything else names itself
/// (`max_tokens` → `max tokens`) so the summary explains the cutoff.
fn abnormal_stop(stop_reason: &str) -> Option<String> {
    match stop_reason {
        "end_turn" | "stop_sequence" => None,
        "max_tokens" => Some("max tokens".to_string()),
        other => Some(other.replace('_', " ")),
    }
}

fn plan_marker() -> Cell {
    Cell::Meta {
        style: theme::meta(),
        text: "  ↳ plan".to_string(),
        link: None,
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
            prev_hash: 0,
            hash: 0,
            kind,
        };
        match feed(
            &ev(EventKind::ToolCallStart {
                call_id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }),
            None,
        ) {
            Feed::NewCells(c) => assert!(matches!(c[0], Cell::Tool { .. })),
            _ => panic!(),
        }
        match feed(
            &ev(EventKind::ToolResult {
                call_id: "c1".into(),
                name: "bash".into(),
                content: "ok".into(),
                is_error: false,
                raw_bytes: 2,
                spilled_to: None,
                denied: false,
            }),
            None,
        ) {
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
