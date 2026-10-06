//! Session liveness lock (memory v3 F4): `<session>/live.lock`, an
//! `flock(2)` via [`File::try_lock`] held from `Agent::start`/`resume`
//! until the Agent drops. One live writer per session: a second
//! process' `EventLog::open` on the same `events.jsonl` would mint the
//! same next ids and fork the hash chain.
//!
//! flock on a second fd conflicts even in-process, so a process-global
//! registry keeps the same-process case working (TUI SwitchSession,
//! tests): a repeated `acquire` of the same canonical session dir bumps
//! a refcount on the one held fd; the last drop releases it.

use std::collections::HashMap;
use std::fs::{File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const LOCK_NAME: &str = "live.lock";
/// How long a contended acquire spins before reporting the session as
/// busy. Long enough to outlive fork→exec fd-inheritance windows,
/// short enough that a genuinely live session fails fast.
const ACQUIRE_MS: u64 = 2_000;
const POLL_MS: u64 = 20;

/// Why an acquire refused — printed verbatim by `--resume`; `memory
/// learn` appends its own retry hint (R2: the constant must stay generic
/// — a resume refusal has nothing to do with learn).
pub const BUSY: &str = "session is open in another overseer process";

/// canonical session dir → (held fd, same-process refcount). The File
/// lives in the map: removing the last refcount drops it, and the
/// kernel releases the flock on close — crash included.
static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, (File, usize)>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<PathBuf, (File, usize)>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn canonical(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// Held liveness claim on one session dir; `Drop` decrements the
/// same-process refcount and releases the flock on the last one.
#[derive(Debug)]
pub struct LiveLock {
    dir: PathBuf,
}

impl LiveLock {
    /// Claim the session's live lock. A held lock in another process
    /// errors with [`BUSY`]; in this process it refcounts.
    pub fn acquire(dir: &Path) -> io::Result<LiveLock> {
        crate::harden::ensure_private_dir(dir)?;
        let dir = canonical(dir);
        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, n)) = reg.get_mut(&dir) {
            *n += 1;
            return Ok(LiveLock { dir });
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(LOCK_NAME))?;
        // Poll, don't single-shot: flock pins the open file description,
        // and a child mid-fork→exec inherits this fd — a WouldBlock
        // window of a few ms after a holder dropped. CLOEXEC clears it
        // at exec, so a short spin distinguishes a real holder.
        let deadline = Instant::now() + Duration::from_millis(ACQUIRE_MS);
        loop {
            match file.try_lock() {
                Ok(()) => {
                    reg.insert(dir.clone(), (file, 1));
                    return Ok(LiveLock { dir });
                }
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            format!("{BUSY} ({})", dir.display()),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(POLL_MS));
                }
                Err(TryLockError::Error(e)) => return Err(e),
            }
        }
    }
}

impl Drop for LiveLock {
    fn drop(&mut self) {
        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, n)) = reg.get_mut(&self.dir) {
            *n -= 1;
            if *n == 0 {
                reg.remove(&self.dir); // drops the File → flock released
            }
        }
    }
}

/// Whether `dir`'s session lock is held anywhere — a fresh fd's
/// `try_lock` fails iff another fd (any process, this one included)
/// holds it. Advisory: the winner of a check-then-acquire race is the
/// acquire, never this.
pub fn held(dir: &Path) -> bool {
    let Ok(file) = File::open(dir.join(LOCK_NAME)) else {
        return false;
    };
    matches!(file.try_lock(), Err(TryLockError::WouldBlock))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn dir() -> PathBuf {
        std::env::temp_dir().join(format!("ov-live-{}", uuid::Uuid::now_v7()))
    }

    /// A forked child holds its inherited copy of a flock'd fd until
    /// exec clears it (CLOEXEC): `held` can read a transient WouldBlock
    /// right after the real holder dropped. Poll briefly.
    fn soon_free(d: &Path) -> bool {
        for _ in 0..50 {
            if !held(d) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn same_process_acquires_refcount_and_last_drop_releases() {
        let d = dir();
        let a = LiveLock::acquire(&d).unwrap();
        let b = LiveLock::acquire(&d).unwrap();
        assert!(held(&d));
        drop(a);
        assert!(held(&d), "first drop only decrements the refcount");
        drop(b);
        assert!(soon_free(&d), "last drop releases the flock");
        assert!(
            LiveLock::acquire(&d).is_ok(),
            "re-acquire after the last drop"
        );
    }

    #[test]
    fn aliased_paths_share_one_lock() {
        let d = dir();
        crate::harden::ensure_private_dir(&d).unwrap();
        let a = LiveLock::acquire(&d).unwrap();
        let b = LiveLock::acquire(&d.join(".")).unwrap();
        drop(a);
        drop(b);
        // Other tests may hold locks concurrently; only this key must go.
        assert!(
            !registry().lock().unwrap().contains_key(&canonical(&d)),
            "canonical keying dedups aliased dirs"
        );
    }

    /// Re-exec worker: `OV_LIVE_WORKER=<dir>\t<fifo>` holds the lock,
    /// writes "ready" to <fifo>, then sleeps until killed.
    fn worker() -> bool {
        let Ok(spec) = std::env::var("OV_LIVE_WORKER") else {
            return false;
        };
        let (dir, fifo) = spec.split_once('\t').unwrap();
        let _l = LiveLock::acquire(Path::new(dir)).unwrap();
        std::fs::write(fifo, "ready").unwrap();
        std::thread::sleep(Duration::from_secs(60));
        true
    }

    #[test]
    fn live_lock_is_exclusive_across_processes() {
        if worker() {
            return;
        }
        let d = dir();
        let fifo = std::env::temp_dir().join(format!("ov-livefifo-{}", uuid::Uuid::now_v7()));
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "live::tests::live_lock_is_exclusive_across_processes",
            ])
            .env(
                "OV_LIVE_WORKER",
                format!("{}\t{}", d.display(), fifo.display()),
            )
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fifo.exists() {
            assert!(Instant::now() < deadline, "child never reported ready");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(held(&d));
        let err = LiveLock::acquire(&d).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert!(err.to_string().contains(BUSY));
        let _ = child.kill();
        let _ = child.wait();
    }
}
