//! Modal overlays (sessions/rewind/transcript/diff/tree) and their
//! line builders — split out of `app.rs` (W3).

use super::input::*;
use super::panel::PANEL_TABS;
use super::*;

pub(crate) fn overlay_sel(o: &mut Overlay) -> (&mut usize, usize) {
    match o {
        Overlay::Sessions {
            rows, sel, filter, ..
        } => (sel, filtered_sessions(rows, filter).len().saturating_sub(1)),
        Overlay::Rewind { rows, sel } => (sel, rows.len().saturating_sub(1)),
        Overlay::Tree { rows, sel } => (sel, rows.len().saturating_sub(1)),
        Overlay::Diff { rows, sel, .. } => (sel, rows.len().saturating_sub(1)),
        // Transcript/Panel scroll lines, not rows — the Up/Down arms in
        // on_overlay_key handle them before this runs. Reaching one
        // here means a new caller routed wrong; keep that loud.
        Overlay::Transcript { .. } | Overlay::Panel { .. } => {
            unreachable!("scroll overlays bypass overlay_sel")
        }
    }
}

/// Fuzzy filter: subsequence match over id + cwd + preview text.
/// Returns (row-index, session) pairs so parallel metadata (branches)
/// still lines up after filtering.
pub(crate) fn filtered_sessions<'a>(
    rows: &'a [overseer_core::session::SessionInfo],
    filter: &str,
) -> Vec<(usize, &'a overseer_core::session::SessionInfo)> {
    rows.iter()
        .enumerate()
        .filter(|(_, s)| {
            let hay = format!(
                "{} {} {}",
                s.id,
                s.cwd,
                s.first_user.as_deref().unwrap_or("")
            );
            subseq_match(&hay, filter)
        })
        .collect()
}

/// Current branch of a session's recorded cwd — `git branch
/// --show-current`, None outside a repo or on any failure. Computed
/// once per picker open, never per frame.
pub(crate) fn git_branch(cwd: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", cwd, "branch", "--show-current"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if b.is_empty() {
        None
    } else {
        Some(b)
    }
}

/// Filtered transcript lines (history + in-flight live cells), used by
/// both the overlay's height math and its render window.
pub(crate) fn transcript_lines(
    app: &App,
    query: &str,
    width: u16,
    expand_tools: bool,
) -> Vec<Line<'static>> {
    let q = query.to_lowercase();
    app.history
        .iter()
        .chain(app.live.iter())
        .filter(|c| q.is_empty() || c.plain().to_lowercase().contains(&q))
        .enumerate()
        .flat_map(|(i, c)| {
            let mut ls = if expand_tools {
                c.lines_expanded(width)
            } else {
                c.lines(width)
            };
            // The prompt band's separator row doesn't lead the overlay.
            if i == 0 {
                crate::cells::strip_top_gap(c, &mut ls);
            }
            ls
        })
        .collect()
}

/// A permission dialog's bash command, one `$ ` row per line.
pub(crate) fn command_lines(cmd: &str, query: &str, width: u16) -> Vec<Line<'static>> {
    let q = query.to_lowercase();
    cmd.lines()
        .filter(|l| q.is_empty() || l.to_lowercase().contains(&q))
        .flat_map(|l| {
            crate::cells::wrap_styled(
                vec![
                    Span::styled("$ ", crate::theme::dialog_key()),
                    Span::styled(l.to_string(), crate::theme::dialog()),
                ],
                width.max(8) as usize,
            )
        })
        .collect()
}

/// Files tracked by checkpoint manifests → `/diff` rows (earliest
/// snapshot vs current working-tree content).
pub(crate) fn collect_diff_rows(session_dir: &std::path::Path) -> Vec<DiffRow> {
    // First-seen manifest entry per path: (existed, stored, checkpoint dir).
    let mut seen: std::collections::HashMap<
        std::path::PathBuf,
        (bool, String, std::path::PathBuf),
    > = std::collections::HashMap::new();
    for b in overseer_core::session::checkpoints(session_dir) {
        let cp = session_dir.join("checkpoints").join(format!("e{b}"));
        let Ok(m) = std::fs::read_to_string(cp.join("manifest.jsonl")) else {
            continue;
        };
        for line in m.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let (Some(path), Some(stored)) = (
                v.get("path").and_then(|p| p.as_str()),
                v.get("stored").and_then(|s| s.as_str()),
            ) else {
                continue;
            };
            let existed = v.get("existed").and_then(|e| e.as_bool()).unwrap_or(false);
            seen.entry(std::path::PathBuf::from(path)).or_insert((
                existed,
                stored.to_string(),
                cp.clone(),
            ));
        }
    }
    const CAP: usize = 400; // LCS is O(n·m) — cap the compared prefix
    let mut rows = Vec::new();
    for (path, (existed, stored, cp)) in seen {
        let snapshot: Option<String> = if existed {
            std::fs::read_to_string(cp.join("files").join(&stored)).ok()
        } else {
            None
        };
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        let status = match (existed, std::path::Path::new(&path).exists()) {
            (false, true) => "created",
            (true, false) => "deleted",
            (_, true) => "modified",
            (false, false) => continue, // created then removed — no change
        };
        let old: String = snapshot.clone().unwrap_or_default();
        let hs = crate::diff::hunks(
            &old.lines().take(CAP).collect::<Vec<_>>().join("\n"),
            &current.lines().take(CAP).collect::<Vec<_>>().join("\n"),
            3,
        );
        if hs.is_empty() {
            continue;
        }
        let added = hs.iter().map(|h| h.added).sum();
        let deleted = hs.iter().map(|h| h.deleted).sum();
        let rejected = vec![false; hs.len()];
        rows.push(DiffRow {
            path,
            status,
            snapshot,
            hunks: hs,
            rejected,
            added,
            deleted,
        });
    }
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    rows
}

pub(crate) fn short_id(id: &str) -> String {
    // Char-safe: session ids can carry non-ASCII (byte slicing would
    // panic mid-char).
    if id.chars().count() > 12 {
        let tail: String = id.chars().skip(id.chars().count() - 10).collect();
        format!("…{tail}")
    } else {
        id.to_string()
    }
}

pub(crate) fn rel_time(ts_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let ago = now.saturating_sub(ts_ms) / 1000;
    if ago < 60 {
        format!("{ago}s")
    } else if ago < 3600 {
        format!("{}m", ago / 60)
    } else if ago < 86400 {
        format!("{}h", ago / 3600)
    } else {
        format!("{}d", ago / 86400)
    }
}

/// Files recorded in checkpoint e<N>'s manifest (the picker's count).
pub(crate) fn manifest_files(session_dir: &std::path::Path, boundary: u64) -> u32 {
    let p = session_dir
        .join("checkpoints")
        .join(format!("e{boundary}"))
        .join("manifest.jsonl");
    std::fs::read_to_string(p)
        .map(|m| m.lines().count() as u32)
        .unwrap_or(0)
}

impl App {
    pub(crate) fn overlay_confirm(&mut self) {
        match self.overlay.take() {
            Some(Overlay::Sessions {
                rows, sel, filter, ..
            }) => {
                if let Some((_, info)) = filtered_sessions(&rows, &filter).get(sel) {
                    let info = (*info).clone();
                    if info.dir != self.session_dir {
                        let _ = self.worker_tx.send(WorkerCmd::SwitchSession {
                            dir: info.dir.clone(),
                        });
                    }
                }
            }
            Some(Overlay::Rewind { rows, sel }) => {
                if let Some(row) = rows.get(sel) {
                    self.do_rewind(row.boundary);
                }
            }
            Some(Overlay::Tree { rows, sel }) => {
                if let Some((info, _)) = rows.get(sel) {
                    if info.dir != self.session_dir {
                        let _ = self.worker_tx.send(WorkerCmd::SwitchSession {
                            dir: info.dir.clone(),
                        });
                    }
                }
            }
            Some(Overlay::Diff { rows, sel, .. }) => {
                if let Some(row) = rows.into_iter().nth(sel) {
                    self.revert_file(&row);
                }
            }
            _ => {}
        }
    }

    /// `/sessions` (or Ctrl+P): the picker only opens between runs —
    /// switching mid-run would orphan its control/ask state.
    pub(crate) fn open_sessions(&mut self) {
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: "finish or interrupt the run first".into(),
                link: None,
            });
            return;
        }
        let Some(root) = self.session_dir.parent().map(|p| p.to_path_buf()) else {
            return;
        };
        let (mut rows, warnings) = overseer_core::session::list_with_warnings(&root);
        if !warnings.is_empty() {
            // Full reasons stay out of the UI — debug log under /tmp.
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(std::env::temp_dir().join("overseer-session-warnings.log"))
            {
                use std::io::Write;
                for w in &warnings {
                    let _ = writeln!(f, "{w}");
                }
            }
        }
        // Current-cwd sessions first (the picker is cwd-scoped by
        // convention, like --continue).
        let cwd = self.cwd.clone();
        rows.sort_by_key(|s| std::cmp::Reverse(s.cwd == cwd));
        let branches = rows.iter().map(|s| git_branch(&s.cwd)).collect();
        self.overlay = Some(Overlay::Sessions {
            rows,
            branches,
            skipped: warnings.len(),
            sel: 0,
            filter: String::new(),
            wide: false,
        });
    }

    /// `/tree`: the fork forest — sessions ordered parents-first.
    pub(crate) fn open_tree(&mut self) {
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: "finish or interrupt the run first".into(),
                link: None,
            });
            return;
        }
        let Some(root) = self.session_dir.parent().map(|p| p.to_path_buf()) else {
            return;
        };
        let rows = overseer_core::session::tree(&root);
        if rows.is_empty() {
            return;
        }
        // Preselect the current session so the user sees where they are.
        let sel = rows
            .iter()
            .position(|(s, _)| s.dir == self.session_dir)
            .unwrap_or(0);
        self.overlay = Some(Overlay::Tree { rows, sel });
    }

    /// `/rewind`: checkpoints of the current session with their labels.
    pub(crate) fn open_rewind(&mut self) {
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: "finish or interrupt the run first".into(),
                link: None,
            });
            return;
        }
        let rows: Vec<RewindRow> = overseer_core::session::checkpoints(&self.session_dir)
            .into_iter()
            .rev()
            .map(|b| RewindRow {
                boundary: b,
                files: manifest_files(&self.session_dir, b),
                label: overseer_core::session::checkpoint_label(&self.session_dir, b)
                    .unwrap_or_else(|| "(no prompt text)".into()),
            })
            .collect();
        if rows.is_empty() {
            self.pending.push(Cell::Meta {
                style: crate::theme::dim(),
                text: "no checkpoints yet — checkpoints open on each prompt".into(),
                link: None,
            });
            return;
        }
        self.overlay = Some(Overlay::Rewind { rows, sel: 0 });
    }

    pub(crate) fn do_rewind(&mut self, boundary: u64) {
        match overseer_core::rewind::restore(
            &self.session_dir,
            Some(boundary),
            overseer_core::rewind::Mode::Both,
        ) {
            Ok(rep) => {
                self.pending.push(Cell::Meta {
                    style: crate::theme::meta(),
                    text: format!(
                        "rewound to e{} — {} file(s) restored, {} removed, {} event(s) dropped",
                        rep.boundary, rep.restored, rep.deleted, rep.truncated
                    ),
                    link: None,
                });
                // The log changed — rebuild the agent's context too.
                let _ = self.worker_tx.send(WorkerCmd::SwitchSession {
                    dir: self.session_dir.clone(),
                });
            }
            Err(e) => self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: format!("rewind failed: {e}"),
                link: None,
            }),
        }
    }

    /// Ctrl+O / `/search`: pager over the whole transcript. The search
    /// query is a plain case-insensitive substring over cell text.
    pub(crate) fn open_transcript(&mut self) {
        self.overlay = Some(Overlay::Transcript {
            query: String::new(),
            scroll: usize::MAX, // clamped to the tail on first render
            expand_tools: false,
            command: None,
        });
    }

    /// Tab on a permission dialog: every line of the bash command in
    /// the transcript pager, from the top. The dialog stays pending.
    pub(crate) fn open_command_view(&mut self, cmd: String) {
        self.overlay = Some(Overlay::Transcript {
            query: String::new(),
            scroll: 0,
            expand_tools: false,
            command: Some(cmd),
        });
    }

    /// The overlay owns the live region unless a dialog waits — then
    /// only the dialog's own command view may cover it.
    pub(crate) fn overlay_shown(&self) -> bool {
        self.dialog.is_none()
            || matches!(
                self.overlay,
                Some(Overlay::Transcript {
                    command: Some(_),
                    ..
                })
            )
    }

    /// `/diff`: every path a checkpoint manifest records, earliest
    /// snapshot vs the working tree. Bash side effects stay invisible —
    /// the same blind spot rewind documents.
    pub(crate) fn open_diff(&mut self) {
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: "finish or interrupt the run first".into(),
                link: None,
            });
            return;
        }
        let rows = collect_diff_rows(&self.session_dir);
        if rows.is_empty() {
            self.pending.push(Cell::Meta {
                style: crate::theme::dim(),
                text: "no tracked changes — checkpoints record write/edit only".into(),
                link: None,
            });
            return;
        }
        self.overlay = Some(Overlay::Diff {
            rows,
            sel: 0,
            preview: false,
            hunk_sel: 0,
        });
    }

    /// Revert one `/diff` row: marked hunks (rejects) restore just their
    /// old lines; no marks = whole file back to its earliest snapshot.
    /// Current content is stashed under checkpoints/revert-stash first —
    /// a revert is itself recoverable.
    pub(crate) fn revert_file(&mut self, row: &DiffRow) {
        if let Ok(cur) = std::fs::read_to_string(&row.path) {
            let stash = self
                .session_dir
                .join("checkpoints/revert-stash")
                .join(row.path.file_name().unwrap_or_default());
            if let Some(p) = stash.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            let _ = std::fs::write(&stash, cur);
        }
        let nrej = row.rejected.iter().filter(|x| **x).count();
        // Partial revert needs the file to exist now; a "created" row's
        // reject-all collapses to the full revert (delete) anyway.
        let partial = nrej > 0 && row.snapshot.is_some();
        let ok = if partial {
            match std::fs::read_to_string(&row.path) {
                Ok(cur) => {
                    let out = crate::diff::apply_rejects(&cur, &row.hunks, &row.rejected);
                    std::fs::write(&row.path, out).is_ok()
                }
                Err(_) => false,
            }
        } else {
            match &row.snapshot {
                Some(snap) => {
                    if let Some(p) = row.path.parent() {
                        let _ = std::fs::create_dir_all(p);
                    }
                    std::fs::write(&row.path, snap).is_ok()
                }
                None => std::fs::remove_file(&row.path).is_ok(),
            }
        };
        // Basename only — the diff row above already carries the path.
        let shown = row
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| display_path(&self.cwd, &row.path));
        if ok {
            let what = if partial {
                format!("rejected {nrej} hunk(s) in {shown}")
            } else {
                format!("reverted {shown}")
            };
            self.set_toast(format!("{what} (stash in revert-stash)"));
        } else {
            self.set_toast(format!("revert failed for {shown}"));
        }
    }

    /// `/approve`: leave plan mode and tell the agent to implement.
    pub(crate) fn approve_plan(&mut self) {
        if self.preset != Preset::Plan {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: "not in plan mode (shift+tab to cycle)".into(),
                link: None,
            });
            return;
        }
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: "finish or interrupt the run first".into(),
                link: None,
            });
            return;
        }
        if !self.session_dir.join("plan.md").exists() {
            self.pending.push(Cell::Meta {
                style: crate::theme::dim(),
                text: "no plan yet — ask the agent for one first".into(),
                link: None,
            });
            return;
        }
        self.preset = Preset::WorkspaceWrite;
        let _ = self.worker_tx.send(WorkerCmd::SetPreset(self.preset));
        self.pending.push(Cell::Meta {
            style: crate::theme::meta(),
            text: "plan approved — switching to workspace mode".into(),
            link: None,
        });
        self.submit("The plan is approved — implement it.".to_string());
    }

    /// `/fork`: branch the session at head into a sibling dir and switch.
    pub(crate) fn do_fork(&mut self) {
        if self.running() {
            self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: "finish or interrupt the run first".into(),
                link: None,
            });
            return;
        }
        let Some(root) = self.session_dir.parent().map(|p| p.to_path_buf()) else {
            return;
        };
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let new_dir = root.join(format!("{ts}"));
        match overseer_core::session::fork(&self.session_dir, None, &new_dir) {
            Ok(()) => {
                self.pending.push(Cell::Meta {
                    style: crate::theme::meta(),
                    text: format!("forked → {}", new_dir.display()),
                    link: None,
                });
                let _ = self
                    .worker_tx
                    .send(WorkerCmd::SwitchSession { dir: new_dir });
            }
            Err(e) => self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: format!("fork failed: {e}"),
                link: None,
            }),
        }
    }

    pub(crate) fn overlay_lines(&self, o: &Overlay, width: u16) -> Vec<Line<'static>> {
        use crate::theme;
        match o {
            Overlay::Sessions {
                rows,
                branches,
                skipped,
                sel,
                filter,
                wide,
            } => {
                // Hint goes LAST — the region clips from the top, so the
                // key hints survive regardless of viewport height.
                let mut out = vec![Line::from(vec![
                    Span::styled("> ", theme::dialog_key()),
                    Span::styled(format!("{filter}▌"), theme::dialog()),
                ])];
                let shown = filtered_sessions(rows, filter);
                if shown.is_empty() {
                    out.push(Line::from(Span::styled(
                        "  no matching sessions".to_string(),
                        theme::dim(),
                    )));
                }
                for (i, (idx, s)) in shown.iter().take(6).enumerate() {
                    let cur = i == *sel;
                    let mark = if cur { "›" } else { " " };
                    let when = rel_time(s.last_ms);
                    // ⤶ marks a fork (SessionStart.parent set).
                    let fork = if s.parent.is_some() { "⤶ " } else { "" };
                    let head = format!(
                        "{mark} {when} · {}{} · {}",
                        fork,
                        short_id(&s.id),
                        s.first_user.as_deref().unwrap_or("(no prompt)")
                    );
                    out.push(Line::from(Span::styled(
                        head,
                        if cur {
                            theme::dialog_sel()
                        } else {
                            theme::dialog()
                        },
                    )));
                    if *wide {
                        let branch = branches.get(*idx).and_then(|b| b.as_deref()).unwrap_or("-");
                        out.push(Line::from(Span::styled(
                            format!(
                                "    {} · {} · ⎇ {} · {} events · {} checkpoint(s)",
                                s.cwd,
                                s.model,
                                branch,
                                s.events,
                                s.checkpoints.len()
                            ),
                            theme::dim(),
                        )));
                    }
                }
                if *skipped > 0 {
                    out.push(Line::from(Span::styled(
                        format!("{skipped} session(s) skipped — unreadable log"),
                        theme::faint(),
                    )));
                }
                out.push(Line::from(Span::styled(
                    "type to filter · ↑↓ · tab preview · enter switch · esc",
                    theme::dim(),
                )));
                out
            }
            Overlay::Tree { rows, sel } => {
                let mut out = Vec::new();
                for (i, (s, depth)) in rows.iter().take(6).enumerate() {
                    let cur = i == *sel;
                    let mark = if cur { "›" } else { " " };
                    let indent = "  ".repeat((*depth).min(4));
                    let fork_mark = if *depth > 0 { "↳ " } else { "" };
                    let here = if s.dir == self.session_dir {
                        " · (current)"
                    } else {
                        ""
                    };
                    out.push(Line::from(Span::styled(
                        format!(
                            "{mark} {indent}{fork_mark}{} · {} · {}{}",
                            short_id(&s.id),
                            rel_time(s.last_ms),
                            s.first_user.as_deref().unwrap_or("(no prompt)"),
                            here
                        ),
                        if cur {
                            theme::dialog_sel()
                        } else {
                            theme::dialog()
                        },
                    )));
                }
                out.push(Line::from(Span::styled(
                    "↑↓ · enter switch · esc",
                    theme::dim(),
                )));
                out
            }
            Overlay::Rewind { rows, sel } => {
                let mut out: Vec<Line<'static>> = rows
                    .iter()
                    .take(6)
                    .enumerate()
                    .map(|(i, r)| {
                        let cur = i == *sel;
                        Line::from(Span::styled(
                            format!(
                                "{} e{} · {} file(s) · {}",
                                if cur { "›" } else { " " },
                                r.boundary,
                                r.files,
                                r.label
                            ),
                            if cur {
                                theme::dialog_sel()
                            } else {
                                theme::dialog()
                            },
                        ))
                    })
                    .collect();
                out.push(Line::from(Span::styled(
                    "↑↓ · enter restore files+conversation · esc",
                    theme::dim(),
                )));
                out
            }
            Overlay::Transcript {
                query,
                scroll,
                expand_tools,
                command,
            } => {
                let (all, win) = match command {
                    Some(cmd) => (
                        command_lines(cmd, query, width),
                        (self.view_h as usize).saturating_sub(2).max(4),
                    ),
                    None => (transcript_lines(self, query, width, *expand_tools), 4),
                };
                let n = all.len();
                let mut out = vec![Line::from(vec![
                    Span::styled("/ ", theme::dialog_key()),
                    Span::styled(format!("{query}▌"), theme::dialog()),
                ])];
                // Scroll is a line offset; usize::MAX (fresh open) means tail.
                let max = n.saturating_sub(win);
                let start = (*scroll).min(max);
                out.extend(all.into_iter().skip(start).take(win));
                let hint = if command.is_some() {
                    format!("{n} lines · type to filter · ↑↓/pgdn · esc back to the prompt")
                } else {
                    format!("{n} lines · type to filter · ↑↓/pgdn · tab expand tools · esc")
                };
                out.push(Line::from(Span::styled(hint, theme::dim())));
                out
            }
            Overlay::Diff {
                rows,
                sel,
                preview,
                hunk_sel,
            } => {
                let mut out = Vec::new();
                for (i, r) in rows.iter().take(4).enumerate() {
                    let cur = i == *sel;
                    let mark = if cur { "›" } else { " " };
                    let nrej = r.rejected.iter().filter(|x| **x).count();
                    let rej = if nrej > 0 {
                        format!(" · {nrej} marked reject")
                    } else {
                        String::new()
                    };
                    out.push(Line::from(Span::styled(
                        format!(
                            "{mark} {} +{} -{} {}{}",
                            r.status,
                            r.added,
                            r.deleted,
                            display_path(&self.cwd, &r.path),
                            rej
                        ),
                        if cur {
                            theme::dialog_sel()
                        } else {
                            theme::dialog()
                        },
                    )));
                    if *preview && cur {
                        for (hi, h) in r.hunks.iter().take(3).enumerate() {
                            let hcur = hi == *hunk_sel;
                            out.push(Line::from(Span::styled(
                                format!(
                                    "  {} {} hunk {} (+{} -{})",
                                    if hcur { "›" } else { " " },
                                    if r.rejected[hi] { "[x]" } else { "[ ]" },
                                    hi + 1,
                                    h.added,
                                    h.deleted
                                ),
                                if hcur {
                                    theme::dialog_sel()
                                } else {
                                    theme::dialog()
                                },
                            )));
                            if hcur {
                                for l in h.lines.iter().skip(1).take(4) {
                                    let style = if l.starts_with('-') {
                                        theme::error()
                                    } else if l.starts_with('+') {
                                        theme::meta()
                                    } else {
                                        theme::dim()
                                    };
                                    out.push(Line::from(Span::styled(format!("    {l}"), style)));
                                }
                            }
                        }
                        if r.hunks.len() > 3 {
                            out.push(Line::from(Span::styled(
                                format!("    … {} more hunk(s)", r.hunks.len() - 3),
                                theme::dim(),
                            )));
                        }
                    }
                }
                out.push(Line::from(Span::styled(
                    if *preview {
                        "↑↓ file · ←→ hunk · space reject · enter revert · esc"
                    } else {
                        "↑↓ file · tab diff · enter revert · esc"
                    },
                    theme::dim(),
                )));
                out
            }
            Overlay::Panel { tab, scroll } => {
                let mut out = Vec::new();
                let mut strip = vec![Span::styled(" panel ", theme::meta())];
                for (i, name) in PANEL_TABS.iter().enumerate() {
                    let st = if i == *tab {
                        theme::dialog_sel()
                    } else {
                        theme::dim()
                    };
                    strip.push(Span::styled(format!(" {name} "), st));
                }
                out.push(Line::from(strip));
                let rows = self.panel_rows(*tab);
                // The scroll offset can't see the row count — clamp here.
                let start = (*scroll).min(rows.len().saturating_sub(1));
                out.extend(rows.into_iter().skip(start).take(10));
                out.push(Line::from(Span::styled(
                    "←/→/tab switch · ↑/↓ scroll · esc close",
                    theme::dim(),
                )));
                out
            }
        }
    }
}
