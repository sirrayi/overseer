//! Store write lock (decision record §9.3): one memory mutation at a
//! time per store. `<store>/.index/write.lock` is claimed with
//! create-new semantics, carries `pid<TAB>epoch-secs`, and counts as
//! stale once older than [`STALE_SECS`]; acquisition retries for up to
//! [`ACQUIRE_MS`]. Dropping the guard removes the file.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A lock older than this is a leftover (crash/kill) and is replaced.
const STALE_SECS: u64 = 30;
/// Total time acquisition may spin before giving up.
const ACQUIRE_MS: u64 = 2_000;
const POLL_MS: u64 = 20;
const LOCK_NAME: &str = "write.lock";

/// Held write lock for one memory store; `Drop` removes the file.
pub struct StoreLock {
    path: PathBuf,
}

impl StoreLock {
    /// Claim `<store>/.index/write.lock`, replacing a stale (>30 s)
    /// leftover. Spins up to 2 s on a live lock, then errors.
    pub fn acquire(store: &Path) -> io::Result<StoreLock> {
        let dir = store.join(".index");
        crate::harden::ensure_private_dir(&dir)?;
        let path = dir.join(LOCK_NAME);
        let deadline = Instant::now() + Duration::from_millis(ACQUIRE_MS);
        let body = || {
            let now = super::now_secs();
            format!("{}\t{now}\n", std::process::id())
        };
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use io::Write;
                    f.write_all(body().as_bytes())?;
                    return Ok(StoreLock { path });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if stale(&path, super::now_secs()) {
                        // A stale file may be re-created between our read
                        // and remove; losing that race just retries.
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            format!("memory: store write lock held ({})", path.display()),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(POLL_MS));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Whether `dir` currently holds a live (non-stale) lock — tests and
    /// the stats surface; not a decision input (races by nature).
    #[allow(dead_code)]
    pub(crate) fn held(store: &Path) -> bool {
        let path = store.join(".index").join(LOCK_NAME);
        path.exists() && !stale(&path, super::now_secs())
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The lock file's recorded epoch is older than [`STALE_SECS`], or the
/// file is unreadable/corrupt (a torn write is never a live lock).
fn stale(path: &Path, now: u64) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return true;
    };
    text.split('\t')
        .nth(1)
        .and_then(|s| s.trim().parse::<u64>().ok())
        .is_none_or(|t| t.saturating_add(STALE_SECS) <= now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> PathBuf {
        let d = std::env::temp_dir().join(format!("ov-lock-{}", uuid::Uuid::now_v7()));
        crate::memory::ensure(&d).unwrap();
        d
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
        assert!(!path.exists(), "drop removes the lock");
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
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stale_lock_is_replaced() {
        let dir = store();
        let path = dir.join(".index").join(LOCK_NAME);
        crate::harden::ensure_private_dir(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("1\t{}\n", super::super::now_secs() - 60)).unwrap();
        assert!(!StoreLock::held(&dir));
        let _l = StoreLock::acquire(&dir).unwrap();
        assert!(StoreLock::held(&dir));
        // A torn lock file counts as stale too.
        drop(_l);
        std::fs::write(&path, "garbage\n").unwrap();
        assert!(StoreLock::acquire(&dir).is_ok());
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
}
