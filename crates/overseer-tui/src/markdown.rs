//! Streaming-tolerant markdown → ratatui lines (P2.2 component spec:
//! incremental parse, incomplete-fence tolerance). Deliberately small —
//! headers, fenced code, inline code, bold, lists — not a CommonMark
//! implementation. An unclosed fence renders as code to the end so a
//! partially-arrived block never flashes as raw text.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme;

/// Render a markdown-ish text block into styled lines.
pub fn render(text: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue; // the fence marker itself is never drawn
        }
        if in_fence {
            out.push(Line::from(Span::styled(
                format!("  {line}"),
                theme::CODE,
            )));
            continue;
        }
        out.push(render_line(line));
    }
    out
}

fn render_line(line: &str) -> Line<'static> {
    if let Some(rest) = line.strip_prefix("#") {
        let rest = rest.trim_start_matches('#').trim_start();
        return Line::from(Span::styled(
            rest.to_string(),
            Style::new().add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(inline_spans(line))
}

/// Inline spans: `code` and **bold** markers; everything else plain.
fn inline_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut chars = text.chars().peekable();
    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::raw(std::mem::take(buf)));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '`' => {
                flush(&mut buf, &mut spans);
                let mut code = String::new();
                let mut closed = false;
                for c2 in chars.by_ref() {
                    if c2 == '`' {
                        closed = true;
                        break;
                    }
                    code.push(c2);
                }
                if closed {
                    spans.push(Span::styled(code, theme::CODE));
                } else {
                    // Unclosed tick: keep literal, don't eat the text.
                    buf.push('`');
                    buf.push_str(&code);
                }
            }
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                flush(&mut buf, &mut spans);
                let mut bold = String::new();
                let mut closed = false;
                while let Some(c2) = chars.next() {
                    if c2 == '*' && chars.peek() == Some(&'*') {
                        chars.next();
                        closed = true;
                        break;
                    }
                    bold.push(c2);
                }
                if closed {
                    spans.push(Span::styled(
                        bold,
                        Style::new().add_modifier(Modifier::BOLD),
                    ));
                } else {
                    buf.push_str("**");
                    buf.push_str(&bold);
                }
            }
            _ => buf.push(c),
        }
    }
    flush(&mut buf, &mut spans);
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn unclosed_fence_stays_code() {
        let out = render("before\n```rust\nfn x() {}\nstill code");
        let text = plain(&out);
        assert!(text.contains("fn x() {}"));
        assert!(text.contains("still code"));
        assert!(!text.contains("```"));
    }

    #[test]
    fn inline_code_and_bold() {
        let out = render("run `cargo test` and **carefully**");
        assert_eq!(out.len(), 1);
        assert_eq!(plain(&out), "run cargo test and carefully");
    }

    #[test]
    fn header_bolded() {
        let out = render("## Plan");
        assert_eq!(plain(&out), "Plan");
        assert!(out[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
    }
}
