//! Store write lock (decision record §9.3): one memory mutation at a
//! time per store. `<store>/.index/write.lock` is an `flock(2)` via
//! [`File::try_lock`]; acquisition retries for up to [`ACQUIRE_MS`].
//! Dropping the guard closes the fd, which releases the lock — the
//! kernel also releases it on crash/kill, so there is no staleness
//! logic. The file itself is NEVER unlinked: removing a flocked file
//! lets a second open race a new inode and breaks exclusion. Its
//! `pid<TAB>epoch-secs` contents are informational only.

use std::fs::{File, TryLockError};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// Total time acquisition may spin before giving up. Sized for a busy
/// store, not an interactive refusal: under W1 the lock covers a whole
/// read-modify-write + commit (+ MEMORY.md regen ≈ hundreds of ms in a
/// debug build), so a handful of queued writers can legitimately outwait
/// a 2 s budget — and a timed-out write is a lost write
/// (E-lock-not-exclusive's apply storm). A dead holder releases at the
/// kernel level, so a long budget never waits on a corpse.
const ACQUIRE_MS: u64 = 10_000;
const POLL_MS: u64 = 20;
const LOCK_NAME: &str = "write.lock";

/// Held write lock for one memory store; `Drop` closes the fd and the
/// kernel releases the lock. Nested acquire on another fd conflicts
/// even in-process — callers hold this for their whole
/// read-modify-write, and inner helpers never re-acquire.
#[derive(Debug)]
pub struct StoreLock {
    _file: File,
}

impl StoreLock {
    /// Claim `<store>/.index/write.lock`. Spins up to [`ACQUIRE_MS`]
    /// while another fd holds the flock, then errors.
    pub fn acquire(store: &Path) -> io::Result<StoreLock> {
        let dir = store.join(".index");
        crate::harden::ensure_private_dir(&dir)?;
        let path = dir.join(LOCK_NAME);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let deadline = Instant::now() + Duration::from_millis(ACQUIRE_MS);
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            format!("memory: store write lock held ({})", path.display()),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(POLL_MS));
                }
                Err(TryLockError::Error(e)) => return Err(e),
            }
        }
        // Informational only: who holds it and since when. Nothing
        // reads this — the flock IS the lock — so a torn write is
        // harmless.
        use io::Write;
        let body = format!("{}\t{}\n", std::process::id(), super::now_secs());
        let _ = file
            .set_len(0)
            .and_then(|_| (&file).write_all(body.as_bytes()));
        Ok(StoreLock { _file: file })
    }

    /// Whether `dir`'s lock is currently held — a fresh fd's `try_lock`
    /// fails iff another fd holds it. Tests and the stats surface; not
    /// a decision input (races by nature).
    #[allow(dead_code)]
    pub(crate) fn held(store: &Path) -> bool {
        let path = store.join(".index").join(LOCK_NAME);
        let Ok(file) = File::open(&path) else {
            return false;
        };
        matches!(file.try_lock(), Err(TryLockError::WouldBlock))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ov-lock-{}", uuid::Uuid::now_v7()));
        crate::memory::ensure(&d).unwrap();
        d
    }

    /// A forked child holds its inherited copy of a flock'd fd until
    /// exec clears it (CLOEXEC): `held` can read a transient WouldBlock
    /// right after the real holder dropped. Poll briefly for the kernel
    /// to catch up.
    fn soon_free(dir: &Path) -> bool {
        for _ in 0..50 {
            if !StoreLock::held(dir) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn acquire_drop_releases_and_contents_record_pid() {
        let dir = store();
        let path = dir.join(".index").join(LOCK_NAME);
        {
            let _l = StoreLock::acquire(&dir).unwrap();
            assert!(path.is_file());
            let text = std::fs::read_to_string(&path).unwrap();
            assert_eq!(
                text.split('\t').next().unwrap(),
                std::process::id().to_string()
            );
            assert!(StoreLock::held(&dir));
        }
        assert!(soon_free(&dir), "close releases the flock");
        assert!(path.exists(), "drop never unlinks the lock file");
        assert!(StoreLock::acquire(&dir).is_ok(), "re-acquire after drop");
    }

    #[test]
    fn second_acquire_waits_and_times_out() {
        let dir = store();
        let _l = StoreLock::acquire(&dir).unwrap();
        let t = Instant::now();
        let err = match StoreLock::acquire(&dir) {
            Err(e) => e,
            Ok(_) => panic!("a held lock must not re-acquire"),
        };
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert!(t.elapsed() >= Duration::from_millis(ACQUIRE_MS));
        assert!(t.elapsed() < Duration::from_millis(ACQUIRE_MS + 5_000));
    }

    #[test]
    fn leftover_lock_file_does_not_block() {
        // L1/L4: a file left by a dead process — empty or with a stale
        // payload — carries no lock; the kernel released it on exit.
        let dir = store();
        let path = dir.join(".index").join(LOCK_NAME);
        crate::harden::ensure_private_dir(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        assert!(!StoreLock::held(&dir));
        let _l = StoreLock::acquire(&dir).unwrap();
        drop(_l);
        std::fs::write(
            &path,
            format!("999999\t{}\n", super::super::now_secs() - 60),
        )
        .unwrap();
        assert!(StoreLock::acquire(&dir).is_ok());
    }

    #[test]
    fn live_lock_is_never_stolen_by_age() {
        // L3: a live holder outlives any clock threshold — the flock
        // doesn't expire while the fd is open.
        let dir = store();
        let _l = StoreLock::acquire(&dir).unwrap();
        assert!(StoreLock::held(&dir));
        let err = StoreLock::acquire(&dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert!(StoreLock::held(&dir), "the live lock is still held");
    }

    #[test]
    fn contention_serializes_writers() {
        let dir = store();
        let counter = dir.join("n.txt");
        std::fs::write(&counter, "0").unwrap();
        let dir2 = dir.clone();
        let counter2 = counter.clone();
        let t = std::thread::spawn(move || {
            for _ in 0..10 {
                let _l = StoreLock::acquire(&dir2).unwrap();
                let n: u64 = std::fs::read_to_string(&counter2)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                std::fs::write(&counter2, format!("{}", n + 1)).unwrap();
            }
        });
        for _ in 0..10 {
            let _l = StoreLock::acquire(&dir).unwrap();
            let n: u64 = std::fs::read_to_string(&counter)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            std::fs::write(&counter, format!("{}", n + 1)).unwrap();
        }
        t.join().unwrap();
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "20");
    }

    #[test]
    fn contention_serializes_across_threads_exact() {
        // N threads × M read-modify-write increments land exactly.
        let dir = store();
        let counter = dir.join("n.txt");
        std::fs::write(&counter, "0").unwrap();
        let mut joins = Vec::new();
        for _ in 0..4 {
            let dir2 = dir.clone();
            let counter2 = counter.clone();
            joins.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    let _l = StoreLock::acquire(&dir2).unwrap();
                    let n: u64 = std::fs::read_to_string(&counter2)
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    std::fs::write(&counter2, format!("{}", n + 1)).unwrap();
                }
            }));
        }
        for j in joins {
            j.join().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "100");
    }

    /// Re-exec workers keyed on `OV_LOCK_WORKER`:
    /// - `hold\t<dir>\t<fifo>` — hold the lock, write "ready" to <fifo>,
    ///   then sleep until killed;
    /// - `bump\t<dir>\t<counter>\t<M>` — M locked read-modify-write
    ///   increments of <counter>, then exit.
    fn worker() -> bool {
        let Ok(spec) = std::env::var("OV_LOCK_WORKER") else {
            return false;
        };
        let (mode, rest) = spec.split_once('\t').unwrap();
        match mode {
            "hold" => {
                let (dir, fifo) = rest.split_once('\t').unwrap();
                let _l = StoreLock::acquire(std::path::Path::new(dir)).unwrap();
                std::fs::write(fifo, "ready").unwrap();
                std::thread::sleep(Duration::from_secs(60));
            }
            "bump" => {
                let mut it = rest.split('\t');
                let dir = std::path::PathBuf::from(it.next().unwrap());
                let counter = std::path::PathBuf::from(it.next().unwrap());
                let m: u64 = it.next().unwrap().parse().unwrap();
                for _ in 0..m {
                    let _l = StoreLock::acquire(&dir).unwrap();
                    let n: u64 = std::fs::read_to_string(&counter)
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    std::fs::write(&counter, format!("{}", n + 1)).unwrap();
                }
            }
            _ => panic!("bad OV_LOCK_WORKER mode"),
        }
        true
    }

    #[test]
    fn contention_serializes_across_processes() {
        if worker() {
            return;
        }
        let dir = store();
        let counter = dir.join("n.txt");
        std::fs::write(&counter, "0").unwrap();
        let mut children = Vec::new();
        for _ in 0..3 {
            children.push(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "memory::lock::tests::contention_serializes_across_processes",
                    ])
                    .env(
                        "OV_LOCK_WORKER",
                        format!("bump\t{}\t{}\t20", dir.display(), counter.display()),
                    )
                    .spawn()
                    .unwrap(),
            );
        }
        for mut c in children {
            assert!(c.wait().unwrap().success());
        }
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "60");
    }

    #[test]
    fn held_releases_when_holder_dies() {
        // A killed holder's flock is released by the kernel.
        if worker() {
            return;
        }
        let dir = store();
        let fifo = std::env::temp_dir().join(format!("ov-lockfifo-{}", uuid::Uuid::now_v7()));
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "memory::lock::tests::held_releases_when_holder_dies",
            ])
            .env(
                "OV_LOCK_WORKER",
                format!("hold\t{}\t{}", dir.display(), fifo.display()),
            )
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fifo.exists() {
            assert!(Instant::now() < deadline, "child never reported ready");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(StoreLock::held(&dir));
        let _ = child.kill();
        let _ = child.wait();
        let got = StoreLock::acquire(&dir);
        assert!(got.is_ok(), "dead holder's lock must be free: {got:?}");
    }
}
