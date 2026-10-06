//! Frame rendering: cell flushing, live stack, draw_full — split
//! out of `app.rs` (W3).

use unicode_width::UnicodeWidthStr;

use super::*;

/// Wrap a draw/insert in synchronized-update markers when probed —
/// the terminal holds the frame until ESU → atomic flip, no flicker.
pub(crate) fn sync_wrap(
    caps: &Caps,
    f: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    if caps.sync_output {
        out.write_all(probe::BSU.as_bytes())?;
    }
    let r = f();
    if caps.sync_output {
        out.write_all(probe::ESU.as_bytes())?;
        out.flush()?;
    }
    r
}

impl App {
    /// Flush completed cells — Inline emits them to native scrollback
    /// via `insert_before`; Full appends their rendered lines to `tbuf`
    /// (the transcript lives inside the managed window there).
    pub(crate) fn flush_cells<B: ratatui::backend::Backend>(
        &mut self,
        term: &mut Terminal<B>,
        caps: &Caps,
    ) -> std::io::Result<()>
    where
        B::Error: std::fmt::Display,
    {
        if self.pending.is_empty() {
            return Ok(());
        }
        let width = term
            .size()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .width
            .max(1);
        if self.full() {
            self.tbuf_at(width);
            for cell in std::mem::take(&mut self.pending) {
                let mut lines = cell.lines(width);
                // §2: the prompt band's blank separator row is dropped
                // when the cell tops the transcript.
                if self.history.is_empty() && self.tbuf.is_empty() {
                    crate::cells::strip_top_gap(&cell, &mut lines);
                }
                self.tbuf.extend(lines);
                self.history.push(cell);
            }
            // DEFERRED(tui): OSC 8 link lines + OSC 133 prompt/output
            // marks can't ride a ratatui buffer — the transcript
            // overlay covers navigation; exit handoff is plain text.
            self.dirty = true;
            return Ok(());
        }
        for cell in std::mem::take(&mut self.pending) {
            self.history.push(cell.clone());
            let mut lines = cell.lines(width);
            if self.history.len() == 1 {
                crate::cells::strip_top_gap(&cell, &mut lines);
            }
            let h = lines.len() as u16;
            if h == 0 {
                continue;
            }
            // OSC 133 marks ride the scrollback stream: A/B bracket the
            // user prompt, C/D bracket finished tool output — terminal
            // "jump to prompt" and output-select features work on the
            // transcript. Emitted raw; the live region can't carry OSC.
            let (pre, post) = Self::osc133_marks(self.osc, &cell);
            sync_wrap(caps, || {
                if let Some(m) = pre {
                    let _ = std::io::stdout().write_all(m.as_bytes());
                }
                term.insert_before(h, |buf| {
                    for (y, line) in lines.iter().enumerate() {
                        buf.set_line(0, y as u16, line, width);
                    }
                })
                .map_err(|e| std::io::Error::other(e.to_string()))?;
                if let Some(m) = post {
                    let _ = std::io::stdout().write_all(m.as_bytes());
                }
                Ok(())
            })?;
            // OSC 8: a clickable file:// link line under the cell —
            // styled paths can't ride the ratatui buffer, so the link
            // is its own line.
            if self.osc {
                if let Some(p) = cell.link_path() {
                    let shown = display_path(&self.cwd, p);
                    let url = format!("file://{}", p.display());
                    let mut out = std::io::stdout();
                    let _ = out.write_all(
                        format!("  ⤷ {}\n", crate::notify::osc8(&url, &shown)).as_bytes(),
                    );
                    let _ = out.flush();
                }
            }
        }
        Ok(())
    }

    /// (pre, post) OSC 133 mark for a cell — None when OSC is off.
    pub(crate) fn osc133_marks(osc: bool, cell: &Cell) -> (Option<String>, Option<String>) {
        if !osc {
            return (None, None);
        }
        use crate::notify::osc133;
        match cell {
            Cell::User { .. } => (Some(osc133("A")), Some(osc133("B"))),
            Cell::Tool { status, .. } => {
                let code = match status {
                    ToolStatus::Ok => 0,
                    _ => 1,
                };
                (Some(osc133("C")), Some(osc133(&format!("D;{code}"))))
            }
            _ => (None, None),
        }
    }

    /// §3 composer placeholder: the empty, idle composer shows the
    /// faint hint after `❯`. Typing or a live run drops it.
    fn composer_frame(&self, width: u16) -> (Vec<Line<'static>>, (u16, u16)) {
        let (mut lines, cur) = self.composer.render(width);
        if self.composer.is_empty() && matches!(self.run, RunState::Idle) {
            if let Some(first) = lines.first_mut() {
                first.spans.push(Span::styled(
                    "ask anything  ·  ↑ panel  ·  ? keys",
                    crate::theme::faint(),
                ));
            }
        }
        (lines, cur)
    }

    /// True for a fresh session — nothing committed or in flight.
    /// Drives the §5 empty state (terminal block + web `e` flag).
    pub(crate) fn transcript_empty(&self) -> bool {
        self.history.is_empty() && self.live.is_empty() && self.pending.is_empty()
    }

    /// The §5 empty state is only on screen when the transcript is
    /// empty AND nothing sits above it — the web `e` flag uses this
    /// same guard so the client's mark can't overlay an open panel
    /// or dialog.
    pub(crate) fn empty_state_shown(&self) -> bool {
        self.transcript_empty() && self.overlay.is_none() && self.dialog.is_none()
    }

    pub(crate) fn live_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        // A modal overlay owns the whole live region — nothing else
        // competes for its rows. An open permission dialog outranks it
        // (the engine is blocked on that answer).
        if self.overlay_shown() {
            if let Some(o) = &self.overlay {
                // Full mode gives Panel its own bottom band — rendering
                // it here would double it inside the transcript region.
                if !(self.full() && matches!(o, Overlay::Panel { .. })) {
                    out.extend(self.overlay_lines(o, width));
                }
                if let Some((text, _)) = &self.toast {
                    out.push(Line::from(Span::styled(
                        format!("◆ {text}"),
                        crate::theme::meta(),
                    )));
                }
                return out;
            }
        }
        // Running tools (cap: newest few stay visible) — the live
        // pass animates their glyph with the spinner tick.
        for c in self.live.iter().rev().take(4).rev() {
            out.extend(c.lines_at(width, Some((self.tick, self.reduce_motion))));
        }
        if let Some((dlg, _)) = &self.dialog {
            out.extend(dlg.lines(width));
        }
        let queued: Vec<String> = match &self.run {
            RunState::Running { control, .. } => control.queued(),
            RunState::Idle => Vec::new(),
        };
        out.extend(widgets::queue_strip(&queued));
        out.extend(widgets::queue_strip(&self.pending_queue));
        // UI pass: the `/`/`@` suggestion strip is gone — Tab still
        // completes silently, `/help`/`?` carry discoverability.
        if let RunState::Running { started, phase, .. } = &self.run {
            out.push(widgets::indicator(
                phase,
                started.elapsed().as_secs(),
                self.tokens,
                self.tick,
                self.reduce_motion,
            ));
        }
        if let Some((text, _)) = &self.toast {
            out.push(Line::from(Span::styled(
                format!("◆ {text}"),
                crate::theme::meta(),
            )));
        }
        if self.show_plan {
            if let Ok(md) = std::fs::read_to_string(self.session_dir.join("plan.md")) {
                for l in md.lines().take(8) {
                    out.push(Line::from(Span::styled(
                        l.to_string(),
                        crate::theme::meta(),
                    )));
                }
            } else {
                out.push(Line::from(Span::styled(
                    "no plan yet".to_string(),
                    crate::theme::dim(),
                )));
            }
        }
        if self.show_help {
            out.extend(widgets::help_panel());
        }
        out
    }

    pub(crate) fn draw<B: ratatui::backend::Backend>(
        &mut self,
        term: &mut Terminal<B>,
        caps: &Caps,
    ) -> std::io::Result<()>
    where
        B::Error: std::fmt::Display,
    {
        if self.full() {
            return self.draw_full(term, caps);
        }
        let width = term
            .size()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .width
            .max(1);
        let (composer_lines, (cx, cy)) = self.composer_frame(width);
        let live = self.live_lines(width);
        let status = widgets::status_line(self.preset, &self.cwd, &self.model, self.cost, width);

        sync_wrap(caps, || {
            term.draw(|f| {
                // Layout is keyed off f.area() — the actual inline
                // viewport rect, not the terminal size.
                let area = f.area();
                let composer_rows = composer_lines.len().clamp(1, 4);
                let composer_clip = composer_lines.len().saturating_sub(composer_rows);
                let live_shown = (area.height as usize).saturating_sub(composer_rows + 1);
                let live_start = live.len().saturating_sub(live_shown);
                let composer_top = live_shown as u16;
                let cy_screen = composer_top
                    + cy.saturating_sub(composer_clip as u16)
                        .min(composer_rows as u16 - 1);

                let mut lines: Vec<Line<'static>> = Vec::with_capacity(area.height as usize);
                lines.extend(live[live_start..].iter().cloned());
                lines.extend(composer_lines[composer_clip..].iter().cloned());
                lines.push(status.clone());
                f.render_widget(Paragraph::new(lines), area);
                f.set_cursor_position((area.x + cx, area.y + cy_screen));
            })
            .map(|_| ())
            .map_err(|e: B::Error| std::io::Error::other(e.to_string()))
        })?;
        self.dirty = false;
        Ok(())
    }

    /// Full-window frame: the transcript owns every row except the
    /// bottom three — a 2-row prompt window hanging directly above the
    /// 1-row footer.
    ///
    /// Inside the transcript region the committed `tbuf` scrolls
    /// (`scroll` = lines up from its tail) while the live stack —
    /// running tools, dialog, overlay, queue, indicator, toast — stays
    /// pinned to the region's bottom rows, so a permission ask is
    /// visible even mid-scroll.
    ///
    /// DEFERRED(tui): `tbuf` rebuilds whole-cell →lines on resize and
    /// the frame slices a shared Vec — O(transcript) per frame.
    /// Cell-granularity caching belongs with an incremental renderer.
    pub(crate) fn draw_full<B: ratatui::backend::Backend>(
        &mut self,
        term: &mut Terminal<B>,
        caps: &Caps,
    ) -> std::io::Result<()>
    where
        B::Error: std::fmt::Display,
    {
        let size = term
            .size()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let width = size.width.max(1);
        self.tbuf_at(width);
        let (composer_lines, (cx, cy)) = self.composer_frame(width);
        let live = self.live_lines(width);

        // 2 prompt rows + 1 footer row are pinned at the bottom; the
        // transcript gets the rest. The live stack renders last inside
        // the transcript region (pinned) — overlays and the permission
        // dialog therefore never hide above the fold. An open control
        // panel takes a further 17-row band under the prompt (~306px),
        // which slides the whole window up.
        // Cap keeps the layout valid on short windows: transcript ≥2,
        // prompt 2, footer 1 — the band shrinks before rects overlap.
        let panel_open = matches!(self.overlay, Some(Overlay::Panel { .. }));
        let panel_h: u16 = if panel_open {
            17.min(size.height.saturating_sub(5))
        } else {
            0
        };
        let t_rows = size.height.saturating_sub(3 + panel_h) as usize;
        self.view_h = t_rows as u16;
        let live_shown = live.len().min(t_rows);
        let tbuf_visible = t_rows.saturating_sub(live_shown);
        // Dialogs/overlays force-follow the tail: they own the region.
        let pin = self.dialog.is_some() || self.overlay.is_some();
        let max_scroll = self.tbuf.len().saturating_sub(tbuf_visible);
        let scroll = if pin { 0 } else { self.scroll.min(max_scroll) };
        self.scroll = scroll; // clamped value stays honest for ↑N
        let end = self.tbuf.len().saturating_sub(scroll);
        let start = end.saturating_sub(tbuf_visible);

        // Footer: nothing but the functional `↑N` scroll marker —
        // badge/dir/model/cost are all off this surface (UI pass).
        let marker = if scroll > 0 {
            format!(" ↑{scroll}")
        } else {
            String::new()
        };
        let status = if marker.is_empty() {
            Line::default()
        } else {
            let pad = (width as usize).saturating_sub(marker.len() + 1).max(1);
            Line::from(vec![
                Span::raw(" ".repeat(pad)),
                Span::styled(marker, crate::theme::dim()),
            ])
        };
        // Bottom-anchored: blank rows precede a short transcript.
        let mut region: Vec<Line<'static>> =
            vec![Line::default(); tbuf_visible.saturating_sub(end - start)];
        region.extend(self.tbuf[start..end].iter().cloned());
        region.extend(live[live.len() - live_shown..].iter().cloned());

        // §5 empty state: a fresh session greets with the centred mark
        // over the wordmark, gone the moment the first cell lands.
        if self.empty_state_shown() {
            let mid = t_rows.saturating_sub(1) / 2;
            let center = |txt: &str, st| {
                let pad = (width as usize).saturating_sub(UnicodeWidthStr::width(txt)) / 2;
                Line::from(vec![
                    Span::raw(" ".repeat(pad)),
                    Span::styled(txt.to_string(), st),
                ])
            };
            // Web mode leaves the glyph row blank — the client
            // overlays the real SVG mark centred on it. The terminal
            // keeps `⋈`. The wordmark row is server-drawn in both.
            if caps.term_version.as_deref() != Some("overseer-web") {
                if let Some(l) = region.get_mut(mid) {
                    *l = center(crate::MARK_GLYPH, crate::theme::faint());
                }
            }
            if let Some(l) = region.get_mut(mid + 1) {
                *l = center("overseer", crate::theme::dim());
            }
        }

        // Composer clipped to 2 rows with the cursor kept visible.
        let c_rows = composer_lines.len().clamp(1, 2);
        let c_top = (cy as usize)
            .saturating_sub(c_rows - 1)
            .min(composer_lines.len() - 1);
        let cy_screen = (cy as usize).saturating_sub(c_top) as u16;

        // Click hit regions + the web client's prompt-row index —
        // computed from the same math the layout below uses.
        self.prompt_top = size.height.saturating_sub(3 + panel_h);
        self.panel_band = if panel_open {
            Some(ratatui::layout::Rect {
                x: 0,
                y: size.height.saturating_sub(1 + panel_h),
                width,
                height: panel_h,
            })
        } else {
            None
        };
        let overlay_len = if self.overlay_shown() && !panel_open && self.overlay.is_some() {
            live.len() - usize::from(self.toast.is_some())
        } else {
            0
        };
        let clip = live.len() - live_shown;
        self.overlay_block = if overlay_len > 0 {
            // Clip eats top lines first, so shown overlay lines shrink
            // by the clip and screen row j maps to overlay line clip+j.
            Some((tbuf_visible as u16, overlay_len.saturating_sub(clip), clip))
        } else {
            None
        };
        let band = if let Some(Overlay::Panel { tab, scroll }) = &self.overlay {
            self.panel_band_lines(*tab, *scroll, width, panel_h)
        } else {
            Vec::new()
        };

        sync_wrap(caps, || {
            term.draw(|f| {
                let area = f.area();
                let transcript = ratatui::layout::Rect {
                    height: area.height.saturating_sub(3 + panel_h),
                    ..area
                };
                let prompt = ratatui::layout::Rect {
                    y: area.y + area.height.saturating_sub(3 + panel_h),
                    height: 2.min(area.height),
                    ..area
                };
                let panel = ratatui::layout::Rect {
                    y: area.y + area.height.saturating_sub(1 + panel_h),
                    height: panel_h,
                    ..area
                };
                let footer = ratatui::layout::Rect {
                    y: area.y + area.height.saturating_sub(1),
                    height: 1,
                    ..area
                };
                f.render_widget(Paragraph::new(region), transcript);
                f.render_widget(Paragraph::new(composer_lines[c_top..].to_vec()), prompt);
                if panel_h > 0 {
                    f.render_widget(Paragraph::new(band.clone()), panel);
                }
                f.render_widget(Paragraph::new(vec![status.clone()]), footer);
                f.set_cursor_position((area.x + cx, prompt.y + cy_screen));
            })
            .map(|_| ())
            .map_err(|e: B::Error| std::io::Error::other(e.to_string()))
        })?;
        self.dirty = false;
        Ok(())
    }
}
