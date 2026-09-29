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
