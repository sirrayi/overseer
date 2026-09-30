//! Verify mode: the engine-side half. Builds the verifier's brief (diff +
//! untracked files), appends the output contract, parses the verdict and
//! runs the tamper check. Writes into `.gitignore`d paths are invisible
//! to the tamper check — an accepted limitation (build output lands
//! there legitimately).
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

    /// A verifier that changed files can at best be `partial`.
    pub fn tampered(&mut self) {
        if self.verdict == "pass" {
            self.verdict = "partial".into();
        }
    }
}

/// The verdict from the last fenced block of `text` that parses as JSON.
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
    };
    let Some(v) = blocks
        .iter()
        .rev()
        .find_map(|b| serde_json::from_str::<serde_json::Value>(b).ok())
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

/// Path → hash of (status, content) — catches edits to already-dirty
/// files that `git status` alone would miss.
pub fn snapshot(cwd: &Path) -> Option<BTreeMap<String, u64>> {
    let rows = porcelain(cwd).ok()?;
    Some(
        rows.into_iter()
            .map(|(code, path)| {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                code.hash(&mut h);
                std::fs::read(cwd.join(&path)).ok().hash(&mut h);
                (path, h.finish())
            })
            .collect(),
    )
}

/// Paths whose status or content differs between two snapshots.
pub fn changed(before: &BTreeMap<String, u64>, after: &BTreeMap<String, u64>) -> Vec<String> {
    let mut paths: Vec<String> = before
        .iter()
        .filter(|(p, h)| after.get(*p) != Some(h))
        .map(|(p, _)| p.clone())
        .chain(after.keys().filter(|p| !before.contains_key(*p)).cloned())
        .collect();
    paths.sort();
    paths.dedup();
    paths
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

    #[test]
    fn verdict_takes_the_last_json_block_and_counts_blockers() {
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
        let mut p = parse("```json\n{\"verdict\":\"pass\"}\n```");
        p.tampered();
        assert_eq!(p.verdict, "partial");
    }
}
