//! Checkpoint rewind (P1.9 core, P2.6 shared): restore snapshotted files
//! and/or truncate the event log at a checkpoint boundary. The CLI's
//! `overseer rewind` and the TUI's `/rewind` menu share this — one
//! implementation, one set of semantics.

use std::path::{Path, PathBuf};

use crate::event::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Restore files from the checkpoint manifest only.
    Code,
    /// Truncate events at the boundary only.
    Conversation,
    /// Files + truncation.
    Both,
    /// Truncation + append a Compaction marker (re-summarize the
    /// surviving log so the tail anchor stays honest). No file restore —
    /// preserved verbatim from the original CLI semantics.
    Summarize,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "code" => Some(Self::Code),
            "conversation" => Some(Self::Conversation),
            "both" => Some(Self::Both),
            "summarize" => Some(Self::Summarize),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub boundary: u64,
    /// Files restored from the checkpoint snapshot.
    pub restored: u32,
    /// Files the agent created that the rewind removed (existed:false).
    pub deleted: u32,
    /// Events dropped from the log.
    pub truncated: u32,
    /// Compaction marker anchor (Summarize mode only).
    pub compaction_at: Option<u64>,
}

/// Rewind `session_dir` to checkpoint `boundary` (None = latest).
/// `Code`/`Both` restore files from `checkpoints/e<boundary>/`;
/// `Conversation`/`Both`/`Summarize` truncate `events.jsonl` after the
/// boundary (the boundary's user input survives); `Summarize` then
/// appends a Compaction marker.
pub fn restore(session_dir: &Path, boundary: Option<u64>, mode: Mode) -> std::io::Result<Report> {
    let cps = crate::session::checkpoints(session_dir);
    let boundary = match boundary.or_else(|| cps.last().copied()) {
        Some(b) if cps.contains(&b) => b,
        Some(b) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no checkpoint e{b} in {}", session_dir.display()),
            ));
        }
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no checkpoints in {}", session_dir.display()),
            ));
        }
    };
    let mut report = Report {
        boundary,
        ..Default::default()
    };

    if matches!(mode, Mode::Code | Mode::Both) {
        restore_files(session_dir, boundary, &mut report)?;
    }

    if !matches!(mode, Mode::Code) {
        report.truncated = truncate_log(session_dir, boundary)?;
    }

    // Summarize = truncate + compaction marker (no file restore —
    // preserved verbatim from the original CLI semantics).
    if mode == Mode::Summarize {
        report.compaction_at = append_compaction(session_dir);
    }
    Ok(report)
}

/// Restore files from checkpoint `e<boundary>`. Every manifest entry is
/// validated BEFORE any fs change: its `path` must resolve (the way
/// `tools::snapshot` records it) under the session's workspace root, and
/// `stored` must be a bare file name inside the checkpoint. One bad entry
/// refuses the whole restore — a tampered manifest never writes or deletes
/// outside the workspace.
fn restore_files(session_dir: &Path, boundary: u64, report: &mut Report) -> std::io::Result<()> {
    let cp_dir = session_dir.join("checkpoints").join(format!("e{boundary}"));
    let Ok(manifest) = std::fs::read_to_string(cp_dir.join("manifest.jsonl")) else {
        return Ok(());
    };
    let mut entries = Vec::new();
    for line in manifest.lines() {
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
        entries.push((path.to_string(), stored.to_string(), existed));
    }
    if entries.is_empty() {
        return Ok(());
    }
    let root = workspace_root(session_dir)?;
    let refuse = |what: String| std::io::Error::new(std::io::ErrorKind::PermissionDenied, what);
    let mut plan = Vec::with_capacity(entries.len());
    for (path, stored, existed) in entries {
        let Some(dst) = resolve_in_workspace(&root, &path) else {
            return Err(refuse(format!(
                "rewind: manifest path `{path}` resolves outside the workspace {} — refusing to restore",
                root.display()
            )));
        };
        let bare = {
            let mut c = Path::new(&stored).components();
            matches!(
                (c.next(), c.next()),
                (Some(std::path::Component::Normal(_)), None)
            )
        };
        if existed && !bare {
            return Err(refuse(format!(
                "rewind: manifest snapshot name `{stored}` for `{path}` is not a bare file name — refusing to restore"
            )));
        }
        plan.push((dst, stored, existed));
    }
    let mut failed: Vec<String> = Vec::new();
    for (dst, stored, existed) in plan {
        if let Some(link) = symlinked_parent(&root, &dst) {
            failed.push(format!(
                "{} (parent {} is a symlink)",
                dst.display(),
                link.display()
            ));
            continue;
        }
        let is_link = std::fs::symlink_metadata(&dst).is_ok_and(|m| m.file_type().is_symlink());
        if existed {
            let src = cp_dir.join("files").join(&stored);
            if let Some(parent) = dst.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    failed.push(format!("{} ({e})", dst.display()));
                    continue;
                }
            }
            // Never write through a link: unlink it, then write a
            // regular file in its place.
            if is_link {
                if let Err(e) = std::fs::remove_file(&dst) {
                    failed.push(format!("{} (cannot unlink symlink: {e})", dst.display()));
                    continue;
                }
            }
            match std::fs::copy(&src, &dst) {
                Ok(_) => report.restored += 1,
                Err(e) => failed.push(format!("{} ({e})", dst.display())),
            }
        } else {
            match std::fs::remove_file(&dst) {
                Ok(()) => report.deleted += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => failed.push(format!("{} (cannot delete: {e})", dst.display())),
            }
        }
    }
    if !failed.is_empty() {
        return Err(std::io::Error::other(format!(
            "rewind: restored {} and deleted {}, but could not restore: {}",
            report.restored,
            report.deleted,
            failed.join(", ")
        )));
    }
    Ok(())
}

/// The first component strictly between `root` and `dst` that is a
/// symlink, if any — restore refuses paths routed through a link.
fn symlinked_parent(root: &Path, dst: &Path) -> Option<PathBuf> {
    let parent = dst.parent()?;
    parent
        .ancestors()
        .take_while(|a| *a != root && a.starts_with(root))
        .find(|a| std::fs::symlink_metadata(a).is_ok_and(|m| m.file_type().is_symlink()))
        .map(Path::to_path_buf)
}

/// The workspace root a session's manifest paths must stay under: the cwd
/// of the LAST `SessionStart` (a fork's own), canonicalized.
fn workspace_root(session_dir: &Path) -> std::io::Result<PathBuf> {
    use crate::event::{EventKind, EventLog};
    let events = EventLog::replay(session_dir.join("events.jsonl"))?;
    let cwd = events
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            EventKind::SessionStart { cwd, .. } if !cwd.is_empty() => Some(PathBuf::from(cwd)),
            _ => None,
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "rewind: no workspace cwd recorded in {} — refusing to restore files",
                    session_dir.join("events.jsonl").display()
                ),
            )
        })?;
    Ok(cwd.canonicalize().unwrap_or(cwd))
}

/// Resolve a manifest `path` the way `tools::snapshot` records it
/// (relative → anchored at the workspace, `.`/`..` folded), then resolve
/// symlinks through the deepest existing ancestor of its parent (the
/// final component stays as named). `Some` only when the
/// result lies under `root`; the returned path is the one fs ops use.
fn resolve_in_workspace(root: &Path, raw: &str) -> Option<PathBuf> {
    use std::path::Component;
    let p = Path::new(raw);
    let anchored = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    let mut norm = PathBuf::new();
    for c in anchored.components() {
        match c {
            Component::ParentDir => {
                if !norm.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => norm.push(other.as_os_str()),
        }
    }
    // The final component is never resolved: restore must act on the
    // path itself (a symlink there is replaced, not followed).
    let name = norm.file_name()?.to_os_string();
    let mut base = norm.parent()?;
    let mut rest = Vec::new();
    let mut full = loop {
        if let Ok(c) = base.canonicalize() {
            break c;
        }
        rest.push(base.file_name()?);
        base = base.parent()?;
    };
    for r in rest.iter().rev() {
        full.push(r);
    }
    full.push(name);
    full.starts_with(root).then_some(full)
}

fn truncate_log(session_dir: &Path, boundary: u64) -> std::io::Result<u32> {
    let events_path = session_dir.join("events.jsonl");
    let text = std::fs::read_to_string(&events_path)?;
    // Durable-tail rule, same as EventLog::replay: a corrupt NON-final
    // line refuses the truncate before any change — keeping it would
    // leave a log resume can never replay. The last non-empty line may
    // be a torn write and is tolerated.
    let lines: Vec<&str> = text.lines().collect();
    let last_nonempty = lines.iter().rposition(|l| !l.trim().is_empty());
    for (i, l) in lines.iter().enumerate() {
        if l.trim().is_empty() {
            continue;
        }
        if let Err(e) = serde_json::from_str::<Event>(l) {
            if Some(i) != last_nonempty {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "{}: corrupt event at line {}: {e}",
                        events_path.display(),
                        i + 1
                    ),
                ));
            }
        }
    }
    let kept: Vec<&str> = lines
        .iter()
        .filter(|l| {
            if l.trim().is_empty() {
                return true; // blank lines pass through verbatim
            }
            match serde_json::from_str::<Event>(l) {
                Ok(ev) => ev.id <= boundary,
                // Only the torn final line reaches here (middles were
                // refused above) — drop it like replay does, so a later
                // append (summarize's Compaction) can't strand a corrupt
                // line mid-file.
                Err(_) => false,
            }
        })
        .copied()
        .collect();
    let dropped = lines.len() - kept.len();
    let tmp = events_path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, kept.join("\n") + "\n")?;
    std::fs::rename(&tmp, &events_path)?;
    Ok(dropped as u32)
}

/// `--mode summarize`: after truncation, append a Compaction marker so the
/// surviving log has a view anchor (deterministic — derived mechanically
/// from raw events, per invariant 8).
fn append_compaction(session_dir: &Path) -> Option<u64> {
    use crate::compact;
    use crate::event::{EventKind, EventLog};
    let events_path = session_dir.join("events.jsonl");
    let events = EventLog::replay(&events_path).unwrap_or_default();
    let anchor = compact::tail_anchor(&events, compact::TAIL_TURNS, 0)?;
    let summary = compact::summarize(&events, anchor);
    if let Ok(mut log) = EventLog::open(&events_path) {
        let _ = log
            .append(EventKind::Compaction {
                summary,
                tail_from: anchor,
            })
            .and_then(|_| log.flush());
    }
    Some(anchor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventKind, EventLog};
    use crate::ir::{Block, Usage};

    fn tmpdir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("overseer-rewind-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Session with SessionStart + `turns` × (UserInput + ModelResponse):
    /// ids 1..=1+2*turns, user inputs on even ids.
    fn mk_session(root: &Path, turns: usize) -> PathBuf {
        let dir = root.join("s");
        std::fs::create_dir_all(&dir).unwrap();
        let mut log = EventLog::create(dir.join("events.jsonl")).unwrap();
        log.append(EventKind::SessionStart {
            session_id: "s".into(),
            cwd: root.display().to_string(),
            model: "m".into(),
            harness_version: "0".into(),
            parent: None,
        })
        .unwrap();
        for i in 0..turns {
            log.append(EventKind::UserInput {
                text: format!("prompt {i}"),
            })
            .unwrap();
            log.append(EventKind::ModelResponse {
                blocks: vec![Block::Text { text: "ok".into() }],
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
                latency_ms: 1,
                cost_usd: 0.0,
            })
            .unwrap();
        }
        log.flush().unwrap();
        dir
    }

    fn mk_checkpoint(dir: &Path, boundary: u64, path: &Path, existed: bool) {
        let cp = dir.join("checkpoints").join(format!("e{boundary}"));
        std::fs::create_dir_all(cp.join("files")).unwrap();
        if existed {
            std::fs::write(cp.join("files").join("f0"), "snapshot-content").unwrap();
        }
        std::fs::write(
            cp.join("manifest.jsonl"),
            format!(
                "{{\"path\":\"{}\",\"stored\":\"f0\",\"existed\":{existed}}}\n",
                path.display()
            ),
        )
        .unwrap();
    }

    fn ids(dir: &Path) -> Vec<u64> {
        EventLog::replay(dir.join("events.jsonl"))
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect()
    }

    #[test]
    fn conversation_truncates_after_boundary() {
        let root = tmpdir("conv");
        let dir = mk_session(&root, 3); // ids 1..7
        mk_checkpoint(&dir, 4, Path::new("/tmp/nope"), true);
        let r = restore(&dir, Some(4), Mode::Conversation).unwrap();
        assert_eq!(r.truncated, 3);
        assert_eq!(r.restored + r.deleted, 0, "no file work");
        assert_eq!(ids(&dir), vec![1, 2, 3, 4]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// S1-review: a corrupt NON-final line refuses the truncate before
    /// any change — keeping it would leave a log resume can't replay.
    #[test]
    fn truncate_refuses_corrupt_middle_line() {
        let root = tmpdir("corrupt");
        let dir = mk_session(&root, 3);
        mk_checkpoint(&dir, 2, Path::new("/tmp/nope"), true);
        let path = dir.join("events.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines.insert(3, "{\"id\":99,\"garbage");
        let mutated = lines.join("\n") + "\n";
        std::fs::write(&path, &mutated).unwrap();
        let e = restore(&dir, Some(2), Mode::Conversation).unwrap_err();
        assert!(
            e.to_string().contains("corrupt event at line 4"),
            "replay-style 1-based line error: {e}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            mutated,
            "refused before any change"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The durable tail is still tolerated: a torn final line drops out
    /// like replay, never refuses.
    #[test]
    fn truncate_drops_torn_final_line() {
        let root = tmpdir("torn");
        let dir = mk_session(&root, 3); // ids 1..7
        mk_checkpoint(&dir, 4, Path::new("/tmp/nope"), true);
        let path = dir.join("events.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{text}{{\"id\":99,\"partial\n")).unwrap();
        let r = restore(&dir, Some(4), Mode::Conversation).unwrap();
        // 3 events over the boundary + the torn tail = 4 lines dropped.
        assert_eq!(r.truncated, 4);
        assert_eq!(ids(&dir), vec![1, 2, 3, 4]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn code_restores_snapshot_and_removes_created() {
        let root = tmpdir("code");
        let dir = mk_session(&root, 2);
        let existing = root.join("existing.txt");
        let created = root.join("created.txt");
        mk_checkpoint(&dir, 2, &existing, true);
        // Manifests can't hold two files in one mk_checkpoint call —
        // append the second line manually.
        let manifest = dir.join("checkpoints/e2/manifest.jsonl");
        let mut m = std::fs::read_to_string(&manifest).unwrap();
        m.push_str(&format!(
            "{{\"path\":\"{}\",\"stored\":\"none\",\"existed\":false}}\n",
            created.display()
        ));
        std::fs::write(&manifest, m).unwrap();
        // Post-checkpoint state the rewind must undo.
        std::fs::write(&existing, "agent-broke-it").unwrap();
        std::fs::write(&created, "agent-made-this").unwrap();

        let r = restore(&dir, Some(2), Mode::Code).unwrap();
        assert_eq!(r.restored, 1);
        assert_eq!(r.deleted, 1);
        assert_eq!(
            std::fs::read_to_string(&existing).unwrap(),
            "snapshot-content"
        );
        assert!(!created.exists());
        assert_eq!(ids(&dir).len(), 5, "conversation untouched");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn both_restores_and_truncates() {
        let root = tmpdir("both");
        let dir = mk_session(&root, 3);
        let f = root.join("f.txt");
        mk_checkpoint(&dir, 2, &f, true);
        std::fs::write(&f, "changed").unwrap();
        let r = restore(&dir, Some(2), Mode::Both).unwrap();
        assert_eq!(r.restored, 1);
        assert_eq!(r.truncated, 5);
        assert_eq!(ids(&dir), vec![1, 2]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn summarize_appends_compaction_marker() {
        let root = tmpdir("sum");
        let dir = mk_session(&root, 4);
        // Boundary at the last prompt: the surviving log keeps 3
        // ModelResponses (ids 3,5,7) → tail_anchor finds a cut point.
        mk_checkpoint(&dir, 8, Path::new("/tmp/nope"), true);
        let r = restore(&dir, Some(8), Mode::Summarize).unwrap();
        assert!(r.compaction_at.is_some());
        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        assert!(matches!(
            events.last().unwrap().kind,
            EventKind::Compaction { .. }
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn summarize_without_tail_skips_marker() {
        let root = tmpdir("sumnone");
        let dir = mk_session(&root, 1); // only 1 ModelResponse → no anchor
        mk_checkpoint(&dir, 2, Path::new("/tmp/nope"), true);
        let r = restore(&dir, Some(2), Mode::Summarize).unwrap();
        assert_eq!(r.compaction_at, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_checkpoint_errors() {
        let root = tmpdir("miss");
        let dir = mk_session(&root, 2);
        assert!(restore(&dir, Some(9), Mode::Both).is_err());
        // And with no checkpoints at all, even None errors.
        assert!(restore(&dir, None, Mode::Both).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn none_boundary_picks_latest() {
        let root = tmpdir("latest");
        let dir = mk_session(&root, 3);
        mk_checkpoint(&dir, 2, Path::new("/tmp/a"), true);
        mk_checkpoint(&dir, 4, Path::new("/tmp/b"), true);
        let r = restore(&dir, None, Mode::Conversation).unwrap();
        assert_eq!(r.boundary, 4);
        assert_eq!(ids(&dir), vec![1, 2, 3, 4]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// C7: manifest paths that resolve outside the session's workspace
    /// (relative traversal or absolute) are refused before any fs change.
    #[test]
    fn manifest_paths_outside_workspace_are_refused() {
        let root = tmpdir("escape");
        let dir = mk_session(&root, 2); // workspace = root
        let outside_dir = root
            .parent()
            .unwrap()
            .join(format!("overseer-rewind-outside-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&outside_dir).unwrap();
        let victim = outside_dir.join("outside.txt");
        std::fs::write(&victim, "precious").unwrap();
        let rel = PathBuf::from("..")
            .join(outside_dir.file_name().unwrap())
            .join("outside.txt");

        // Relative traversal, existed:false → would delete the victim.
        mk_checkpoint(&dir, 2, &rel, false);
        let err = restore(&dir, Some(2), Mode::Both).unwrap_err();
        assert!(err.to_string().contains("outside.txt"), "{err}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        assert_eq!(ids(&dir).len(), 5, "refused rewind leaves the log alone");

        // Absolute path outside the root, existed:true → would overwrite.
        mk_checkpoint(&dir, 2, &victim, true);
        let err = restore(&dir, Some(2), Mode::Code).unwrap_err();
        assert!(err.to_string().contains("outside.txt"), "{err}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside_dir);
    }
}
