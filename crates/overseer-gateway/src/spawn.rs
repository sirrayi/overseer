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

use crate::channels;
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

/// The policy a run is spawned under. `Untrusted` runs carry the
/// external-approval floor and the engine's untrusted marker; local runs
/// keep the operator's own configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Local,
    /// Caused by an inbound channel message: `channel:<channel>:<sender>`.
    Untrusted(String),
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

/// Build the child command. Kept separate (and pure but for `Stdio`) so the
/// argv and environment a run is launched with can be asserted directly
/// instead of re-derived in tests.
fn build_command(
    cfg: &SpawnConfig,
    prompt: &str,
    origin: &Origin,
    overseer_bin: &Path,
    out: Stdio,
    err: Stdio,
) -> Command {
    let mut cmd = Command::new(overseer_bin);
    cmd.arg("exec")
        .arg("--bare")
        .arg("--max-steps")
        .arg(cfg.max_steps.to_string());
    if let Origin::Untrusted(marker) = origin {
        // An untrusted-originated run may not act on external effects
        // without approval, whatever the local autonomy map says, and it
        // tells the engine where it came from so the session starts armed.
        cmd.args(channels::UNTRUSTED_AUTONOMY_FLOOR);
        cmd.env(channels::UNTRUSTED_ENV, marker);
    }
    cmd.arg(prompt).stdout(out).stderr(err).stdin(Stdio::null());
    if let Some(cwd) = &cfg.cwd {
        cmd.current_dir(cwd);
    }
    cmd
}

/// Spawn `overseer exec --bare` for a prompt. `--bare` = throwaway
/// session in tmp (never ~/.overseer), fail-closed permission mode —
/// the unattended default. The child's stdout/stderr go to a per-run
/// log under `runs/` for post-hoc audit.
///
/// The origin is explicit (P7-4): an untrusted origin *adds* the
/// external-approval autonomy floor and exports the marker the engine
/// arms on; it never removes anything the local config set.
pub fn spawn_run_from(
    cfg: &SpawnConfig,
    runs_dir: &Path,
    prompt: &str,
    inbox_id: Option<String>,
    overseer_bin: &Path,
    origin: &Origin,
) -> Result<Spawned, String> {
    let id = uuid::Uuid::now_v7().to_string();
    let log_path = runs_dir.join(format!("{id}.log"));
    let log = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;

    let mut cmd = build_command(
        cfg,
        prompt,
        origin,
        overseer_bin,
        Stdio::from(log),
        Stdio::from(log_err),
    );
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
    use crate::test_util::tmpdir;

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
    fn untrusted_spawn_carries_the_floor_and_the_marker() {
        // P7-4: an untrusted-originated run gets the autonomy floor in its
        // argv and the origin marker in its environment. A local run gets
        // neither — the floor is added, never subtracted from.
        let cfg = SpawnConfig {
            max_concurrent: 1,
            max_steps: 7,
            timeout_s: 5,
            cwd: None,
        };
        let bin = PathBuf::from("/nonexistent/overseer");
        let untrusted = build_command(
            &cfg,
            "do the thing",
            &Origin::Untrusted("channel:telegram:77".into()),
            &bin,
            Stdio::null(),
            Stdio::null(),
        );
        let args: Vec<String> = untrusted
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "exec",
                "--bare",
                "--max-steps",
                "7",
                "--autonomy",
                "external=approve",
                "do the thing",
            ]
        );
        let envs: Vec<(String, Option<String>)> = untrusted
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect();
        assert_eq!(
            envs,
            vec![(
                channels::UNTRUSTED_ENV.to_string(),
                Some("channel:telegram:77".to_string())
            )]
        );

        let local = build_command(
            &cfg,
            "do the thing",
            &Origin::Local,
            &bin,
            Stdio::null(),
            Stdio::null(),
        );
        let local_args: Vec<String> = local
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            local_args,
            vec!["exec", "--bare", "--max-steps", "7", "do the thing"]
        );
        assert_eq!(local.get_envs().count(), 0, "local runs export nothing");
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
