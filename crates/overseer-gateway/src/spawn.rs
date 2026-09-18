//! Run spawning — the "act" tier. The daemon never executes agent work
//! itself (thin-and-safe boundary, playbook §7 key decision): it spawns
//! `overseer exec` as a child process and reaps it later. Every spawn is
//! journaled with the session dir so the audit trail links intent → run.
//!
//! Model access flows through the child's own env/key-gating — the
//! daemon holds no API keys and strips them from its own address space
//! at startup (defense-in-depth: a compromised daemon yields nothing).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde::Serialize;

use crate::config::SpawnConfig;
use crate::event::now_ms;

/// A live spawned run.
pub struct Spawned {
    pub id: String,
    /// Inbox item this run serves, when any.
    pub inbox_id: Option<String>,
    pub child: Child,
    pub started_ms: u64,
    pub timeout_ms: u64,
    pub prompt: String,
    pub log_path: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct SpawnOutcome {
    pub id: String,
    pub inbox_id: Option<String>,
    pub exit: Option<i32>,
    pub killed: bool,
    pub wall_ms: u64,
    pub log_path: String,
}

/// Spawn `overseer exec --bare` for a prompt. `--bare` = throwaway
/// session in tmp (never ~/.overseer), fail-closed permission mode —
/// the unattended default. The child's stdout/stderr go to a per-run
/// log under `runs/` for post-hoc audit.
pub fn spawn_run(
    cfg: &SpawnConfig,
    runs_dir: &Path,
    prompt: &str,
    inbox_id: Option<String>,
    overseer_bin: &PathBuf,
) -> Result<Spawned, String> {
    let id = uuid::Uuid::now_v7().to_string();
    let log_path = runs_dir.join(format!("{id}.log"));
    let log = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;

    let mut cmd = Command::new(overseer_bin);
    cmd.arg("exec")
        .arg("--bare")
        .arg("--max-steps")
        .arg(cfg.max_steps.to_string())
        .arg(prompt)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .stdin(Stdio::null());
    if let Some(cwd) = &cfg.cwd {
        cmd.current_dir(cwd);
    }
    let child = cmd.spawn().map_err(|e| format!("spawn overseer: {e}"))?;
    Ok(Spawned {
        id,
        inbox_id,
        child,
        started_ms: now_ms(),
        timeout_ms: cfg.timeout_s * 1000,
        prompt: prompt.to_string(),
        log_path,
    })
}

/// Non-blocking reap: returns Some(outcome) when the run is done —
/// either exited or killed by the watchdog (wall-clock cap).
pub fn reap(sp: &mut Spawned) -> Option<SpawnOutcome> {
    if now_ms().saturating_sub(sp.started_ms) > sp.timeout_ms {
        let _ = sp.child.kill();
        let _ = sp.child.wait();
        return Some(SpawnOutcome {
            id: sp.id.clone(),
            inbox_id: sp.inbox_id.clone(),
            exit: None,
            killed: true,
            wall_ms: now_ms() - sp.started_ms,
            log_path: sp.log_path.display().to_string(),
        });
    }
    match sp.child.try_wait() {
        Ok(Some(status)) => Some(SpawnOutcome {
            id: sp.id.clone(),
            inbox_id: sp.inbox_id.clone(),
            exit: status.code(),
            killed: false,
            wall_ms: now_ms() - sp.started_ms,
            log_path: sp.log_path.display().to_string(),
        }),
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("overseer-gateway-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn live_child(
        program: &str,
        args: &[&str],
        timeout_ms: u64,
        started_ms: u64,
        tag: &str,
    ) -> Spawned {
        let child = Command::new(program)
            .args(args)
            .spawn()
            .expect("helper binary must spawn");
        Spawned {
            id: "test-run".to_string(),
            inbox_id: None,
            child,
            started_ms,
            timeout_ms,
            prompt: "test".to_string(),
            log_path: tmpdir(tag).join("test.log"),
        }
    }

    #[test]
    fn reap_kills_over_timeout_child() {
        let mut sp = live_child(
            "/bin/sleep",
            &["30"],
            1,
            now_ms().saturating_sub(5_000),
            "spawn-kill",
        );
        let outcome = reap(&mut sp).expect("over-timeout run must reap immediately");
        assert!(outcome.killed);
        assert_eq!(outcome.exit, None);
    }

    #[test]
    fn reap_collects_fast_exit() {
        let mut sp = live_child("/bin/sleep", &["0"], 60_000, now_ms(), "spawn-fast");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let outcome = loop {
            if let Some(outcome) = reap(&mut sp) {
                break outcome;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fast-exit child was not reaped in time"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(!outcome.killed);
        assert_eq!(outcome.exit, Some(0));
    }
}
