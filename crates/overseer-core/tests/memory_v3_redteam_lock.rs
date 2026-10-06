//! Red team class E: `StoreLock` exclusivity and concurrent review
//! writes, multi-threaded and multi-process (the test binary re-execs
//! itself as a worker via `REDTEAM_WORKER`). `REDTEAM_SLOW=1` enables
//! the 31 s live-holder case.

use overseer_core::memory::learn::{self, ApplyCtx};
use overseer_core::memory::{self, Scope, StoreLock};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

fn store(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ov-rt-lock-{tag}-{}", uuid::Uuid::now_v7()));
    memory::ensure(&d).unwrap();
    memory::commit(&d, "init");
    d
}

fn lock_path(s: &Path) -> PathBuf {
    s.join(".index").join("write.lock")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// One review-path ADD under the store lock (+ commit). Returns rejects.
fn add(store: &Path, w: usize, i: usize) -> Vec<String> {
    let stores = vec![(Scope::Project, store.to_path_buf())];
    let parsed = learn::parse(&format!(
        "ADD semantic w{w}-note-{i} :: worker {w} wrote durable fact number {i}"
    ));
    assert_eq!(parsed.ops.len(), 1, "{:?}", parsed.rejected);
    let ctx = ApplyCtx {
        stores: &stores,
        tainted: false,
        attended: false,
        stage_all: false,
        session_id: "redteam-lock",
        trigger: "test",
        through: 1,
        now: now(),
    };
    let out = learn::apply(&parsed, &ctx);
    out.rejected
}

/// Lock-guarded read-modify-write of `<store>/counter`.
fn bump(store: &Path) -> bool {
    let Ok(_l) = StoreLock::acquire(store) else {
        return false;
    };
    let p = store.join("counter");
    let n: u64 = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(2));
    std::fs::write(&p, format!("{}\n", n + 1)).unwrap();
    true
}

fn counter(store: &Path) -> u64 {
    std::fs::read_to_string(store.join("counter"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn fsck(store: &Path) {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(store)
        .args(["fsck", "--strict"])
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "fsck: {}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
}

fn notes_and_pointers(store: &Path) -> (usize, usize) {
    let notes = std::fs::read_dir(store.join("semantic"))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with('w'))
                .count()
        })
        .unwrap_or(0);
    let idx = std::fs::read_to_string(store.join("INDEX.md")).unwrap_or_default();
    let ptrs = idx.lines().filter(|l| l.starts_with("semantic/w")).count();
    (notes, ptrs)
}

/// Re-exec entry point. A no-op unless `REDTEAM_WORKER` is set.
#[test]
fn e_worker() {
    let Ok(mode) = std::env::var("REDTEAM_WORKER") else {
        return;
    };
    let s = PathBuf::from(std::env::var("REDTEAM_STORE").unwrap());
    let w: usize = std::env::var("REDTEAM_WID").unwrap().parse().unwrap();
    let n: usize = std::env::var("REDTEAM_N").unwrap().parse().unwrap();
    match mode.as_str() {
        "add" => {
            for i in 0..n {
                let r = add(&s, w, i);
                assert!(r.is_empty(), "worker {w} add {i} rejected: {r:?}");
            }
        }
        "counter" => {
            let mut fails = 0;
            for _ in 0..n {
                while !bump(&s) {
                    fails += 1;
                }
            }
            eprintln!("worker {w}: {fails} acquire timeouts");
        }
        "slow" => {
            let _l = StoreLock::acquire(&s).unwrap();
            std::fs::write(s.join("slow-inside"), "1").unwrap();
            std::thread::sleep(Duration::from_secs(31));
            std::fs::remove_file(s.join("slow-inside")).unwrap();
        }
        other => panic!("unknown worker {other}"),
    }
}

fn spawn(mode: &str, s: &Path, w: usize, n: usize) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "e_worker", "--nocapture", "--test-threads=1"])
        .env("REDTEAM_WORKER", mode)
        .env("REDTEAM_STORE", s)
        .env("REDTEAM_WID", w.to_string())
        .env("REDTEAM_N", n.to_string())
        .spawn()
        .unwrap()
}

const W: usize = 4;
const N: usize = 12;

#[test]
fn e_threads_add_commit_no_lost_writes() {
    let s = store("tadd");
    let hs: Vec<_> = (0..W)
        .map(|w| {
            let s = s.clone();
            std::thread::spawn(move || (0..N).flat_map(|i| add(&s, w, i)).collect::<Vec<_>>())
        })
        .collect();
    let rejects: Vec<String> = hs.into_iter().flat_map(|h| h.join().unwrap()).collect();
    assert!(rejects.is_empty(), "{rejects:?}");
    assert_eq!(notes_and_pointers(&s), (W * N, W * N));
    fsck(&s);
}

#[test]
fn e_processes_add_commit_no_lost_writes() {
    let s = store("padd");
    let kids: Vec<_> = (0..W).map(|w| spawn("add", &s, w, N)).collect();
    for mut k in kids {
        assert!(k.wait().unwrap().success());
    }
    assert_eq!(notes_and_pointers(&s), (W * N, W * N));
    fsck(&s);
}

#[test]
fn e_threads_counter_never_loses_an_increment() {
    let s = store("tctr");
    let hs: Vec<_> = (0..8)
        .map(|_| {
            let s = s.clone();
            std::thread::spawn(move || {
                for _ in 0..40 {
                    while !bump(&s) {}
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(counter(&s), 320);
}

#[test]
fn e_processes_counter_never_loses_an_increment() {
    let s = store("pctr");
    let kids: Vec<_> = (0..6).map(|w| spawn("counter", &s, w, 40)).collect();
    for mut k in kids {
        assert!(k.wait().unwrap().success());
    }
    assert_eq!(counter(&s), 240);
}

/// A dead writer's empty lock (old mtime) is recovered.
#[test]
fn e_old_empty_lock_is_recovered() {
    let s = store("emptyold");
    let f = std::fs::File::create(lock_path(&s)).unwrap();
    f.set_modified(std::time::SystemTime::now() - Duration::from_secs(120))
        .unwrap();
    drop(f);
    drop(StoreLock::acquire(&s).unwrap());
}

/// A just-created, still-empty lock (a live writer between create_new
/// and write) is stolen at once — both writers then hold the lock.
#[test]
fn e_fresh_empty_lock_is_not_stolen() {
    // Assertion adjusted to the flock policy (L1–L4): the lock file is
    // informational and NEVER deleted, so an unheld file — fresh, empty
    // or stale — carries no lock at all. The "writer between create_new
    // and write" window this test modelled no longer exists: exclusion
    // is the flock on the open fd, not the file's contents. (Same
    // guarantee as lock.rs's leftover_lock_file_does_not_block.)
    let s = store("emptyfresh");
    std::fs::write(lock_path(&s), "").unwrap();
    let t = std::time::Instant::now();
    let got = StoreLock::acquire(&s);
    assert!(
        got.is_ok(),
        "an unheld empty lock file blocked acquire for {:?}",
        t.elapsed()
    );
    // …but a FLOCKED file — whatever its bytes — refuses a second fd.
    let b = StoreLock::acquire(&s);
    assert!(b.is_err(), "acquired while the first fd still holds it");
}

/// A live holder past 30 s loses the lock to a second writer; the first
/// holder's Drop then deletes the second writer's lock file.
#[test]
fn e_live_holder_past_stale_age_keeps_exclusivity() {
    let s = store("stalelive");
    let a = StoreLock::acquire(&s).unwrap();
    // Equivalent to A having held the lock for 31 s.
    std::fs::write(
        lock_path(&s),
        format!("{}\t{}\n", std::process::id(), now() - 31),
    )
    .unwrap();
    let b = StoreLock::acquire(&s);
    assert!(b.is_err(), "B acquired while A still holds the lock");
    drop(a);
}

#[test]
fn e_slow_live_holder_31s_keeps_exclusivity() {
    if std::env::var("REDTEAM_SLOW").as_deref() != Ok("1") {
        eprintln!("skipped: set REDTEAM_SLOW=1");
        return;
    }
    let s = store("slow");
    let mut k = spawn("slow", &s, 0, 0);
    while !s.join("slow-inside").exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(30_200));
    let mut stolen = false;
    while s.join("slow-inside").exists() {
        if let Ok(l) = StoreLock::acquire(&s) {
            stolen = s.join("slow-inside").exists();
            drop(l);
            break;
        }
    }
    k.wait().unwrap();
    assert!(
        !stolen,
        "acquired the lock while the slow writer was inside"
    );
}

/// K writers race to replace one stale lock; at most one may be inside.
#[test]
fn e_racing_stealers_stay_exclusive() {
    let s = store("race");
    const K: usize = 8;
    let mut worst = 0;
    for _round in 0..40 {
        std::fs::write(lock_path(&s), "999999\t0\n").unwrap();
        let inside = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let bar = Arc::new(Barrier::new(K));
        let hs: Vec<_> = (0..K)
            .map(|_| {
                let (s, inside, max, bar) = (s.clone(), inside.clone(), max.clone(), bar.clone());
                std::thread::spawn(move || {
                    bar.wait();
                    if let Ok(l) = StoreLock::acquire(&s) {
                        let n = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        max.fetch_max(n, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(3));
                        inside.fetch_sub(1, Ordering::SeqCst);
                        drop(l);
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        worst = worst.max(max.load(Ordering::SeqCst));
        let _ = std::fs::remove_file(lock_path(&s));
    }
    assert_eq!(worst, 1, "{worst} writers inside the lock at once");
}
