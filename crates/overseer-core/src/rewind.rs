//! Checkpoint rewind (P1.9 core, P2.6 shared): restore snapshotted files
//! and/or truncate the event log at a checkpoint boundary. The CLI's
//! `overseer rewind` and the TUI's `/rewind` menu share this — one
//! implementation, one set of semantics.

use std::path::{Path, PathBuf};

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
pub fn restore(
    session_dir: &Path,
    boundary: Option<u64>,
    mode: Mode,
) -> std::io::Result<Report> {
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
        restore_files(session_dir, boundary, &mut report);
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

fn restore_files(session_dir: &Path, boundary: u64, report: &mut Report) {
    let cp_dir = session_dir.join("checkpoints").join(format!("e{boundary}"));
    let Ok(manifest) = std::fs::read_to_string(cp_dir.join("manifest.jsonl")) else {
        return;
    };
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
        if v.get("existed").and_then(|e| e.as_bool()).unwrap_or(false) {
            let src = cp_dir.join("files").join(stored);
            let dst = PathBuf::from(path);
            if let Some(parent) = dst.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::copy(&src, &dst).is_ok() {
                report.restored += 1;
            }
        } else if std::fs::remove_file(path).is_ok() {
            report.deleted += 1;
        }
    }
}

fn truncate_log(session_dir: &Path, boundary: u64) -> std::io::Result<u32> {
    let events_path = session_dir.join("events.jsonl");
    let text = std::fs::read_to_string(&events_path)?;
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .ok()
                .and_then(|v| v.get("id").and_then(|id| id.as_u64()))
                .map(|id| id <= boundary)
                .unwrap_or(true)
        })
        .collect();
    let dropped = text.lines().count() - kept.len();
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
        let d = std::env::temp_dir().join(format!("overseer-rewind-{tag}-{}", uuid::Uuid::now_v7()));
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
            cwd: "/work".into(),
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
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "snapshot-content");
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
}
