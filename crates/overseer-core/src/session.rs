//! Session enumeration (P2.6): the picker, `--continue`, and `--last` all
//! read the same summary view. Sessions are directories under
//! `~/.overseer/sessions/` containing `events.jsonl`; the `SessionStart`
//! event carries id/cwd/model so listing never trusts dirnames.

use std::path::{Path, PathBuf};

use crate::event::{EventKind, EventLog};

/// One session's summary for listing/preview.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub dir: PathBuf,
    /// `session_id` from SessionStart (dirname fallback for old logs).
    pub id: String,
    pub cwd: String,
    pub model: String,
    /// First event timestamp (ms); 0 if unknown.
    pub started_ms: u64,
    /// Last event timestamp (ms) — drives recency ordering.
    pub last_ms: u64,
    pub events: u64,
    /// First user-typed input — the picker's preview line.
    pub first_user: Option<String>,
    /// Checkpoint boundaries (`e<N>` ids) present on disk.
    pub checkpoints: Vec<u64>,
    /// Session this forked from (SessionStart.parent) — the `/tree`
    /// navigator's edges. None for roots and pre-fork logs.
    pub parent: Option<String>,
}

/// List sessions under `root` (e.g. `~/.overseer/sessions`), most recent
/// activity first. Tolerates torn/missing logs — a directory without a
/// readable events.jsonl is skipped, not fatal.
pub fn list(root: &Path) -> Vec<SessionInfo> {
    list_with_warnings(root).0
}

/// [`list`], plus one warning per session skipped because its
/// `events.jsonl` has a corrupt non-final line (the logs
/// `EventLog::replay` refuses to resume). A corrupt session never fails
/// the whole listing; frontends decide whether to print the warnings.
pub fn list_with_warnings(root: &Path) -> (Vec<SessionInfo>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let Ok(dirs) = std::fs::read_dir(root) else {
        return (out, warnings);
    };
    for entry in dirs.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        match summarize(&dir) {
            Ok(Some(info)) => out.push(info),
            Ok(None) => {}
            Err(w) => warnings.push(w),
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.last_ms));
    (out, warnings)
}

/// Sessions whose recorded cwd matches `cwd` (canonicalized comparison
/// on both sides, falling back to the raw path when canonicalize fails —
/// e.g. a workspace that no longer exists).
pub fn for_cwd(root: &Path, cwd: &Path) -> Vec<SessionInfo> {
    let want = canonical_or_raw(cwd);
    list(root)
        .into_iter()
        .filter(|s| canonical_or_raw(Path::new(&s.cwd)) == want)
        .collect()
}

fn canonical_or_raw(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// The `/tree` navigator's ordering: DFS over the fork forest —
/// (info, depth) pairs, parents before their children. Roots are
/// sessions with `parent: None` or a parent that isn't listed (a fork
/// whose origin was pruned still shows, as a root). Each sibling group
/// sorts by recency. Deterministic; a session appears exactly once.
pub fn tree(root: &Path) -> Vec<(SessionInfo, usize)> {
    let all = list(root);
    let known: std::collections::HashSet<String> = all.iter().map(|s| s.id.clone()).collect();
    let mut children: std::collections::HashMap<Option<String>, Vec<SessionInfo>> =
        std::collections::HashMap::new();
    for s in all {
        let key = match &s.parent {
            Some(p) if known.contains(p.as_str()) => Some(p.clone()),
            _ => None,
        };
        children.entry(key).or_default().push(s);
    }
    let mut out = Vec::new();
    let mut stack: Vec<SessionInfo> = children.remove(&None).unwrap_or_default();
    stack.sort_by_key(|s| s.last_ms); // pop() → most recent first
                                      // Depth-first: each pop pushes its children so they render right
                                      // under their parent. Depth tracked parallel to the stack.
    let mut stack: Vec<(SessionInfo, usize)> = stack.into_iter().map(|s| (s, 0)).collect();
    while let Some((info, depth)) = stack.pop() {
        let mut kids = children.remove(&Some(info.id.clone())).unwrap_or_default();
        kids.sort_by_key(|s| s.last_ms);
        stack.extend(kids.into_iter().map(|k| (k, depth + 1)));
        out.push((info, depth));
    }
    out
}

/// The most recently active session dir — `--last` (any cwd) or
/// `--continue` (scoped to cwd).
pub fn most_recent(root: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    let candidates = match cwd {
        Some(cwd) => for_cwd(root, cwd),
        None => list(root),
    };
    candidates.into_iter().next().map(|s| s.dir)
}

/// [`most_recent`] minus live sessions (F4): `--continue`/`--last` skip
/// a session whose `live.lock` is held — it is open in another overseer
/// process — and take the next newest free one.
pub fn most_recent_resumable(root: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    let candidates = match cwd {
        Some(cwd) => for_cwd(root, cwd),
        None => list(root),
    };
    candidates
        .into_iter()
        .map(|s| s.dir)
        .find(|d| !crate::live::held(d))
}

/// Checkpoint boundaries on disk for a session (`checkpoints/e<N>`).
pub fn checkpoints(session_dir: &Path) -> Vec<u64> {
    let cp_root = session_dir.join("checkpoints");
    let mut cps: Vec<u64> = std::fs::read_dir(&cp_root)
        .map(|d| {
            d.filter_map(|e| {
                e.ok()?
                    .file_name()
                    .to_string_lossy()
                    .strip_prefix('e')
                    .and_then(|n| n.parse().ok())
            })
            .collect()
        })
        .unwrap_or_default();
    cps.sort_unstable();
    cps
}

/// `Ok(None)`: no readable log (skipped silently, as before). `Err`: a
/// warning naming a corrupt non-final line.
fn summarize(dir: &Path) -> Result<Option<SessionInfo>, String> {
    let events_path = dir.join("events.jsonl");
    // Cap the parse work: sessions can grow large; the header + a tail
    // slice cover everything the picker needs.
    const CAP: u64 = 4 * 1024 * 1024;
    let Ok(meta) = std::fs::metadata(&events_path) else {
        return Ok(None);
    };
    if !meta.is_file() {
        return Ok(None);
    }
    let sliced = meta.len() > CAP;
    let text = if sliced {
        head_and_tail(&events_path)
    } else {
        std::fs::read_to_string(&events_path).ok()
    };
    let Some(text) = text else {
        return Ok(None);
    };
    let last_nonempty = text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, _)| i)
        .last();
    let mut info = SessionInfo {
        dir: dir.to_path_buf(),
        id: dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        cwd: String::new(),
        model: String::new(),
        started_ms: 0,
        last_ms: 0,
        events: 0,
        first_user: None,
        checkpoints: checkpoints(dir),
        parent: None,
    };
    for (i, line) in text.lines().enumerate() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            // Only a full read can prove a corrupt middle line — a sliced
            // read cuts lines at its seams; a torn tail is tolerated.
            if !sliced && !line.trim().is_empty() && Some(i) != last_nonempty {
                return Err(format!(
                    "skipping session {}: corrupt events.jsonl line {}",
                    dir.display(),
                    i + 1
                ));
            }
            continue;
        };
        info.events += 1;
        if let Some(ts) = v.get("ts_ms").and_then(|t| t.as_u64()) {
            if info.started_ms == 0 {
                info.started_ms = ts;
            }
            info.last_ms = ts;
        }
        match v.get("type").and_then(|t| t.as_str()) {
            Some("session_start") => {
                info.id = v
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or(&info.id)
                    .to_string();
                info.cwd = v
                    .get("cwd")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                info.model = v
                    .get("model")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                // The LAST SessionStart carries fork provenance —
                // earlier ones in a copied log are the parent's.
                if let Some(p) = v.get("parent").and_then(|p| p.as_str()) {
                    info.parent = Some(p.to_string());
                }
            }
            Some("user_input") if info.first_user.is_none() => {
                info.first_user = v
                    .get("text")
                    .and_then(|s| s.as_str())
                    .map(|s| s.chars().take(200).collect());
            }
            _ => {}
        }
    }
    if info.events == 0 {
        return Ok(None);
    }
    Ok(Some(info))
}

/// First 32 KiB + last 32 KiB of a large log — enough for SessionStart,
/// first user input, and the tail timestamp. Line-count is approximate
/// (deliberately — the picker never shows exact counts).
fn head_and_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    const SLICE: u64 = 32 * 1024;
    let mut text = String::new();
    let mut head = vec![0u8; SLICE.min(len) as usize];
    f.read_exact(&mut head).ok()?;
    text.push_str(&String::from_utf8_lossy(&head));
    if len > SLICE {
        f.seek(SeekFrom::End(-(SLICE as i64))).ok()?;
        let mut tail = Vec::new();
        f.read_to_end(&mut tail).ok()?;
        text.push_str(&String::from_utf8_lossy(&tail));
    }
    Some(text)
}

/// Fork a session at `at_event` (None = head) into `new_dir`: copies the
/// event log (truncated at the boundary when given) plus the checkpoints
/// that survive the cut. The fork gets a fresh ledger — prior spend stays
/// on the parent session's books.
pub fn fork(session_dir: &Path, at_event: Option<u64>, new_dir: &Path) -> std::io::Result<()> {
    let src = session_dir.join("events.jsonl");
    let text = std::fs::read_to_string(&src)?;
    let kept: Vec<&str> = match at_event {
        Some(boundary) => text
            .lines()
            .filter(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()
                    .and_then(|v| v.get("id").and_then(|id| id.as_u64()))
                    .map(|id| id <= boundary)
                    .unwrap_or(true)
            })
            .collect(),
        None => text.lines().collect(),
    };
    crate::harden::ensure_private_dir(new_dir)?;
    std::fs::write(new_dir.join("events.jsonl"), kept.join("\n") + "\n")?;
    // A fork is a NEW session — fresh ledger (spend starts at 0), and
    // resume's `Ledger::open` refuses a missing file outright.
    crate::ledger::Ledger::create(new_dir.join("ledger.jsonl"))?;

    // Checkpoints at or before the boundary come along so rewind still
    // works inside the fork.
    let cp_src = session_dir.join("checkpoints");
    let boundary = at_event.unwrap_or(u64::MAX);
    for e in checkpoints(session_dir) {
        if e > boundary {
            continue;
        }
        let dst = new_dir.join("checkpoints").join(format!("e{e}"));
        copy_dir(&cp_src.join(format!("e{e}")), &dst)?;
    }

    // A fork is a new session: append a fresh SessionStart (the parent's
    // stays in the copied history as provenance). The id comes from the
    // new dirname; cwd/model inherit the parent's last SessionStart.
    let events = EventLog::replay(new_dir.join("events.jsonl"))?;
    // The parent's identity comes from ITS LAST SessionStart — in a
    // previously-forked log, earlier starts belong to grandparents.
    let parent_start = events.iter().rev().find_map(|e| match &e.kind {
        EventKind::SessionStart {
            session_id,
            cwd,
            model,
            ..
        } => Some((session_id.clone(), cwd.clone(), model.clone())),
        _ => None,
    });
    let mut log = EventLog::open(new_dir.join("events.jsonl"))?;
    let (parent_id, cwd, model) =
        parent_start.unwrap_or_else(|| (String::new(), String::new(), String::new()));
    let id = new_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "fork".into());
    log.append(EventKind::SessionStart {
        session_id: id,
        cwd,
        model,
        harness_version: env!("CARGO_PKG_VERSION").to_string(),
        parent: if parent_id.is_empty() {
            None
        } else {
            Some(parent_id)
        },
    })?;
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let to = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Rehydrate-check used by tests.
#[cfg(test)]
pub fn event_count(session_dir: &Path) -> usize {
    EventLog::replay(session_dir.join("events.jsonl"))
        .map(|e| e.len())
        .unwrap_or(0)
}

/// The user-input text that opened checkpoint `e<N>` — the rewind menu's
/// row label.
pub fn checkpoint_label(session_dir: &Path, boundary: u64) -> Option<String> {
    let events = EventLog::replay(session_dir.join("events.jsonl")).ok()?;
    events
        .iter()
        .find(|e| e.id == boundary)
        .and_then(|e| match &e.kind {
            EventKind::UserInput { text } => Some(text.clone()),
            _ => None,
        })
        .map(|t| t.chars().take(80).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventKind, EventLog};
    use crate::ir::{Block, Usage};

    fn tmpdir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("overseer-session-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A session dir with SessionStart + alternating UserInput /
    /// ModelResponse events (ids 1, 2, 3, …).
    fn mk_session(root: &Path, name: &str, cwd: &str, inputs: &[&str]) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let mut log = EventLog::create(dir.join("events.jsonl")).unwrap();
        log.append(EventKind::SessionStart {
            session_id: name.into(),
            cwd: cwd.into(),
            model: "test-model".into(),
            harness_version: "0".into(),
            parent: None,
        })
        .unwrap();
        for text in inputs {
            log.append(EventKind::UserInput {
                text: text.to_string(),
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

    /// Checkpoint dir e<N> with a one-line manifest.
    fn mk_checkpoint(dir: &Path, boundary: u64, path: &Path, existed: bool, stored: &str) {
        let cp = dir.join("checkpoints").join(format!("e{boundary}"));
        std::fs::create_dir_all(cp.join("files")).unwrap();
        if existed {
            std::fs::write(cp.join("files").join(stored), "snapshot-content").unwrap();
        }
        std::fs::write(
            cp.join("manifest.jsonl"),
            format!(
                "{{\"path\":\"{}\",\"stored\":\"{stored}\",\"existed\":{existed}}}\n",
                path.display()
            ),
        )
        .unwrap();
    }

    #[test]
    fn list_reads_session_start_and_orders_by_recency() {
        let root = tmpdir("list");
        let a = mk_session(&root, "aaa", "/work/a", &["first prompt"]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = mk_session(&root, "bbb", "/work/b", &["second prompt"]);
        let rows = list(&root);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].dir, b, "newest activity first");
        assert_eq!(rows[1].dir, a);
        assert_eq!(rows[0].id, "bbb");
        assert_eq!(rows[0].cwd, "/work/b");
        assert_eq!(rows[0].model, "test-model");
        assert_eq!(rows[0].first_user.as_deref(), Some("second prompt"));
        assert_eq!(rows[0].events, 3); // start + input + response
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn most_recent_scopes_to_cwd() {
        let root = tmpdir("recent");
        let a = mk_session(&root, "aaa", "/work/a", &["x"]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let _b = mk_session(&root, "bbb", "/work/b", &["y"]);
        // Any cwd → the newest.
        assert_eq!(most_recent(&root, None).unwrap(), root.join("bbb"));
        // Scoped to /work/a → the older one wins by filter.
        assert_eq!(most_recent(&root, Some(Path::new("/work/a"))).unwrap(), a);
        assert!(most_recent(&root, Some(Path::new("/work/never"))).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fork_truncates_events_and_drops_late_checkpoints() {
        let root = tmpdir("fork");
        let src = mk_session(&root, "src", "/work", &["one", "two", "three"]);
        // ids: 1=start,2=input,3=resp,4=input,5=resp,6=input,7=resp
        let target = src.join("some.txt");
        mk_checkpoint(&src, 2, &target, true, "a");
        mk_checkpoint(&src, 6, &target, true, "b");
        let dst = root.join("forked");
        fork(&src, Some(4), &dst).unwrap();

        let events = EventLog::replay(dst.join("events.jsonl")).unwrap();
        let ids: Vec<u64> = events.iter().map(|e| e.id).collect();
        // Copied history stops at 4; the fork's own SessionStart is id 5.
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
        match &events[4].kind {
            EventKind::SessionStart {
                session_id, cwd, ..
            } => {
                assert_eq!(session_id, "forked");
                assert_eq!(cwd, "/work");
            }
            other => panic!("expected fresh SessionStart, got {other:?}"),
        }
        // e2 came along, e6 (past the boundary) did not.
        assert!(dst.join("checkpoints/e2/manifest.jsonl").exists());
        assert!(!dst.join("checkpoints/e6").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fork_head_copies_everything() {
        let root = tmpdir("forkhead");
        let src = mk_session(&root, "src", "/work", &["one"]);
        mk_checkpoint(&src, 2, Path::new("/tmp/x"), true, "a");
        let dst = root.join("forked");
        fork(&src, None, &dst).unwrap();
        assert!(dst.join("checkpoints/e2").exists());
        // 3 source events + fork's SessionStart.
        assert_eq!(event_count(&dst), 4);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkpoint_label_finds_the_prompt() {
        let root = tmpdir("label");
        let dir = mk_session(&root, "s", "/w", &["fix the flaky test"]);
        assert_eq!(
            checkpoint_label(&dir, 2).as_deref(),
            Some("fix the flaky test")
        );
        assert_eq!(checkpoint_label(&dir, 3), None, "e3 is a ModelResponse");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fork_records_parentage() {
        let root = tmpdir("parentage");
        let src = mk_session(&root, "src", "/work", &["one"]);
        let dst = root.join("child");
        fork(&src, None, &dst).unwrap();

        // The fork's own SessionStart names its parent.
        let events = EventLog::replay(dst.join("events.jsonl")).unwrap();
        match &events.last().unwrap().kind {
            EventKind::SessionStart {
                session_id, parent, ..
            } => {
                assert_eq!(session_id, "child");
                assert_eq!(parent.as_deref(), Some("src"));
            }
            other => panic!("expected SessionStart, got {other:?}"),
        }
        // And it survives the summary path.
        let info = list(&root).into_iter().find(|s| s.dir == dst).unwrap();
        assert_eq!(info.parent.as_deref(), Some("src"));
        // The source session stays a root.
        let src_info = list(&root).into_iter().find(|s| s.dir == src).unwrap();
        assert_eq!(src_info.parent, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn logs_without_parent_field_still_load() {
        // A pre-fork-era SessionStart line has no `parent` key at all.
        let root = tmpdir("legacy");
        let dir = root.join("old");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("events.jsonl"),
            "{\"id\":1,\"ts_ms\":1,\"type\":\"session_start\",\"session_id\":\"old\",\"cwd\":\"/w\",\"model\":\"m\",\"harness_version\":\"0\"}\n\
             {\"id\":2,\"ts_ms\":2,\"type\":\"user_input\",\"text\":\"hi\"}\n",
        )
        .unwrap();
        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        assert_eq!(events.len(), 2);
        match &events[0].kind {
            EventKind::SessionStart { parent, .. } => assert_eq!(parent, &None),
            other => panic!("expected SessionStart, got {other:?}"),
        }
        let info = list(&root).into_iter().next().unwrap();
        assert_eq!(info.parent, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tree_orders_parents_before_children() {
        let root = tmpdir("tree");
        let a = mk_session(&root, "root-a", "/w", &["a"]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = root.join("child-b");
        fork(&a, None, &b).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let c = root.join("grandchild-c");
        fork(&b, None, &c).unwrap();
        let d = mk_session(&root, "root-d", "/w", &["d"]);

        let t = tree(&root);
        let order: Vec<(&str, usize)> = t.iter().map(|(s, d)| (s.id.as_str(), *d)).collect();
        // DFS: each parent's subtree completes before the next root.
        let pos = |id: &str| order.iter().position(|(s, _)| *s == id).unwrap();
        assert!(pos("root-a") < pos("child-b"));
        assert!(pos("child-b") < pos("grandchild-c"));
        assert_eq!(order[pos("child-b")].1, 1);
        assert_eq!(order[pos("grandchild-c")].1, 2);
        assert_eq!(order[pos("root-d")].1, 0);
        assert_eq!(t.len(), 4, "every session exactly once");
        let _ = (a, b, c, d);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tree_treats_orphaned_parent_as_root() {
        // Fork a session, then delete the parent's log — the fork lists
        // as a root rather than disappearing.
        let root = tmpdir("orphan");
        let src = mk_session(&root, "gone", "/w", &["x"]);
        let dst = root.join("kid");
        fork(&src, None, &dst).unwrap();
        std::fs::remove_dir_all(&src).unwrap();
        let t = tree(&root);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0.id, "kid");
        assert_eq!(t[0].1, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// C11: a symlinked cwd resolves to the same session.
    #[cfg(unix)]
    #[test]
    fn for_cwd_matches_through_symlink() {
        let root = tmpdir("symlink");
        let real = root.join("real-ws");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("link-ws");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        mk_session(&sessions, "aaa", &real.display().to_string(), &["x"]);
        assert_eq!(for_cwd(&sessions, &link).len(), 1, "link → real");
        mk_session(&sessions, "bbb", &link.display().to_string(), &["y"]);
        assert_eq!(for_cwd(&sessions, &real).len(), 2, "real → both");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// C6 audit: a session with a corrupt middle line is skipped with a
    /// warning; the rest of the listing survives.
    #[test]
    fn corrupt_session_skipped_with_warning() {
        let root = tmpdir("corrupt");
        mk_session(&root, "good", "/work/a", &["x"]);
        let bad = mk_session(&root, "bad", "/work/b", &["y"]);
        let path = bad.join("events.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines.insert(1, "{garbage");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let (rows, warnings) = list_with_warnings(&root);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "good");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("line 2"), "{warnings:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
