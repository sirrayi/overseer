//! File-based memory v1 (playbook Ch.3 §9.4).
//!
//! `/memory/` is a directory of topic files the agent edits with ordinary
//! file tools — no special memory tool (the playbook's minimal option).
//! `INDEX.md` is a ≤25KB file of one-line pointers, injected at the *end of
//! the static prompt region* every turn: the index is always in context,
//! the topic files are read on demand (progressive disclosure).
//!
//! The dir is git-versioned (Letta MemFS): free history, diffs, rollback.
//! Commits are engine-made at turn boundaries, not model actions.

use std::path::{Path, PathBuf};
use std::process::Command;

pub const INDEX_NAME: &str = "INDEX.md";
/// Playbook cap: the index is a pointer table, not a document store.
pub const INDEX_CAP: usize = 25_000;

const SEED_INDEX: &str = "# Memory Index\n\n\
    One line per topic file: `name.md — what it's about`. \
    Keep this index small; details live in the files.\n";

/// Create the memory dir and seed INDEX.md if absent. Returns the index path.
pub fn ensure(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let idx = dir.join(INDEX_NAME);
    if !idx.exists() {
        std::fs::write(&idx, SEED_INDEX)?;
    }
    Ok(idx)
}

/// The system-prompt segment carrying the index — sits at the end of the
/// static region (Invariant 2): stable bytes when the index is unchanged,
/// and an edit only invalidates cache from this segment onward.
/// Re-read every turn because the model may have just edited it. An index
/// over the cap is truncated *with a repair note* — never silently.
pub fn index_segment(dir: &Path) -> String {
    let idx = dir.join(INDEX_NAME);
    let text = std::fs::read_to_string(&idx).unwrap_or_default();
    let (body, note) = if text.len() > INDEX_CAP {
        // Largest char-boundary byte offset still within the cap.
        let cut = text
            .char_indices()
            .map(|(i, c)| i + c.len_utf8())
            .take_while(|end| *end <= INDEX_CAP)
            .last()
            .unwrap_or(0);
        (
            &text[..cut],
            "\n\n[overseer] INDEX.md exceeds 25KB — prune it: keep only \
             one-line pointers and move detail into topic files.",
        )
    } else {
        (text.as_str(), "")
    };
    format!(
        "## Memory index\n\
         `{}/` is your persistent memory — read and update it with ordinary \
         file tools. {INDEX_NAME} holds one-line pointers (≤25KB); details \
         live in topic files you create there.\n\n{body}{note}",
        dir.display()
    )
}

/// Git-version the memory dir. Runs `git init` once, then commits any dirty
/// state. Best-effort: memory works without history, so failures are
/// swallowed (no git binary, read-only fs) rather than killing the turn.
pub fn commit(dir: &Path, msg: &str) {
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if !dir.join(".git").exists() && !git(&["init", "-q"]) {
        return;
    }
    if !git(&["add", "-A"]) {
        return;
    }
    // diff --cached --quiet exits 1 when there's something to commit.
    if git(&["diff", "--cached", "--quiet"]) {
        return;
    }
    let _ = git(&[
        "-c",
        "user.name=overseer",
        "-c",
        "user.email=overseer@local",
        "commit",
        "-qm",
        msg,
    ]);
}

/// Sleep-time consolidation (P3.8): a small-tier call that dedupes and
/// tightens `INDEX.md`, then a git commit. Topic files are read for
/// context but only the index is rewritten — merging topic bodies is the
/// model's job through ordinary edits, not a bulk engine rewrite.
///
/// The model returns the new index between `---INDEX---` markers; the
/// engine writes it (hard-capped) and reports what changed. Deterministic
/// fallback: a parse failure leaves the index untouched.
pub fn consolidate(
    provider: &dyn crate::provider::Provider,
    model: &str,
    dir: &Path,
) -> Result<String, String> {
    let idx = ensure(dir).map_err(|e| e.to_string())?;
    let old_index = std::fs::read_to_string(&idx).unwrap_or_default();

    // Topic files: bounded context for the dedupe pass.
    let mut topics = String::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "md")
                && e.file_name() != INDEX_NAME
            {
                if let Ok(t) = std::fs::read_to_string(&p) {
                    let head: String = t.chars().take(2_000).collect();
                    topics.push_str(&format!("\n### {}\n{head}\n", e.file_name().to_string_lossy()));
                }
            }
        }
    }

    let prompt = format!(
        "You are consolidating an agent's file-based memory. Below is \
         INDEX.md (one-line pointers) and the heads of the topic files.\n\
         Rewrite INDEX.md only: dedupe pointers, drop stale entries whose \
         topic file is gone, keep one line per topic in the form \
         `name.md — what it's about`. Validity: if a topic's content says \
         it expired or was superseded, drop its pointer.\n\
         Reply with the full new index between ---INDEX--- markers.\n\n\
         == CURRENT INDEX.md ==\n{old_index}\n== TOPIC HEADS =={topics}"
    );
    let msgs = [crate::ir::Message::user_text(prompt)];
    let req = crate::provider::Request {
        model,
        system: &[],
        tools: &[],
        messages: &msgs,
        max_tokens: 4_096,
        thinking_budget: None,
        effort: Some(crate::provider::Effort::Min),
        cache_breakpoints: false,
    };
    let resp = provider
        .complete(&req)
        .map_err(|e| format!("consolidate: {e}"))?;
    let text: String = resp
        .blocks
        .iter()
        .filter_map(|b| match b {
            crate::ir::Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    let new_index = text
        .split("---INDEX---")
        .nth(1)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "consolidate: model reply had no ---INDEX--- section".to_string()
        })?;
    let capped: String = new_index.chars().take(INDEX_CAP).collect();
    std::fs::write(&idx, format!("{capped}\n")).map_err(|e| e.to_string())?;
    commit(dir, "consolidate");

    let dropped = old_index
        .lines()
        .filter(|l| l.contains(".md"))
        .filter(|l| !new_index.contains(l.trim()))
        .count();
    Ok(format!(
        "consolidated: {} → {} index lines, {dropped} pointers dropped",
        old_index.lines().count(),
        capped.lines().count()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-mem-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn ensure_seeds_index() {
        let dir = tmpdir().join("memory");
        let idx = ensure(&dir).unwrap();
        assert!(idx.exists());
        let text = std::fs::read_to_string(&idx).unwrap();
        assert!(text.contains("Memory Index"));
    }

    #[test]
    fn segment_carries_index_and_cap_note() {
        let dir = tmpdir();
        let idx = ensure(&dir).unwrap();
        std::fs::write(&idx, "facts.md — user facts\n").unwrap();
        let seg = index_segment(&dir);
        assert!(seg.contains("## Memory index"));
        assert!(seg.contains("facts.md — user facts"));
        assert!(!seg.contains("exceeds 25KB"));

        // Over-cap index → truncated + repair note.
        std::fs::write(&idx, "x".repeat(INDEX_CAP + 100)).unwrap();
        let seg = index_segment(&dir);
        assert!(seg.contains("exceeds 25KB"));
        assert!(seg.len() < INDEX_CAP + 1_000);
    }

    #[test]
    fn commit_versions_changes() {
        let dir = tmpdir();
        ensure(&dir).unwrap();
        commit(&dir, "seed");
        assert!(dir.join(".git").exists());
        std::fs::write(dir.join("facts.md"), "likes rust\n").unwrap();
        commit(&dir, "add facts");
        // Log should have two commits.
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["log", "--oneline"])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stdout);
        assert_eq!(log.lines().count(), 2);
    }

    struct FixedProvider {
        reply: String,
    }
    impl crate::provider::Provider for FixedProvider {
        fn complete(
            &self,
            req: &crate::provider::Request,
        ) -> Result<crate::provider::Response, crate::provider::ProviderError> {
            assert_eq!(req.effort, Some(crate::provider::Effort::Min));
            Ok(crate::provider::Response {
                blocks: vec![crate::ir::Block::Text {
                    text: self.reply.clone(),
                }],
                stop_reason: crate::provider::StopReason::EndTurn,
                usage: crate::ir::Usage::default(),
                request_bytes: 0,
                latency_ms: 0,
            })
        }
        fn name(&self) -> &'static str {
            "fixed"
        }
    }

    #[test]
    fn consolidate_rewrites_index_and_commits() {
        let dir = tmpdir();
        let idx = ensure(&dir).unwrap();
        std::fs::write(&idx, "# Memory Index\n\nfacts.md — old\ndupe.md — old\n").unwrap();
        std::fs::write(dir.join("facts.md"), "data").unwrap();
        let p = FixedProvider {
            reply: "---INDEX---\n# Memory Index\n\nfacts.md — user facts\n---INDEX---".into(),
        };
        let msg = consolidate(&p, "tiny", &dir).unwrap();
        assert!(msg.contains("consolidated"));
        let new = std::fs::read_to_string(&idx).unwrap();
        assert!(new.contains("facts.md — user facts"));
        assert!(!new.contains("dupe.md"), "stale pointer dropped");
        assert!(dir.join(".git").exists(), "consolidate commits");
    }

    #[test]
    fn consolidate_parse_failure_leaves_index() {
        let dir = tmpdir();
        let idx = ensure(&dir).unwrap();
        std::fs::write(&idx, "original\n").unwrap();
        let p = FixedProvider {
            reply: "no markers here".into(),
        };
        assert!(consolidate(&p, "tiny", &dir).is_err());
        assert_eq!(std::fs::read_to_string(&idx).unwrap(), "original\n");
    }
}
