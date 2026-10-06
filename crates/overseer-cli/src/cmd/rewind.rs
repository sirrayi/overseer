//! `overseer rewind` — checkpoint restore (P1.9).

use std::path::PathBuf;

use overseer_core::rewind::Mode;

use crate::args::{self, Arg, Flag};

const REWIND_FLAGS: &[Flag] = &[Flag::value(&["--checkpoint"]), Flag::value(&["--mode"])];

/// `overseer rewind <session-dir> [--checkpoint <n>] [--mode <m>]` —
/// P1.9 checkpoint rewind. Modes: `code` (restore snapshotted files),
/// `conversation` (truncate events at the checkpoint boundary),
/// `both` (default), `summarize` (truncate + compact what remains).
/// Blind spot: `bash` side effects are never snapshotted — only
/// write/edit edits are recorded in the manifest.
/// Claim `dir`'s live lock for a restore; `Err` is the exit code (2 =
/// busy). A missing dir takes no lock — `restore` reports it.
fn lock_session(dir: &std::path::Path) -> Result<Option<overseer_core::live::LiveLock>, i32> {
    if !dir.is_dir() {
        return Ok(None);
    }
    match overseer_core::live::LiveLock::acquire(dir) {
        Ok(l) => Ok(Some(l)),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            eprintln!("overseer rewind: {}", overseer_core::live::BUSY);
            Err(2)
        }
        Err(e) => {
            eprintln!("overseer rewind: {}: {e}", dir.display());
            Err(1)
        }
    }
}

pub(crate) fn cmd_rewind(argv: &[String]) -> i32 {
    let parsed = match args::parse(argv, REWIND_FLAGS) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("overseer rewind: {e}");
            return 2;
        }
    };
    let mut dir: Option<PathBuf> = None;
    let mut want_cp: Option<u64> = None;
    let mut mode = "both".to_string();
    for arg in parsed.args {
        match arg {
            Arg::Flag {
                name: "--checkpoint",
                value: Some(v),
            } => {
                want_cp = v.trim_start_matches('e').parse().ok();
                if want_cp.is_none() {
                    eprintln!("overseer rewind: bad --checkpoint '{v}'");
                    return 2;
                }
            }
            Arg::Flag {
                name: "--mode",
                value: Some(v),
            } => match v.as_str() {
                "code" | "conversation" | "both" | "summarize" => mode = v,
                other => {
                    eprintln!(
                        "overseer rewind: bad --mode '{other}' (code|conversation|both|summarize)"
                    );
                    return 2;
                }
            },
            Arg::Flag { name, .. } => {
                eprintln!("overseer rewind: unknown flag '{name}'");
                return 2;
            }
            Arg::Pos(p) => dir = Some(PathBuf::from(p)),
        }
    }
    let Some(dir) = dir else {
        eprintln!("overseer rewind: session dir required");
        return 2;
    };

    // R5: a session live in another overseer process keeps appending to
    // its log — truncating it here would fork the hash chain. The lock is
    // held for the whole restore so no process can open the session
    // mid-rewind. CLI-only: the TUI's in-process /rewind holds it itself.
    let _live = match lock_session(&dir) {
        Ok(l) => l,
        Err(code) => return code,
    };

    let m = Mode::parse(&mode).unwrap_or(Mode::Both);
    match overseer_core::rewind::restore(&dir, want_cp, m) {
        Ok(r) => {
            if matches!(m, Mode::Code | Mode::Both) {
                println!(
                    "code: restored {} file(s), removed {} created file(s)",
                    r.restored, r.deleted
                );
            }
            if !matches!(m, Mode::Code) {
                println!(
                    "conversation: truncated {} event(s) at boundary e{}",
                    r.truncated, r.boundary
                );
            }
            if let Some(a) = r.compaction_at {
                println!("conversation: appended compaction summary at e{a}");
            }
            0
        }
        Err(e) => {
            eprintln!("overseer rewind: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    /// S3: the restore runs under a held live lock, not a probe.
    #[test]
    fn rewind_holds_the_live_lock_for_the_restore() {
        let d = std::env::temp_dir().join(format!("overseer-rewind-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let guard = super::lock_session(&d).unwrap();
        assert!(guard.is_some());
        assert!(overseer_core::live::held(&d), "lock held while restoring");
        drop(guard);
        assert!(!overseer_core::live::held(&d));
        let missing = d.join("nope");
        assert!(super::lock_session(&missing).unwrap().is_none());
        assert!(!missing.exists(), "a missing dir is not created");
        let _ = std::fs::remove_dir_all(&d);
    }
}
