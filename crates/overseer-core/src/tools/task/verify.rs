//! Verify mode: the engine-side half. Builds the verifier's brief (diff +
//! untracked files), appends the output contract, parses the verdict and
//! runs the tamper check. Writes into `.gitignore`d paths are invisible
//! to the tamper check — an accepted limitation (build output lands
//! there legitimately) — except in the git dir: its config, hooks and
//! info are hashed.
// DEFERRED(owner): cross-provider verifiers (verify on another vendor's model) — gate: same-transport tiers prove insufficient

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Command;

use crate::tools::middle_truncate;

const DIFF_CAP: usize = 24_000;
const UNTRACKED_FILES: usize = 20;
const UNTRACKED_FILE_CAP: usize = 2_000;
const UNTRACKED_CAP: usize = 12_000;

pub const CONTRACT: &str = "\
[verifier contract] Verify the stated task against the changes above by \
running the project's checks. Cite evidence for every claim. Do not modify \
files. End with exactly one fenced JSON block:
```json
{\"verdict\":\"pass|fail|partial\",\"evidence\":[{\"claim\":\"…\",\"source\":\"cmd|file:line\"}],\
\"issues\":[{\"severity\":\"blocker|minor\",\"desc\":\"…\",\"location\":\"…\"}],\"ran\":[\"…\"],\
\"confidence\":\"low|med|high\"}
```";

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    /// pass | fail | partial | unknown (missing/unparsable — never a pass).
    pub verdict: String,
    pub blockers: usize,
    /// What the verifier changed (set by the tamper check).
    pub tampered: Option<String>,
}

impl Verdict {
    /// `verdict: fail (2 blockers)`.
    pub fn summary(&self) -> String {
        match self.blockers {
            0 => format!("verdict: {}", self.verdict),
            1 => format!("verdict: {} (1 blocker)", self.verdict),
            n => format!("verdict: {} ({n} blockers)", self.verdict),
        }
    }

    /// A verifier that changed the repository fails, whatever it claimed.
    pub fn tampered(&mut self, what: &str) {
        self.verdict = "fail".into();
        self.tampered = Some(what.to_string());
    }
}

/// The verdict from the final fenced block of `text` — only that one: if
/// it is not a valid verdict (or the last fence never closes), the
/// verdict is `unknown` whatever came before.
pub fn parse(text: &str) -> Verdict {
    let mut blocks = Vec::new();
    let mut cur: Option<String> = None;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            match cur.take() {
                Some(b) => blocks.push(b),
                None => cur = Some(String::new()),
            }
        } else if let Some(b) = cur.as_mut() {
            b.push_str(line);
            b.push('\n');
        }
    }
    let unknown = Verdict {
        verdict: "unknown".into(),
        blockers: 0,
        tampered: None,
    };
    let Some(v) = blocks
        .last()
        .filter(|_| cur.is_none())
        .and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok())
    else {
        return unknown;
    };
    match v.get("verdict").and_then(|x| x.as_str()) {
        Some(verdict @ ("pass" | "fail" | "partial")) => Verdict {
            verdict: verdict.into(),
            blockers: v
                .get("issues")
                .and_then(|i| i.as_array())
                .map(|a| {
                    a.iter()
                        .filter(|i| i.get("severity").and_then(|s| s.as_str()) == Some("blocker"))
                        .count()
                })
                .unwrap_or(0),
            tampered: None,
        },
        _ => unknown,
    }
}

fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `(status, path)` per changed/untracked path; ignored paths excluded.
fn porcelain(cwd: &Path) -> Result<Vec<(String, String)>, String> {
    let raw = git(
        cwd,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let mut out = Vec::new();
    let mut it = raw.split('\0').filter(|e| e.len() > 3);
    while let Some(e) = it.next() {
        let (code, path) = (e[..2].to_string(), e[3..].to_string());
        if code.starts_with('R') || code.starts_with('C') {
            it.next(); // rename source
        }
        out.push((code, path));
    }
    Ok(out)
}

/// Whether `cwd` differs from `base`: a tracked diff (committed or not),
/// an untracked file or an ignored one. A git failure counts as changed —
/// the verifier then runs and reports it.
pub fn has_changes(cwd: &Path, base: &str) -> bool {
    let tracked = git(cwd, &["diff", base, "--name-only"]).map(|d| !d.trim().is_empty());
    let untracked = porcelain(cwd).map(|rows| rows.iter().any(|(c, _)| c == "??"));
    let ignored = ignored(cwd).map(|rows| !rows.is_empty());
    !matches!(
        (tracked, untracked, ignored),
        (Ok(false), Ok(false), Ok(false))
    )
}

/// Gitignored paths in `cwd` (an ignored dir is one entry, `dir/`).
pub fn ignored(cwd: &Path) -> Result<Vec<String>, String> {
    let raw = git(cwd, &["status", "--porcelain=v1", "-z", "--ignored"])?;
    Ok(raw
        .split('\0')
        .filter_map(|e| e.strip_prefix("!! "))
        .map(str::to_string)
        .collect())
}

/// The repository state a verifier must leave alone: `HEAD`, the sha256
/// of `git for-each-ref` (any branch/tag moved, made or deleted), `git
/// stash list`, path → hash of (status, content) from the porcelain
/// — which catches edits to already-dirty files `git status` misses —
/// and the git internals that run code or redirect git: the common dir's
/// `config`, `hooks/**` and `info/**`, plus an in-repo `core.hooksPath`.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    head: String,
    refs: String,
    stash: String,
    files: BTreeMap<String, u64>,
    internals: BTreeMap<String, u64>,
}

/// Path → hash of (mode, content or link target) for every entry at or
/// under `path`; symlinks are hashed, never followed.
fn hash_tree(path: &Path, label: String, out: &mut BTreeMap<String, u64>) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    meta.permissions().mode().hash(&mut h);
    if meta.is_dir() {
        let mut kids: Vec<_> = std::fs::read_dir(path)
            .map(|d| d.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        kids.sort();
        for k in kids {
            let name = k.to_string_lossy();
            hash_tree(&path.join(&k), format!("{label}/{name}"), out);
        }
    } else if meta.file_type().is_symlink() {
        std::fs::read_link(path).ok().hash(&mut h);
    } else {
        std::fs::read(path).ok().hash(&mut h);
    }
    out.insert(label, h.finish());
}

/// [`hash_tree`] over the git internals of the repo at `cwd`. A worktree's
/// hooks and config live in the common dir (`--git-common-dir`). Labels
/// are relative to the work tree top when inside it, else absolute.
fn internals(cwd: &Path) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    let Ok(common) = git(cwd, &["rev-parse", "--git-common-dir"]) else {
        return out;
    };
    let common = cwd.join(common.trim());
    let top = git(cwd, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(|t| std::path::PathBuf::from(t.trim()))
        .and_then(|t| t.canonicalize().ok());
    let label = |p: &Path| -> String {
        let p = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        match top.as_deref().and_then(|t| p.strip_prefix(t).ok()) {
            Some(rel) => rel.display().to_string(),
            None => p.display().to_string(),
        }
    };
    for sub in ["config", "hooks", "info"] {
        let p = common.join(sub);
        hash_tree(&p, label(&p), &mut out);
    }
    let hooks_path = git(cwd, &["config", "--get", "core.hooksPath"])
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty());
    if let (Some(h), Some(t)) = (hooks_path, top.as_deref()) {
        let p = t.join(h);
        if p.canonicalize().is_ok_and(|c| c.starts_with(t)) {
            hash_tree(&p, label(&p), &mut out);
        }
    }
    out
}

pub fn snapshot(cwd: &Path) -> Option<Snapshot> {
    use sha2::{Digest, Sha256};
    let rows = porcelain(cwd).ok()?;
    let head = git(cwd, &["rev-parse", "--verify", "HEAD"])
        .map(|h| h.trim().to_string())
        .unwrap_or_default();
    let refs = git(cwd, &["for-each-ref"]).ok()?;
    let refs = format!("{:x}", Sha256::digest(refs.as_bytes()));
    let stash = git(cwd, &["stash", "list"]).ok()?;
    let files = rows
        .into_iter()
        .map(|(code, path)| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            code.hash(&mut h);
            std::fs::read(cwd.join(&path)).ok().hash(&mut h);
            (path, h.finish())
        })
        .collect();
    Some(Snapshot {
        head,
        refs,
        stash,
        files,
        internals: internals(cwd),
    })
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// What differs between two snapshots, one entry per kind of change.
pub fn changed(before: &Snapshot, after: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();
    if before.head != after.head {
        out.push(format!(
            "HEAD {}→{}",
            short(&before.head),
            short(&after.head)
        ));
    }
    if before.refs != after.refs {
        out.push("refs changed".into());
    }
    if before.stash != after.stash {
        out.push("stash changed".into());
    }
    let mut git_paths: Vec<&str> = before
        .internals
        .iter()
        .filter(|(p, h)| after.internals.get(*p) != Some(h))
        .map(|(p, _)| p.as_str())
        .chain(
            after
                .internals
                .keys()
                .filter(|p| !before.internals.contains_key(*p))
                .map(String::as_str),
        )
        .collect();
    git_paths.sort();
    git_paths.dedup();
    if !git_paths.is_empty() {
        out.push(format!("git internals ({})", git_paths.join(", ")));
    }
    let mut paths: Vec<String> = before
        .files
        .iter()
        .filter(|(p, h)| after.files.get(*p) != Some(h))
        .map(|(p, _)| p.clone())
        .chain(
            after
                .files
                .keys()
                .filter(|p| !before.files.contains_key(*p))
                .cloned(),
        )
        .collect();
    paths.sort();
    paths.dedup();
    if !paths.is_empty() {
        out.push(format!("files {}", paths.join(", ")));
    }
    out
}

/// The verifier's first message: the stated task, the diff against
/// `base`, bounded untracked-file contents, then the contract.
pub fn brief(stated: &str, cwd: &Path, base: &str) -> Result<String, String> {
    let diff = git(cwd, &["diff", base])
        .map_err(|e| format!("verify needs a git repo at {}: {e}", cwd.display()))?;
    let mut untracked = String::new();
    let new: Vec<String> = porcelain(cwd)?
        .into_iter()
        .filter(|(c, _)| c == "??")
        .map(|(_, p)| p)
        .collect();
    for path in new.iter().take(UNTRACKED_FILES) {
        let body = match std::fs::read(cwd.join(path)) {
            Ok(b) => match String::from_utf8(b) {
                Ok(s) => middle_truncate(&s, UNTRACKED_FILE_CAP),
                Err(_) => "(binary)".into(),
            },
            Err(e) => format!("(unreadable: {e})"),
        };
        untracked.push_str(&format!("--- {path} (untracked)\n{body}\n"));
    }
    if new.len() > UNTRACKED_FILES {
        untracked.push_str(&format!(
            "(+{} more untracked)\n",
            new.len() - UNTRACKED_FILES
        ));
    }
    let diff = if diff.trim().is_empty() {
        "(no tracked changes)".to_string()
    } else {
        middle_truncate(&diff, DIFF_CAP)
    };
    let mut out = format!(
        "{stated}\n\n[changes under review: `git diff {base}` in {}]\n```diff\n{diff}\n```\n",
        cwd.display()
    );
    if !untracked.is_empty() {
        out.push_str(&format!(
            "[untracked files]\n```\n{}```\n",
            middle_truncate(&untracked, UNTRACKED_CAP)
        ));
    }
    out.push('\n');
    out.push_str(CONTRACT);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(cwd: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn worktree_and_hooks_path_internals_are_tamper_checked() {
        let root = std::env::temp_dir().join(format!(
            "ov-verify-internals-{}-{}",
            std::process::id(),
            crate::event::now_ms()
        ));
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q"]);
        std::fs::write(repo.join(".gitignore"), ".githooks/\n").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-qm", "x"]);
        sh(&repo, &["config", "core.hooksPath", ".githooks"]);
        let wt = root.join("wt");
        sh(
            &repo,
            &["worktree", "add", "-q", "-b", "w", wt.to_str().unwrap()],
        );

        // A worktree's hooks are the common dir's.
        let before = snapshot(&wt).unwrap();
        std::fs::write(repo.join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();
        let what = changed(&before, &snapshot(&wt).unwrap()).join("; ");
        let hook = repo.join(".git/hooks/pre-commit").canonicalize().unwrap();
        assert_eq!(what, format!("git internals ({})", hook.display()));

        // An in-repo core.hooksPath, though gitignored.
        let before = snapshot(&repo).unwrap();
        std::fs::create_dir_all(repo.join(".githooks")).unwrap();
        std::fs::write(repo.join(".githooks/pre-push"), "#!/bin/sh\n").unwrap();
        let what = changed(&before, &snapshot(&repo).unwrap()).join("; ");
        assert!(what.contains(".githooks/pre-push"), "{what}");

        // config and info.
        let before = snapshot(&repo).unwrap();
        sh(&repo, &["config", "alias.st", "!touch pwned"]);
        std::fs::write(repo.join(".git/info/exclude"), "*.rs\n").unwrap();
        let what = changed(&before, &snapshot(&repo).unwrap()).join("; ");
        assert_eq!(what, "git internals (.git/config, .git/info/exclude)");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn verdict_takes_the_final_fence_only_and_counts_blockers() {
        let text = "```json\n{\"verdict\":\"pass\"}\n```\nthen\n```json\n{\"verdict\":\"fail\",\"issues\":[\
            {\"severity\":\"blocker\"},{\"severity\":\"minor\"},{\"severity\":\"blocker\"}]}\n```";
        let v = parse(text);
        assert_eq!(v.summary(), "verdict: fail (2 blockers)");
        assert_eq!(parse("no block").verdict, "unknown");
        assert_eq!(
            parse("```json\n{\"verdict\":\"great\"}\n```").verdict,
            "unknown"
        );
        assert_eq!(parse("```\nnot json\n```").verdict, "unknown");
        // Only the final fence counts: a valid verdict followed by junk
        // (or an unclosed fence) is unknown, not the earlier verdict.
        let junk = "```json\n{\"verdict\":\"pass\"}\n```\nlater\n```\nnot json\n```";
        assert_eq!(parse(junk).verdict, "unknown");
        let unclosed = "```json\n{\"verdict\":\"pass\"}\n```\n```json\n{\"verdict\":";
        assert_eq!(parse(unclosed).verdict, "unknown");
        let mut p = parse("```json\n{\"verdict\":\"pass\"}\n```");
        p.tampered("HEAD a→b");
        assert_eq!(p.verdict, "fail");
        assert_eq!(p.tampered.as_deref(), Some("HEAD a→b"));
    }
}
