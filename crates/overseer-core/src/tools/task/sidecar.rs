//! `task.json`: the per-dir record every subagent dir carries. Written
//! atomically at spawn, updated at finish; `resume`, `verify target`, the
//! in-flight count, the drain and settlement all read it — nothing
//! guesses paths.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use super::route::TaskMode;
use crate::profile::Tier;

pub const FILE: &str = "task.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Done,
    /// Its process exited while it ran (threads die with their process).
    Dead,
    /// Stopped by `task action=cancel` or a parent interrupt.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sidecar {
    /// The dir name: `task-N`, or `task-N-r1` for an escalation attempt.
    pub id: String,
    pub mode: TaskMode,
    pub tier: Tier,
    pub model: String,
    pub background: bool,
    /// Where it runs when not the parent's cwd (a writer's worktree, or
    /// the worktree a targeted verifier reviews).
    pub worktree: Option<PathBuf>,
    pub branch: Option<String>,
    /// Commit the worktree branched from — what `verify target` diffs
    /// against.
    #[serde(default)]
    pub base: Option<String>,
    pub cap_usd: f64,
    pub process_nonce: String,
    pub state: State,
    /// This dir's own ledger total — what the parent settles to.
    pub cost_usd: f64,
    /// Runs so far (a resume adds one); names the run's done marker.
    #[serde(default = "one")]
    pub run: u32,
    /// The escalation attempt that holds the final log, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalated_to: Option<String>,
}

fn one() -> u32 {
    1
}

/// Random per process: a `running` sidecar with another nonce belongs to
/// a process that is gone.
pub fn process_nonce() -> &'static str {
    static NONCE: OnceLock<String> = OnceLock::new();
    NONCE.get_or_init(|| uuid::Uuid::now_v7().simple().to_string())
}

/// Write a marker file atomically — temp file in the same dir, then
/// rename — so a reader that polls for it sees either no file or the
/// whole content, never a torn write.
pub fn write_marker(path: &Path, text: &str) -> std::io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "marker".into());
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::now_v7().simple()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// The background notice file for run `run` of a task.
pub fn done_marker(run: u32) -> String {
    if run <= 1 {
        "done.txt".into()
    } else {
        format!("done-{run}.txt")
    }
}

/// `task-N` exactly (not attempts, worktrees or memory views).
pub fn task_seq(name: &str) -> Option<u64> {
    let n = name.strip_prefix("task-")?;
    n.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| n.parse().ok())?
}

/// Highest `task-N` under `subagents_dir` — seeds the session counter.
pub fn max_seq(subagents_dir: &Path) -> u64 {
    std::fs::read_dir(subagents_dir)
        .map(|d| {
            d.flatten()
                .filter_map(|e| task_seq(&e.file_name().to_string_lossy()))
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// Own ledger total of a subagent dir (includes its own settlements).
pub fn ledger_total(dir: &Path) -> f64 {
    let rows = crate::ledger::Ledger::read_all(dir.join("ledger.jsonl"));
    crate::ledger::Ledger::summarize(&rows).total_cost_usd
}

/// Finished records this process could not store (dir → record): what
/// `load` returns for that dir, so a failed write never leaves a
/// live-looking `running` sidecar behind in this process.
fn unstored() -> std::sync::MutexGuard<'static, BTreeMap<PathBuf, Sidecar>> {
    static UNSTORED: OnceLock<Mutex<BTreeMap<PathBuf, Sidecar>>> = OnceLock::new();
    UNSTORED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Task dirs whose background thread has not written its done marker
/// yet: a `done` sidecar there without a marker is still delivering, not
/// a lost digest.
fn delivering_set() -> std::sync::MutexGuard<'static, std::collections::BTreeSet<PathBuf>> {
    static SET: OnceLock<Mutex<std::collections::BTreeSet<PathBuf>>> = OnceLock::new();
    SET.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Held by a background thread from spawn until its marker is written.
pub struct Delivery(PathBuf);

impl Delivery {
    pub fn new(dir: &Path) -> Self {
        delivering_set().insert(dir.to_path_buf());
        Delivery(dir.to_path_buf())
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        delivering_set().remove(&self.0);
    }
}

pub fn delivering(dir: &Path) -> bool {
    delivering_set().contains(dir)
}

impl Sidecar {
    pub fn load(dir: &Path) -> Option<Self> {
        if let Some(sc) = unstored().get(dir) {
            return Some(sc.clone());
        }
        serde_json::from_str(&std::fs::read_to_string(dir.join(FILE)).ok()?).ok()
    }

    /// Write-then-rename, so a reader never sees a torn record.
    pub fn store(&self, dir: &Path) -> std::io::Result<()> {
        let tmp = dir.join(format!("{FILE}.tmp"));
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(tmp, dir.join(FILE))?;
        unstored().remove(dir);
        Ok(())
    }

    /// Running in this process.
    pub fn is_live(&self) -> bool {
        self.state == State::Running && self.process_nonce == process_nonce()
    }

    /// Mark finished with `dir`'s ledger total. When the store fails the
    /// finished record is still what this process loads for `dir`.
    pub fn finish(&mut self, dir: &Path, state: State) -> std::io::Result<()> {
        self.state = state;
        self.cost_usd = ledger_total(dir);
        self.store(dir).inspect_err(|_| {
            unstored().insert(dir.to_path_buf(), self.clone());
        })
    }
}

/// Sort key: `task-N` (and its `task-N-r1` attempt) by N, then name.
fn seq_key(path: &Path) -> (u64, String) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let n = name
        .strip_prefix("task-")
        .map(|r| r.split('-').next().unwrap_or(r))
        .and_then(|d| d.parse().ok())
        .unwrap_or(u64::MAX);
    (n, name)
}

/// Every sidecar under `subagents_dir`, in task order (`task-2` before
/// `task-10`).
pub fn all(subagents_dir: &Path) -> Vec<(PathBuf, Sidecar)> {
    let mut v: Vec<(PathBuf, Sidecar)> = std::fs::read_dir(subagents_dir)
        .map(|d| {
            d.flatten()
                .filter_map(|e| Some((e.path(), Sidecar::load(&e.path())?)))
                .collect()
        })
        .unwrap_or_default();
    v.sort_by_cached_key(|(p, _)| seq_key(p));
    v
}

/// Mark every `running` sidecar owned by another process `dead`, with
/// whatever its ledger shows; a background task also gets its done
/// marker so the next drain notices it and its slot frees.
/// `<session>/live.lock` (live.rs) enforces the single-process-session
/// assumption this relies on: a second overseer process can't open the
/// session dir, so no foreign process' live tasks are reaped here and
/// no spend is double-settled.
pub fn reap_dead(subagents_dir: &Path) {
    for (dir, mut sc) in all(subagents_dir) {
        if sc.state != State::Running || sc.is_live() {
            continue;
        }
        let marker = dir.join(done_marker(sc.run));
        if sc.background && !marker.exists() {
            let _ = write_marker(
                &marker,
                &format!("[subagent {} died with its process]", sc.id),
            );
        }
        let _ = sc.finish(&dir, State::Dead);
    }
}

/// Background tasks running in this process, after reaping the dead.
pub fn in_flight(subagents_dir: &Path) -> usize {
    reap_dead(subagents_dir);
    all(subagents_dir)
        .iter()
        .filter(|(_, sc)| sc.background && sc.is_live())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample(id: &str) -> Sidecar {
        Sidecar {
            id: id.into(),
            mode: TaskMode::Read,
            tier: Tier::Light,
            model: "claude-haiku-4-5".into(),
            background: true,
            worktree: None,
            branch: None,
            base: None,
            cap_usd: 0.25,
            process_nonce: process_nonce().into(),
            state: State::Running,
            cost_usd: 0.0,
            run: 1,
            escalated_to: None,
        }
    }

    #[test]
    fn all_orders_by_task_number() {
        let dir = std::env::temp_dir().join(format!("overseer-sc-{}", uuid::Uuid::now_v7()));
        for id in ["task-10", "task-2", "task-3-r1", "task-3", "task-1"] {
            let d = dir.join(id);
            std::fs::create_dir_all(&d).unwrap();
            sample(id).store(&d).unwrap();
        }
        let ids: Vec<String> = all(&dir).into_iter().map(|(_, sc)| sc.id).collect();
        assert_eq!(ids, ["task-1", "task-2", "task-3", "task-3-r1", "task-10"]);
    }

    #[test]
    fn task_seq_accepts_only_task_dirs() {
        assert_eq!(task_seq("task-12"), Some(12));
        for n in ["task-3-r1", "task-3.filtered", "wt-3", "bg-3", "task-"] {
            assert_eq!(task_seq(n), None, "{n}");
        }
        assert_eq!(done_marker(1), "done.txt");
        assert_eq!(done_marker(3), "done-3.txt");
    }

    #[test]
    fn write_marker_lands_whole_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("overseer-sc-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("done.txt");
        write_marker(&marker, "[subagent task-1 finished]\ndigest").unwrap();
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "[subagent task-1 finished]\ndigest"
        );
        let leftover: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "{leftover:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `running` dir left by a previous process must not wedge the cap.
    #[test]
    fn foreign_running_task_is_reaped_not_counted() {
        let root = std::env::temp_dir().join(format!("overseer-sc-{}", uuid::Uuid::now_v7()));
        let live = root.join("task-1");
        let dead = root.join("task-2");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&dead).unwrap();
        sample("task-1").store(&live).unwrap();
        let mut old = sample("task-2");
        old.process_nonce = "previous-process".into();
        old.store(&dead).unwrap();
        assert_eq!(in_flight(&root), 1);
        let reaped = Sidecar::load(&dead).unwrap();
        assert_eq!(reaped.state, State::Dead);
        let note = std::fs::read_to_string(dead.join("done.txt")).unwrap();
        assert!(note.contains("task-2 died with its process"), "{note}");
        assert_eq!(max_seq(&root), 2);
        let _ = std::fs::remove_dir_all(&root);
    }
}
